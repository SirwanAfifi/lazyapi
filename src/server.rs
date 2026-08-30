use std::{
    collections::BTreeMap,
    io::Read,
    net::ToSocketAddrs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::SyncSender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::{SecondsFormat, Utc};
use reqwest::blocking::Client;
use tiny_http::{Header, Request, Response, Server, StatusCode};
use url::{Url, form_urlencoded};

use crate::model::{Endpoint, ExchangePart, HeaderValue, LogEntry};

const MAX_CAPTURED_BODY_BYTES: usize = 2 << 20;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const RESPONSE_TRUNCATION_NOTICE: &[u8] = b"\n... response body truncated by LazyAPI ...";
const RESPONSE_INTERRUPTION_NOTICE: &[u8] =
    b"\n... response body capture interrupted before completion ...";
const REDACTED_VALUE: &str = "[REDACTED]";

#[derive(Clone)]
struct ServerConfig {
    listen_addr: String,
    target: Option<Url>,
    endpoints: Arc<Vec<Endpoint>>,
    redact_sensitive: bool,
}

pub struct CaptureServer {
    config: ServerConfig,
    output_tx: SyncSender<String>,
    logs_tx: SyncSender<LogEntry>,
    shutdown: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
    replay_workers: Mutex<Vec<JoinHandle<()>>>,
    active_addr: Option<String>,
}

impl CaptureServer {
    pub fn new(
        listen_addr: String,
        target: Option<String>,
        output_tx: SyncSender<String>,
        logs_tx: SyncSender<LogEntry>,
    ) -> Result<Self, String> {
        let listen_addr = if listen_addr.trim().is_empty() {
            "127.0.0.1:3000".to_string()
        } else {
            listen_addr
        };
        listen_addr
            .to_socket_addrs()
            .map_err(|error| format!("invalid listen address: {error}"))?
            .next()
            .ok_or_else(|| "invalid listen address: no address resolved".to_string())?;

        let target = target
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                let parsed = Url::parse(&value)
                    .map_err(|error| format!("could not parse target URL: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err("target must be an absolute http or https URL".to_string());
                }
                Ok(parsed)
            })
            .transpose()?;

        Ok(Self {
            config: ServerConfig {
                listen_addr,
                target,
                endpoints: Arc::new(Vec::new()),
                redact_sensitive: true,
            },
            output_tx,
            logs_tx,
            shutdown: None,
            worker: None,
            replay_workers: Mutex::new(Vec::new()),
            active_addr: None,
        })
    }

    /// Configures the OpenAPI operations used by local mock mode and contract checks.
    pub fn with_endpoints(mut self, endpoints: Vec<Endpoint>) -> Self {
        self.config.endpoints = Arc::new(endpoints);
        self
    }

    /// Enables or disables masking secrets in captured exchanges. Masking is on by default.
    pub fn with_redaction(mut self, enabled: bool) -> Self {
        self.config.redact_sensitive = enabled;
        self
    }

    pub fn start(&mut self) -> Result<String, String> {
        if self.worker.is_some() {
            return Ok(self.active_addr.clone().unwrap_or_default());
        }

        let server = Server::http(&self.config.listen_addr)
            .map_err(|error| format!("could not listen on {}: {error}", self.config.listen_addr))?;
        let active_addr = server.server_addr().to_string();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let config = self.config.clone();
        let output_tx = self.output_tx.clone();
        let logs_tx = self.logs_tx.clone();

        emit_output(
            &output_tx,
            format!("Capture server listening on http://{active_addr}"),
        );
        if let Some(target) = &config.target {
            emit_output(
                &output_tx,
                format!("Proxying requests to {}", sanitized_url_for_display(target)),
            );
        } else if !config.endpoints.is_empty() {
            emit_output(
                &output_tx,
                format!(
                    "Serving OpenAPI mock responses for {} operations",
                    config.endpoints.len()
                ),
            );
        } else {
            emit_output(
                &output_tx,
                "No proxy target configured; returning local capture responses".into(),
            );
        }

        self.worker = Some(thread::spawn(move || {
            run_server(server, config, worker_shutdown, output_tx, logs_tx);
        }));
        self.shutdown = Some(shutdown);
        self.active_addr = Some(active_addr.clone());
        Ok(active_addr)
    }

    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.store(true, Ordering::Release);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
            emit_output(&self.output_tx, "Capture server stopped".into());
        }
        self.active_addr = None;
        let mut replay_workers = self
            .replay_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_workers_bounded(&mut replay_workers, &self.output_tx, "replay");
    }

    pub fn restart(&mut self) -> Result<String, String> {
        self.stop();
        self.start()
    }

    /// Replays a captured request through this server so the replay is captured like new traffic.
    pub fn replay(&self, entry: &LogEntry) -> Result<(), String> {
        validate_replay_entry(entry)?;
        let active_addr = self
            .active_addr
            .as_deref()
            .ok_or_else(|| "capture server is not running".to_string())?;
        let path = if entry.path.starts_with('/') {
            entry.path.clone()
        } else {
            format!("/{}", entry.path)
        };
        let mut replay_url = format!("http://{active_addr}{path}");
        if let Some(query) = entry.query.as_deref().filter(|query| !query.is_empty()) {
            replay_url.push('?');
            replay_url.push_str(query);
        }
        let replay_url = Url::parse(&replay_url)
            .map_err(|error| format!("could not create replay URL: {error}"))?;
        let method = reqwest::Method::from_bytes(entry.method.as_bytes())
            .map_err(|error| format!("could not replay HTTP method: {error}"))?;
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| format!("could not create replay client: {error}"))?;
        let mut replay = client
            .request(method, replay_url)
            .body(entry.request.body.clone());
        for (name, value) in entry.request.iter_headers() {
            if !is_hop_by_hop(name)
                && !name.eq_ignore_ascii_case("content-length")
                && !name.eq_ignore_ascii_case("host")
            {
                replay = replay.header(name, value);
            }
        }
        let replay = replay
            .build()
            .map_err(|error| format!("could not build replay request: {error}"))?;
        let output_tx = self.output_tx.clone();
        let mut replay_workers = self
            .replay_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reap_finished_workers(&mut replay_workers, &output_tx, "replay");
        replay_workers.push(thread::spawn(move || {
            let result = client
                .execute(replay)
                .map_err(|error| format!("replay request failed: {error}"))
                .and_then(|mut response| {
                    std::io::copy(&mut response, &mut std::io::sink())
                        .map(|_| ())
                        .map_err(|error| format!("could not finish replay response: {error}"))
                });
            if let Err(error) = result {
                emit_output(&output_tx, format!("Replay failed: {error}"));
            }
        }));
        Ok(())
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_replay_entry(entry: &LogEntry) -> Result<(), String> {
    if entry.request.truncated {
        return Err("cannot replay a truncated request capture".into());
    }
    if entry.request.body.contains('\u{fffd}') {
        return Err(
            "cannot replay a request whose body contains lossy UTF-8 replacement characters".into(),
        );
    }
    let contains_redaction = entry.query.as_deref().is_some_and(contains_redacted_marker)
        || entry
            .request
            .iter_headers()
            .any(|(_, value)| contains_redacted_marker(value))
        || contains_redacted_marker(&entry.request.body);
    if contains_redaction {
        return Err("cannot replay a redacted request capture; recapture with --no-redact".into());
    }
    Ok(())
}

fn contains_redacted_marker(value: &str) -> bool {
    if value.to_ascii_lowercase().contains("[redacted]") {
        return true;
    }
    form_urlencoded::parse(value.as_bytes()).any(|(key, value)| {
        key.to_ascii_lowercase().contains("[redacted]")
            || value.to_ascii_lowercase().contains("[redacted]")
    })
}

fn run_server(
    server: Server,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    output_tx: SyncSender<String>,
    logs_tx: SyncSender<LogEntry>,
) {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build();
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            emit_output(
                &output_tx,
                format!("Could not create proxy client: {error}"),
            );
            return;
        }
    };

    let mut request_workers = Vec::new();
    while !shutdown.load(Ordering::Acquire) {
        reap_finished_workers(&mut request_workers, &output_tx, "request");
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) => {
                if request_workers.len() >= MAX_CONCURRENT_REQUESTS {
                    let response = Response::from_string("capture server is busy")
                        .with_status_code(StatusCode(503));
                    if let Err(error) = request.respond(response) {
                        emit_output(
                            &output_tx,
                            format!("Could not reject excess request: {error}"),
                        );
                    } else {
                        emit_output(
                            &output_tx,
                            format!(
                                "Capture server busy; rejected request after {} concurrent requests",
                                MAX_CONCURRENT_REQUESTS
                            ),
                        );
                    }
                    continue;
                }
                let config = config.clone();
                let client = client.clone();
                let output_tx = output_tx.clone();
                let logs_tx = logs_tx.clone();
                request_workers.push(thread::spawn(move || {
                    handle_request(request, &config, &client, &output_tx, &logs_tx);
                }));
            }
            Ok(None) => {}
            Err(error) => {
                emit_output(
                    &output_tx,
                    format!("Capture server stopped with error: {error}"),
                );
                break;
            }
        }
    }

    finish_workers_bounded(&mut request_workers, &output_tx, "request");
}

fn reap_finished_workers(
    workers: &mut Vec<JoinHandle<()>>,
    output_tx: &SyncSender<String>,
    kind: &str,
) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            if worker.join().is_err() {
                emit_output(output_tx, format!("A {kind} worker stopped unexpectedly"));
            }
        } else {
            index += 1;
        }
    }
}

fn finish_workers_bounded(
    workers: &mut Vec<JoinHandle<()>>,
    output_tx: &SyncSender<String>,
    kind: &str,
) {
    let deadline = Instant::now() + WORKER_SHUTDOWN_GRACE;
    while !workers.is_empty() && Instant::now() < deadline {
        reap_finished_workers(workers, output_tx, kind);
        if !workers.is_empty() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    reap_finished_workers(workers, output_tx, kind);
    if !workers.is_empty() {
        let stalled = workers.len();
        workers.clear();
        emit_output(
            output_tx,
            format!("Detached {stalled} stalled {kind} worker(s) during shutdown"),
        );
    }
}

fn handle_request(
    mut request: Request,
    config: &ServerConfig,
    client: &Client,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let started_at = Instant::now();
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let method = request.method().as_str().to_string();
    let request_url = request.url().to_string();
    let (path, query) = split_request_url(&request_url);
    let query = query.map(str::to_string);
    let request_header_values = capture_request_headers(&request);
    let mut request_body = Vec::new();
    let read_result = request
        .as_reader()
        .take((MAX_CAPTURED_BODY_BYTES + 1) as u64)
        .read_to_end(&mut request_body);

    if let Err(error) = read_result {
        finish_with_error(
            request,
            RequestContext::new(
                method,
                path,
                query,
                request_header_values,
                request_body,
                started_at,
                timestamp,
            ),
            400,
            format!("could not read request body: {error}"),
            config,
            output_tx,
            logs_tx,
        );
        return;
    }
    if request_body.len() > MAX_CAPTURED_BODY_BYTES {
        let body_size = request_body.len();
        request_body.truncate(MAX_CAPTURED_BODY_BYTES);
        let mut context = RequestContext::new(
            method,
            path,
            query,
            request_header_values,
            request_body,
            started_at,
            timestamp,
        );
        context.body_size = body_size;
        context.body_truncated = true;
        finish_with_error(
            request,
            context,
            413,
            "request body exceeds 2 MiB capture limit".into(),
            config,
            output_tx,
            logs_tx,
        );
        return;
    }

    let context = RequestContext::new(
        method,
        path.clone(),
        query,
        request_header_values,
        request_body,
        started_at,
        timestamp,
    );
    if let Some(target) = &config.target {
        proxy_request(request, context, target, client, config, output_tx, logs_tx);
    } else {
        mock_request(request, context, config, output_tx, logs_tx);
    }
}

fn mock_request(
    request: Request,
    context: RequestContext,
    config: &ServerConfig,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    if config.endpoints.is_empty() {
        let body = serde_json::to_vec(&serde_json::json!({
            "captured": true,
            "method": context.method,
            "path": context.path,
        }))
        .unwrap_or_else(|_| b"{\"error\":\"could not create capture response\"}".to_vec());
        finish_response(
            request,
            context,
            200,
            vec![("Content-Type".into(), "application/json".into())],
            body,
            config,
            EventChannels {
                output: output_tx,
                logs: logs_tx,
            },
        );
        return;
    }

    let Some(endpoint) = config
        .endpoints
        .iter()
        .find(|endpoint| endpoint.matches(&context.method, &context.path))
    else {
        let message = format!(
            "no OpenAPI operation matches {} {}",
            context.method, context.path
        );
        finish_with_error(request, context, 404, message, config, output_tx, logs_tx);
        return;
    };

    let Some(mock) = endpoint.mock_response() else {
        finish_with_error(
            request,
            context,
            501,
            "matched OpenAPI operation has no mock response".into(),
            config,
            output_tx,
            logs_tx,
        );
        return;
    };
    let headers = mock
        .content_type
        .map(|content_type| vec![("Content-Type".into(), content_type)])
        .unwrap_or_default();
    finish_response(
        request,
        context,
        mock.status,
        headers,
        mock.body.into_bytes(),
        config,
        EventChannels {
            output: output_tx,
            logs: logs_tx,
        },
    );
}

struct RequestContext {
    method: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    header_values: Vec<HeaderValue>,
    body: Vec<u8>,
    body_size: usize,
    body_truncated: bool,
    started_at: Instant,
    timestamp: String,
}

impl RequestContext {
    fn new(
        method: String,
        path: String,
        query: Option<String>,
        header_values: Vec<HeaderValue>,
        body: Vec<u8>,
        started_at: Instant,
        timestamp: String,
    ) -> Self {
        let body_size = body.len();
        let headers = flatten_header_values(&header_values);
        Self {
            method,
            path,
            query,
            headers,
            header_values,
            body,
            body_size,
            body_truncated: false,
            started_at,
            timestamp,
        }
    }
}

fn proxy_request(
    request: Request,
    context: RequestContext,
    target: &Url,
    client: &Client,
    config: &ServerConfig,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let target_url = match upstream_url(target, &context.path, context.query.as_deref()) {
        Ok(url) => url,
        Err(error) => {
            finish_with_error(request, context, 400, error, config, output_tx, logs_tx);
            return;
        }
    };

    let method = match reqwest::Method::from_bytes(context.method.as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            finish_with_error(
                request,
                context,
                502,
                format!("could not create upstream request: {error}"),
                config,
                output_tx,
                logs_tx,
            );
            return;
        }
    };

    let forwarded_host = context
        .header_values
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("host"))
        .map(|header| header.value.clone())
        .unwrap_or_default();
    let mut upstream = client
        .request(method, target_url)
        .body(context.body.clone());
    for header in &context.header_values {
        if !is_hop_by_hop(&header.name)
            && !header.name.eq_ignore_ascii_case("host")
            && !is_forwarding_metadata(&header.name)
        {
            upstream = upstream.header(&header.name, &header.value);
        }
    }
    upstream = upstream
        .header("X-Forwarded-Host", forwarded_host)
        .header("X-Forwarded-Proto", "http");

    let response = match upstream.send() {
        Ok(response) => response,
        Err(error) => {
            emit_output(
                output_tx,
                format!("Upstream request failed: {}", error.without_url()),
            );
            finish_with_error(
                request,
                context,
                502,
                "upstream request failed".into(),
                config,
                output_tx,
                logs_tx,
            );
            return;
        }
    };

    let status = response.status().as_u16();
    let content_length = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok());
    let response_header_values: Vec<HeaderValue> = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()) && name.as_str() != "content-length")
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| HeaderValue::new(name.as_str(), value))
        })
        .collect();
    let flattened_headers = flatten_header_values(&response_header_values);
    let headers: Vec<_> = response_header_values
        .iter()
        .filter_map(|header| {
            Header::from_bytes(header.name.as_bytes(), header.value.as_bytes()).ok()
        })
        .collect();
    let captured = Arc::new(Mutex::new(CapturedBody::default()));
    let reader = CapturingReader::new(response, Arc::clone(&captured));
    let response = Response::new(StatusCode(status), headers, reader, content_length, None);
    let response_interrupted = match request.respond(response) {
        Ok(()) => false,
        Err(error) => {
            emit_output(output_tx, format!("Upstream response interrupted: {error}"));
            true
        }
    };
    let mut captured_response = captured
        .lock()
        .map(|body| body.snapshot())
        .unwrap_or_default();
    if response_interrupted {
        mark_response_interrupted(&mut captured_response, content_length);
    }
    emit_log_entry(
        context,
        status,
        CapturedResponse {
            headers: flattened_headers,
            header_values: response_header_values,
            ..captured_response
        },
        config,
        output_tx,
        logs_tx,
    );
}

fn mark_response_interrupted(response: &mut CapturedResponse, expected_size: Option<usize>) {
    if !response.truncated {
        response
            .body
            .extend_from_slice(RESPONSE_INTERRUPTION_NOTICE);
    }
    response.truncated = true;
    if let Some(expected_size) = expected_size {
        response.size = response.size.max(expected_size);
    }
}

fn upstream_url(
    target: &Url,
    request_path: &str,
    request_query: Option<&str>,
) -> Result<Url, String> {
    if path_has_dot_segment(request_path) {
        return Err("request path contains a forbidden dot segment".into());
    }

    let base_path = target.path().trim_end_matches('/');
    let joined_path = format!("{base_path}/{}", request_path.trim_start_matches('/'));
    let mut target_url = target.clone();
    target_url.set_path(&joined_path);

    // URL setters normalize encoded dot segments. Keep a configured path prefix from
    // ever being escaped even if a future URL parser accepts a representation that the
    // explicit check above does not recognize.
    if !base_path.is_empty() && base_path != "/" {
        let expected_prefix = format!("{base_path}/");
        if target_url.path() != base_path && !target_url.path().starts_with(&expected_prefix) {
            return Err("request path escapes the configured upstream base path".into());
        }
    }

    let joined_query = match (target.query(), request_query) {
        (Some(base), Some(request)) => Some(format!("{base}&{request}")),
        (Some(base), None) => Some(base.to_string()),
        (None, Some(request)) => Some(request.to_string()),
        (None, None) => None,
    };
    target_url.set_query(joined_query.as_deref());
    target_url.set_fragment(None);
    Ok(target_url)
}

fn path_has_dot_segment(path: &str) -> bool {
    let mut decoded = path.as_bytes().to_vec();
    for _ in 0..=4 {
        if decoded
            .split(|byte| matches!(byte, b'/' | b'\\'))
            .any(|segment| segment == b"." || segment == b"..")
        {
            return true;
        }

        let mut next = Vec::with_capacity(decoded.len());
        let mut index = 0;
        let mut changed = false;
        while index < decoded.len() {
            if decoded[index] == b'%'
                && index + 2 < decoded.len()
                && let (Some(high), Some(low)) =
                    (hex_value(decoded[index + 1]), hex_value(decoded[index + 2]))
            {
                next.push((high << 4) | low);
                index += 3;
                changed = true;
            } else {
                next.push(decoded[index]);
                index += 1;
            }
        }
        if !changed {
            return false;
        }
        decoded = next;
    }
    false
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn finish_with_error(
    request: Request,
    context: RequestContext,
    status: u16,
    message: String,
    config: &ServerConfig,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let body = serde_json::to_vec(&serde_json::json!({ "error": message }))
        .unwrap_or_else(|_| b"{\"error\":\"request failed\"}".to_vec());
    finish_response(
        request,
        context,
        status,
        vec![("Content-Type".into(), "application/json".into())],
        body,
        config,
        EventChannels {
            output: output_tx,
            logs: logs_tx,
        },
    );
}

#[derive(Clone, Copy)]
struct EventChannels<'a> {
    output: &'a SyncSender<String>,
    logs: &'a SyncSender<LogEntry>,
}

fn finish_response(
    request: Request,
    context: RequestContext,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    config: &ServerConfig,
    channels: EventChannels<'_>,
) {
    let response_header_values: Vec<_> = headers
        .iter()
        .map(|(name, value)| HeaderValue::new(name, value))
        .collect();
    let response_headers = flatten_header_values(&response_header_values);
    let response_size = body.len();
    let response_truncated = response_size > MAX_CAPTURED_BODY_BYTES;
    let captured_response = truncate_response(&body);
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    for (name, value) in headers {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response = response.with_header(header);
        }
    }
    if let Err(error) = request.respond(response) {
        emit_output(
            channels.output,
            format!("Could not write response: {error}"),
        );
    }

    emit_log_entry(
        context,
        status,
        CapturedResponse {
            headers: response_headers,
            header_values: response_header_values,
            body: captured_response,
            size: response_size,
            truncated: response_truncated,
        },
        config,
        channels.output,
        channels.logs,
    );
}

fn emit_log_entry(
    context: RequestContext,
    status: u16,
    captured_response: CapturedResponse,
    config: &ServerConfig,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let mut entry = LogEntry {
        method: context.method,
        path: context.path,
        query: context.query,
        status,
        timestamp: context.timestamp,
        request: ExchangePart {
            headers: context.headers,
            header_values: context.header_values,
            body: String::from_utf8_lossy(&context.body).into_owned(),
            size: context.body_size,
            truncated: context.body_truncated,
        },
        response: ExchangePart {
            headers: captured_response.headers,
            header_values: captured_response.header_values,
            body: String::from_utf8_lossy(&captured_response.body).into_owned(),
            size: captured_response.size,
            truncated: captured_response.truncated,
        },
        latency_ms: context.started_at.elapsed().as_millis(),
        ..LogEntry::default()
    };
    entry.validate_against(&config.endpoints);
    if config.redact_sensitive {
        redact_log_entry(&mut entry);
    }
    let summary = format!(
        "{} {} -> {} ({}ms)",
        entry.method, entry.path, entry.status, entry.latency_ms
    );
    if logs_tx.send(entry).is_err() {
        emit_output(
            output_tx,
            "Structured log receiver disconnected; capture could not be delivered".into(),
        );
    }
    emit_output(output_tx, summary);
}

#[derive(Default)]
struct CapturedResponse {
    headers: BTreeMap<String, String>,
    header_values: Vec<HeaderValue>,
    body: Vec<u8>,
    size: usize,
    truncated: bool,
}

fn split_request_url(url: &str) -> (String, Option<&str>) {
    match url.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query)),
        None => (url.to_string(), None),
    }
}

fn capture_request_headers(request: &Request) -> Vec<HeaderValue> {
    request
        .headers()
        .iter()
        .map(|header| HeaderValue::new(header.field.to_string(), header.value.to_string()))
        .collect()
}

fn flatten_header_values(headers: &[HeaderValue]) -> BTreeMap<String, String> {
    let mut flattened = BTreeMap::new();
    for header in headers {
        append_header(&mut flattened, header.name.clone(), header.value.clone());
    }
    flattened
}

fn append_header(headers: &mut BTreeMap<String, String>, name: String, value: String) {
    headers
        .entry(name)
        .and_modify(|current| {
            current.push_str(", ");
            current.push_str(&value);
        })
        .or_insert(value);
}

fn redact_log_entry(entry: &mut LogEntry) {
    if let Some(query) = entry.query.as_mut() {
        *query = redact_form_encoded(query);
    }
    redact_exchange_part(&mut entry.request);
    redact_exchange_part(&mut entry.response);
}

fn redact_exchange_part(part: &mut ExchangePart) {
    let content_type = part
        .header_value("content-type")
        .unwrap_or_default()
        .to_string();
    let encoded = part
        .header_value("content-encoding")
        .is_some_and(|encoding| !encoding.trim().eq_ignore_ascii_case("identity"));
    part.body = if encoded && !part.body.is_empty() {
        REDACTED_VALUE.into()
    } else {
        redact_body(&part.body, &content_type)
    };
    for header in &mut part.header_values {
        header.value = redact_header_value(&header.name, &header.value);
    }
    for (name, value) in &mut part.headers {
        *value = redact_header_value(name, value);
    }
}

fn redact_header_value(name: &str, value: &str) -> String {
    if is_sensitive_header(name) {
        REDACTED_VALUE.into()
    } else if is_url_header(name) {
        redact_url_value(value)
    } else {
        value.into()
    }
}

fn is_url_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "location" | "content-location" | "referer"
    )
}

fn redact_url_value(value: &str) -> String {
    let parsed = Url::parse(value).map(|url| (url, false)).or_else(|error| {
        if value.starts_with("//") {
            Url::parse(&format!("http:{value}")).map(|url| (url, true))
        } else {
            Err(error)
        }
    });
    if let Ok((mut url, scheme_relative)) = parsed {
        redact_url_components(&mut url);
        if let Some(fragment) = url.fragment()
            && contains_sensitive_key_hint(fragment)
        {
            url.set_fragment(Some(REDACTED_VALUE));
        }
        let redacted = url.to_string();
        return if scheme_relative {
            redacted
                .strip_prefix("http:")
                .unwrap_or(&redacted)
                .to_string()
        } else {
            redacted
        };
    }

    let (without_fragment, fragment) = value
        .split_once('#')
        .map_or((value, None), |(url, fragment)| (url, Some(fragment)));
    let (path, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, None), |(path, query)| {
            (path, Some(query))
        });
    let mut redacted = path.to_string();
    if let Some(query) = query {
        redacted.push('?');
        redacted.push_str(&redact_form_encoded(query));
    }
    if let Some(fragment) = fragment {
        redacted.push('#');
        if contains_sensitive_key_hint(fragment) {
            redacted.push_str(REDACTED_VALUE);
        } else {
            redacted.push_str(fragment);
        }
    }
    redacted
}

fn sanitized_url_for_display(url: &Url) -> String {
    let mut sanitized = url.clone();
    redact_url_components(&mut sanitized);
    sanitized.set_fragment(None);
    sanitized.into()
}

fn redact_url_components(url: &mut Url) {
    if !url.username().is_empty() {
        let _ = url.set_username(REDACTED_VALUE);
    }
    if url.password().is_some() {
        let _ = url.set_password(Some(REDACTED_VALUE));
    }
    if let Some(query) = url.query() {
        let query = redact_form_encoded(query);
        url.set_query(Some(&query));
    }
}

fn redact_body(body: &str, content_type: &str) -> String {
    if body.is_empty() {
        return String::new();
    }

    let normalized_content_type = content_type.to_ascii_lowercase();
    if normalized_content_type.contains("multipart/form-data") {
        return redact_multipart(body, content_type);
    }
    let trimmed = body.trim_start();
    let looks_like_json = normalized_content_type.contains("json")
        || trimmed.starts_with('{')
        || trimmed.starts_with('[');
    if looks_like_json {
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(mut value) => {
                if redact_json_value(&mut value) {
                    return serde_json::to_string(&value)
                        .unwrap_or_else(|_| REDACTED_VALUE.to_string());
                }
                return body.to_string();
            }
            Err(_) if contains_sensitive_key_hint(body) => return REDACTED_VALUE.to_string(),
            Err(_) => {}
        }
    }

    if normalized_content_type.contains("x-www-form-urlencoded") {
        return redact_form_encoded(body);
    }
    body.to_string()
}

fn redact_multipart(body: &str, content_type: &str) -> String {
    let contains_sensitive_field = multipart_contains_sensitive_field(body);
    let Some(boundary) = multipart_boundary(content_type) else {
        return if contains_sensitive_field {
            REDACTED_VALUE.into()
        } else {
            body.into()
        };
    };
    let delimiter = format!("--{boundary}");
    if !body.contains(&delimiter) {
        return if contains_sensitive_field {
            REDACTED_VALUE.into()
        } else {
            body.into()
        };
    }

    let mut output = String::with_capacity(body.len());
    let mut changed = false;
    for (index, part) in body.split(&delimiter).enumerate() {
        if index > 0 {
            output.push_str(&delimiter);
        }
        if multipart_field_name(part)
            .as_deref()
            .is_some_and(is_sensitive_key)
        {
            changed = true;
            output.push_str(&redact_multipart_part(part));
        } else {
            output.push_str(part);
        }
    }
    if changed {
        output
    } else if contains_sensitive_field {
        REDACTED_VALUE.into()
    } else {
        body.into()
    }
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

fn multipart_contains_sensitive_field(body: &str) -> bool {
    body.lines().any(|line| {
        multipart_name_from_disposition(line)
            .as_deref()
            .is_some_and(is_sensitive_key)
    })
}

fn multipart_field_name(part: &str) -> Option<String> {
    let headers = part
        .split_once("\r\n\r\n")
        .map(|(headers, _)| headers)
        .or_else(|| part.split_once("\n\n").map(|(headers, _)| headers))
        .unwrap_or(part);
    headers.lines().find_map(multipart_name_from_disposition)
}

fn multipart_name_from_disposition(line: &str) -> Option<String> {
    let (header, value) = line.trim().split_once(':')?;
    if !header.trim().eq_ignore_ascii_case("content-disposition") {
        return None;
    }
    value.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.trim().split_once('=')?;
        name.trim().eq_ignore_ascii_case("name").then(|| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
    })
}

fn redact_multipart_part(part: &str) -> String {
    let separator = if part.contains("\r\n\r\n") {
        "\r\n\r\n"
    } else if part.contains("\n\n") {
        "\n\n"
    } else {
        return REDACTED_VALUE.into();
    };
    let Some((headers, payload)) = part.split_once(separator) else {
        return REDACTED_VALUE.into();
    };
    let trailer = if payload.ends_with("\r\n") {
        "\r\n"
    } else if payload.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    format!("{headers}{separator}{REDACTED_VALUE}{trailer}")
}

fn redact_json_value(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            let mut changed = false;
            for (key, value) in object {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String(REDACTED_VALUE.into());
                    changed = true;
                } else {
                    changed |= redact_json_value(value);
                }
            }
            changed
        }
        serde_json::Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= redact_json_value(value);
            }
            changed
        }
        _ => false,
    }
}

fn redact_form_encoded(value: &str) -> String {
    let pairs: Vec<_> = form_urlencoded::parse(value.as_bytes())
        .into_owned()
        .collect();
    if !pairs.iter().any(|(key, _)| is_sensitive_key(key)) {
        return value.to_string();
    }

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(
            &key,
            if is_sensitive_key(&key) {
                REDACTED_VALUE
            } else {
                &value
            },
        );
    }
    serializer.finish()
}

fn is_sensitive_header(name: &str) -> bool {
    let normalized = normalize_sensitive_name(name);
    matches!(normalized.as_str(), "cookie" | "setcookie")
        || ["token", "secret", "password", "apikey", "auth"]
            .iter()
            .any(|needle| normalized.contains(needle))
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_sensitive_name(key);
    [
        "token",
        "secret",
        "password",
        "apikey",
        "credential",
        "auth",
        "authorization",
        "authentication",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn normalize_sensitive_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn contains_sensitive_key_hint(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api-key",
        "api_key",
        "apikey",
        "credential",
        "auth",
        "authorization",
        "authentication",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_forwarding_metadata(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-port"
            | "x-forwarded-proto"
            | "x-forwarded-protocol"
            | "x-forwarded-scheme"
            | "x-original-forwarded-for"
    )
}

fn truncate_response(body: &[u8]) -> Vec<u8> {
    if body.len() <= MAX_CAPTURED_BODY_BYTES {
        return body.to_vec();
    }
    let mut truncated = body[..MAX_CAPTURED_BODY_BYTES].to_vec();
    truncated.extend_from_slice(RESPONSE_TRUNCATION_NOTICE);
    truncated
}

#[derive(Default)]
struct CapturedBody {
    bytes: Vec<u8>,
    size: usize,
    truncated: bool,
}

impl CapturedBody {
    fn push(&mut self, data: &[u8]) {
        self.size += data.len();
        let remaining = MAX_CAPTURED_BODY_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&data[..data.len().min(remaining)]);
        self.truncated |= data.len() > remaining;
    }

    fn snapshot(&self) -> CapturedResponse {
        let mut bytes = self.bytes.clone();
        if self.truncated {
            bytes.extend_from_slice(RESPONSE_TRUNCATION_NOTICE);
        }
        CapturedResponse {
            body: bytes,
            size: self.size,
            truncated: self.truncated,
            ..CapturedResponse::default()
        }
    }
}

struct CapturingReader<R> {
    inner: R,
    captured: Arc<Mutex<CapturedBody>>,
}

impl<R> CapturingReader<R> {
    fn new(inner: R, captured: Arc<Mutex<CapturedBody>>) -> Self {
        Self { inner, captured }
    }
}

impl<R: Read> Read for CapturingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if let Ok(mut captured) = self.captured.lock() {
            captured.push(&buffer[..read]);
        }
        Ok(read)
    }
}

fn emit_output(sender: &SyncSender<String>, message: String) {
    let _ = sender.try_send(message);
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Write,
        net::TcpStream,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use serde_json::json;
    use tiny_http::{Header, Response, Server, StatusCode};

    use super::{
        CaptureServer, CapturedResponse, MAX_CAPTURED_BODY_BYTES, REDACTED_VALUE,
        mark_response_interrupted, redact_body, redact_exchange_part, redact_header_value,
        sanitized_url_for_display, upstream_url,
    };
    use crate::model::{Endpoint, ExchangePart, LogEntry, MediaTypeSpec, ResponseSpec};

    fn header_values<'a>(part: &'a ExchangePart, wanted: &str) -> Vec<&'a str> {
        part.iter_headers()
            .filter(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value)
            .collect()
    }

    fn test_server() -> (
        CaptureServer,
        String,
        mpsc::Receiver<crate::model::LogEntry>,
    ) {
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server =
            CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx).unwrap();
        let address = server.start().unwrap();
        (server, address, logs_rx)
    }

    #[test]
    fn captures_local_response() {
        let (mut server, address, logs) = test_server();
        let response = reqwest::blocking::Client::new()
            .post(format!("http://{address}/users/42?expand=team"))
            .header("Content-Type", "application/json")
            .body(r#"{"name":"Ada"}"#)
            .send()
            .unwrap();
        assert_eq!(response.status(), 200);

        let entry = logs.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(entry.method, "POST");
        assert_eq!(entry.path, "/users/42");
        assert_eq!(entry.query.as_deref(), Some("expand=team"));
        assert_eq!(entry.request.body, r#"{"name":"Ada"}"#);
        assert_eq!(entry.request.size, 14);
        assert!(!entry.request.truncated);
        assert!(entry.response.body.contains("\"captured\":true"));
        assert!(entry.response.size > 0);
        server.stop();
    }

    #[test]
    fn full_log_channel_applies_backpressure_without_losing_captures() {
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs) = mpsc::sync_channel(1);
        let mut server =
            CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx).unwrap();
        let address = server.start().unwrap();

        for path in ["one", "two"] {
            reqwest::blocking::get(format!("http://{address}/{path}"))
                .unwrap()
                .text()
                .unwrap();
        }

        assert_eq!(
            logs.recv_timeout(Duration::from_secs(2)).unwrap().path,
            "/one"
        );
        assert_eq!(
            logs.recv_timeout(Duration::from_secs(2)).unwrap().path,
            "/two"
        );
        server.stop();
    }

    #[test]
    fn serves_documented_mock_response_and_rejects_unmatched_operations() {
        let mut endpoint = Endpoint::new("GET", "/users/{id}");
        endpoint.responses.insert(
            "200".into(),
            ResponseSpec {
                content: BTreeMap::from([(
                    "application/json".into(),
                    MediaTypeSpec {
                        example: Some(json!({ "id": 42, "name": "Ada" })),
                        ..MediaTypeSpec::default()
                    },
                )]),
                ..ResponseSpec::default()
            },
        );
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx)
            .unwrap()
            .with_endpoints(vec![endpoint]);
        let address = server.start().unwrap();

        let response = reqwest::blocking::get(format!("http://{address}/users/42")).unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["Content-Type"], "application/json");
        assert!(response.text().unwrap().contains("\"name\": \"Ada\""));
        let matched = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matched.contract.is_valid());

        let response = reqwest::blocking::get(format!("http://{address}/teams/42")).unwrap();
        assert_eq!(response.status(), 404);
        assert!(
            response
                .text()
                .unwrap()
                .contains("no OpenAPI operation matches GET /teams/42")
        );
        let unmatched = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            unmatched.contract.violations[0].code,
            "undocumented_operation"
        );
        server.stop();
    }

    #[test]
    fn rejects_oversized_request() {
        let (mut server, address, logs) = test_server();
        let response = reqwest::blocking::Client::new()
            .post(format!("http://{address}/upload"))
            .body(vec![b'x'; MAX_CAPTURED_BODY_BYTES + 1])
            .send()
            .unwrap();
        assert_eq!(response.status(), 413);
        let entry = logs.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(entry.status, 413);
        assert_eq!(entry.request.size, MAX_CAPTURED_BODY_BYTES + 1);
        assert!(entry.request.truncated);
        server.stop();
    }

    #[test]
    fn redacts_sensitive_capture_data_without_changing_forwarded_traffic() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_address = upstream.server_addr().to_string();
        let (observed_tx, observed_rx) = mpsc::channel();
        let upstream_worker = thread::spawn(move || {
            let mut request = upstream
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .expect("proxy did not contact upstream");
            let url = request.url().to_string();
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_default();
            let auth_token = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("X-Auth-Token"))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            observed_tx
                .send((url, authorization, auth_token, body))
                .unwrap();

            let response =
                Response::from_string(r#"{"accessToken":"response-secret","name":"Ada"}"#)
                    .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
                    .with_header(
                        Header::from_bytes("Set-Cookie", "session=response-cookie").unwrap(),
                    );
            request.respond(response).unwrap();
        });

        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new(
            "127.0.0.1:0".into(),
            Some(format!("http://{upstream_address}")),
            output_tx,
            logs_tx,
        )
        .unwrap();
        let address = server.start().unwrap();
        let raw_request_body = r#"{"profile":{"password":"request-secret"},"name":"Ada"}"#;
        let response = reqwest::blocking::Client::new()
            .post(format!(
                "http://{address}/users?access_token=query-secret&visible=yes"
            ))
            .header("Authorization", "Bearer request-token")
            .header("Cookie", "session=request-cookie")
            .header("X-API-Key", "request-api-key")
            .header("Api-Key", "request-generic-api-key")
            .header("X-Auth-Token", "request-auth-token")
            .header("X-Service-Secret", "request-header-secret")
            .header("Content-Type", "application/json")
            .body(raw_request_body)
            .send()
            .unwrap();
        assert!(response.text().unwrap().contains("response-secret"));

        let (url, authorization, auth_token, forwarded_body) =
            observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(url.contains("access_token=query-secret"));
        assert_eq!(authorization, "Bearer request-token");
        assert_eq!(auth_token, "request-auth-token");
        assert_eq!(forwarded_body, raw_request_body);

        let entry = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let captured = serde_json::to_string(&entry).unwrap();
        assert!(!captured.contains("request-token"));
        assert!(!captured.contains("request-cookie"));
        assert!(!captured.contains("request-api-key"));
        assert!(!captured.contains("request-generic-api-key"));
        assert!(!captured.contains("request-auth-token"));
        assert!(!captured.contains("request-header-secret"));
        assert!(!captured.contains("query-secret"));
        assert!(!captured.contains("request-secret"));
        assert!(!captured.contains("response-cookie"));
        assert!(!captured.contains("response-secret"));
        assert!(captured.contains("visible=yes"));
        assert!(captured.contains("Ada"));
        server.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn can_disable_capture_redaction() {
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx)
            .unwrap()
            .with_redaction(false);
        let address = server.start().unwrap();
        reqwest::blocking::Client::new()
            .post(format!("http://{address}/login?token=query-secret"))
            .header("Authorization", "Bearer request-token")
            .header("Content-Type", "application/json")
            .body(r#"{"password":"request-secret"}"#)
            .send()
            .unwrap();

        let entry = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let captured = serde_json::to_string(&entry).unwrap();
        assert!(captured.contains("request-token"));
        assert!(captured.contains("query-secret"));
        assert!(captured.contains("request-secret"));
        server.stop();
    }

    #[test]
    fn redacts_sensitive_multipart_fields_conservatively() {
        let body = "--lazy-boundary\r\n\
                    Content-Disposition: form-data; name=\"username\"\r\n\r\n\
                    Ada\r\n\
                    --lazy-boundary\r\n\
                    Content-Disposition: form-data; name=\"password\"\r\n\r\n\
                    request-password\r\n\
                    --lazy-boundary\r\n\
                    Content-Disposition: form-data; name=\"auth_token\"\r\n\r\n\
                    request-auth-token\r\n\
                    --lazy-boundary--\r\n";
        let redacted = redact_body(body, "multipart/form-data; boundary=\"lazy-boundary\"");
        assert!(redacted.contains("Ada"));
        assert!(!redacted.contains("request-password"));
        assert!(!redacted.contains("request-auth-token"));
        assert_eq!(redacted.matches(REDACTED_VALUE).count(), 2);

        assert_eq!(redact_body(body, "multipart/form-data"), REDACTED_VALUE);
    }

    #[test]
    fn redacts_encoded_capture_bodies_that_cannot_be_safely_inspected() {
        let mut part = ExchangePart {
            headers: BTreeMap::from([
                ("Content-Type".into(), "application/json".into()),
                ("Content-Encoding".into(), "gzip".into()),
            ]),
            body: "opaque encoded bytes".into(),
            size: 20,
            ..ExchangePart::default()
        };
        redact_exchange_part(&mut part);
        assert_eq!(part.body, REDACTED_VALUE);
        assert_eq!(part.size, 20);
    }

    #[test]
    fn interrupted_upstream_bodies_are_marked_as_incomplete() {
        let mut response = CapturedResponse {
            body: b"partial".to_vec(),
            size: 7,
            ..CapturedResponse::default()
        };
        mark_response_interrupted(&mut response, Some(128));
        assert!(response.truncated);
        assert_eq!(response.size, 128);
        assert!(
            String::from_utf8(response.body)
                .unwrap()
                .contains("interrupted before completion")
        );
    }

    #[test]
    fn redacts_secrets_embedded_in_url_headers_and_target_output() {
        assert_eq!(
            redact_header_value(
                "Location",
                "/callback?code=visible&access_token=secret#token=fragment-secret"
            ),
            "/callback?code=visible&access_token=%5BREDACTED%5D#[REDACTED]"
        );
        assert_eq!(
            redact_header_value(
                "Referer",
                "https://user:password@example.test/path?api_key=secret&view=full"
            ),
            "https://%5BREDACTED%5D:%5BREDACTED%5D@example.test/path?api_key=%5BREDACTED%5D&view=full"
        );
        assert_eq!(
            redact_header_value("Location", "//user:password@example.test/path?token=secret"),
            "//%5BREDACTED%5D:%5BREDACTED%5D@example.test/path?token=%5BREDACTED%5D"
        );

        let target = url::Url::parse(
            "https://user:password@example.test/api?token=secret&source=test#private",
        )
        .unwrap();
        let displayed = sanitized_url_for_display(&target);
        assert!(!displayed.contains("user"));
        assert!(!displayed.contains("password"));
        assert!(!displayed.contains("secret"));
        assert!(!displayed.contains("private"));
        assert!(displayed.contains("source=test"));
    }

    #[test]
    fn rejects_dot_segments_before_joining_an_upstream_base_path() {
        let target = url::Url::parse("http://example.test/api").unwrap();
        for path in [
            "/../admin",
            "/%2e%2e/admin",
            "/%252e%252e/admin",
            "/%2e%2e%2fadmin",
            "/.%2E/admin",
            "/safe/%2e/hidden",
            "/safe%5c..%5cadmin",
        ] {
            assert!(
                upstream_url(&target, path, None).is_err(),
                "accepted {path}"
            );
        }
        assert_eq!(
            upstream_url(&target, "/widgets/7", Some("view=full"))
                .unwrap()
                .as_str(),
            "http://example.test/api/widgets/7?view=full"
        );
    }

    #[test]
    fn replay_rejects_incomplete_lossy_and_redacted_captures() {
        let (output_tx, _output_rx) = mpsc::sync_channel(8);
        let (logs_tx, _logs_rx) = mpsc::sync_channel(8);
        let server = CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx).unwrap();
        let base = LogEntry {
            method: "POST".into(),
            path: "/widgets".into(),
            ..LogEntry::default()
        };

        let mut truncated = base.clone();
        truncated.request.truncated = true;
        assert!(server.replay(&truncated).unwrap_err().contains("truncated"));

        let mut lossy = base.clone();
        lossy.request.body = "invalid \u{fffd} body".into();
        assert!(server.replay(&lossy).unwrap_err().contains("lossy UTF-8"));

        let mut redacted = base;
        redacted.query = Some("token=%5BREDACTED%5D".into());
        assert!(server.replay(&redacted).unwrap_err().contains("redacted"));
    }

    #[test]
    fn replays_a_captured_request_through_the_listener() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_address = upstream.server_addr().to_string();
        let upstream_worker = thread::spawn(move || {
            for index in 0..2 {
                let request = upstream
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("proxy did not contact upstream");
                if index == 1 {
                    thread::sleep(Duration::from_millis(400));
                }
                request.respond(Response::from_string("ok")).unwrap();
            }
        });
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new(
            "127.0.0.1:0".into(),
            Some(format!("http://{upstream_address}")),
            output_tx,
            logs_tx,
        )
        .unwrap();
        let address = server.start().unwrap();
        reqwest::blocking::Client::new()
            .patch(format!("http://{address}/widgets/7?view=full"))
            .header("Content-Type", "application/json")
            .header("X-Replay", "one")
            .header("X-Replay", "two")
            .body(r#"{"enabled":true}"#)
            .send()
            .unwrap()
            .text()
            .unwrap();
        let original = logs.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(header_values(&original.request, "x-replay"), ["one", "two"]);

        let replay_started = Instant::now();
        server.replay(&original).unwrap();
        assert!(replay_started.elapsed() < Duration::from_millis(200));
        let replayed = logs.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(replayed.method, original.method);
        assert_eq!(replayed.path, original.path);
        assert_eq!(replayed.query, original.query);
        assert_eq!(replayed.request.body, original.request.body);
        assert_eq!(header_values(&replayed.request, "x-replay"), ["one", "two"]);
        assert!(replayed.latency_ms >= 350);
        let request_started = chrono::DateTime::parse_from_rfc3339(&replayed.timestamp)
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert!(
            chrono::Utc::now()
                .signed_duration_since(request_started)
                .num_milliseconds()
                >= 350
        );
        server.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn proxy_does_not_follow_upstream_redirects() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_address = upstream.server_addr().to_string();
        let (followed_tx, followed_rx) = mpsc::channel();
        let upstream_worker = thread::spawn(move || {
            let request = upstream
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .expect("proxy did not contact upstream");
            request
                .respond(
                    Response::from_string("redirect")
                        .with_status_code(StatusCode(302))
                        .with_header(Header::from_bytes("Location", "/final").unwrap()),
                )
                .unwrap();
            let followed = upstream
                .recv_timeout(Duration::from_millis(300))
                .unwrap()
                .is_some();
            followed_tx.send(followed).unwrap();
        });
        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new(
            "127.0.0.1:0".into(),
            Some(format!("http://{upstream_address}")),
            output_tx,
            logs_tx,
        )
        .unwrap();
        let address = server.start().unwrap();
        let response = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://{address}/redirect"))
            .send()
            .unwrap();
        assert_eq!(response.status(), 302);
        assert_eq!(
            logs_rx.recv_timeout(Duration::from_secs(2)).unwrap().status,
            302
        );
        assert!(!followed_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        server.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn stop_detaches_a_worker_blocked_on_an_incomplete_request_body() {
        let (output_tx, output_rx) = mpsc::sync_channel(64);
        let (logs_tx, _logs_rx) = mpsc::sync_channel(8);
        let mut server =
            CaptureServer::new("127.0.0.1:0".into(), None, output_tx, logs_tx).unwrap();
        let address = server.start().unwrap();
        let mut connection = TcpStream::connect(&address).unwrap();
        write!(
            connection,
            "POST /slow HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2048\r\n\r\nx"
        )
        .unwrap();
        connection.flush().unwrap();
        thread::sleep(Duration::from_millis(150));

        let stopping = Instant::now();
        server.stop();
        assert!(stopping.elapsed() < Duration::from_secs(2));
        let output: Vec<_> = output_rx.try_iter().collect();
        assert!(
            output
                .iter()
                .any(|line| line.contains("Detached 1 stalled request worker"))
        );
        drop(connection);
    }

    #[test]
    fn proxies_and_captures_upstream_response() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_address = upstream.server_addr().to_string();
        let (observed_tx, observed_rx) = mpsc::channel();
        let upstream_worker = thread::spawn(move || {
            let mut request = upstream
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .expect("proxy did not contact upstream");
            let url = request.url().to_string();
            let forwarded_host = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("X-Forwarded-Host"))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_default();
            let forwarded = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Forwarded"))
                .map(|header| header.value.as_str().to_string());
            let forwarded_proto = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("X-Forwarded-Proto"))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            observed_tx
                .send((url, forwarded_host, forwarded, forwarded_proto, body))
                .unwrap();

            let response = Response::from_string("created")
                .with_status_code(StatusCode(201))
                .with_header(Header::from_bytes("X-Upstream", "yes").unwrap());
            request.respond(response).unwrap();
        });

        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new(
            "127.0.0.1:0".into(),
            Some(format!("http://{upstream_address}/api?source=test")),
            output_tx,
            logs_tx,
        )
        .unwrap();
        let address = server.start().unwrap();

        let response = reqwest::blocking::Client::new()
            .put(format!("http://{address}/widgets/7?expand=owner"))
            .header("Forwarded", "for=attacker.example")
            .header("X-Forwarded-Host", "attacker.example")
            .header("X-Forwarded-Proto", "https")
            .body("payload")
            .send()
            .unwrap();
        assert_eq!(response.status(), 201);
        assert_eq!(response.headers()["X-Upstream"], "yes");
        assert_eq!(response.text().unwrap(), "created");

        let (url, forwarded_host, forwarded, forwarded_proto, body) =
            observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(url, "/api/widgets/7?source=test&expand=owner");
        assert!(!forwarded_host.is_empty());
        assert_ne!(forwarded_host, "attacker.example");
        assert!(forwarded.is_none());
        assert_eq!(forwarded_proto, "http");
        assert_eq!(body, "payload");

        let entry = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(entry.status, 201);
        assert_eq!(entry.query.as_deref(), Some("expand=owner"));
        assert_eq!(entry.response.body, "created");
        assert_eq!(entry.response.size, 7);
        assert!(!entry.response.truncated);
        server.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn preserves_repeated_headers_through_proxy_and_capture() {
        let upstream = Server::http("127.0.0.1:0").unwrap();
        let upstream_address = upstream.server_addr().to_string();
        let (observed_tx, observed_rx) = mpsc::channel();
        let upstream_worker = thread::spawn(move || {
            let request = upstream
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .expect("proxy did not contact upstream");
            let repeated: Vec<_> = request
                .headers()
                .iter()
                .filter(|header| header.field.equiv("X-Repeat"))
                .map(|header| header.value.to_string())
                .collect();
            observed_tx.send(repeated).unwrap();
            request
                .respond(
                    Response::from_string("ok")
                        .with_header(Header::from_bytes("Set-Cookie", "theme=dark").unwrap())
                        .with_header(Header::from_bytes("Set-Cookie", "session=abc").unwrap()),
                )
                .unwrap();
        });

        let (output_tx, _output_rx) = mpsc::sync_channel(32);
        let (logs_tx, logs_rx) = mpsc::sync_channel(32);
        let mut server = CaptureServer::new(
            "127.0.0.1:0".into(),
            Some(format!("http://{upstream_address}")),
            output_tx,
            logs_tx,
        )
        .unwrap()
        .with_redaction(false);
        let address = server.start().unwrap();

        let response = reqwest::blocking::Client::new()
            .get(format!("http://{address}/headers"))
            .header("X-Repeat", "one")
            .header("X-Repeat", "two")
            .send()
            .unwrap();
        let cookies: Vec<_> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(cookies, ["theme=dark", "session=abc"]);
        assert_eq!(response.text().unwrap(), "ok");
        assert_eq!(
            observed_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            ["one", "two"]
        );

        let entry = logs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(header_values(&entry.request, "x-repeat"), ["one", "two"]);
        assert_eq!(
            header_values(&entry.response, "set-cookie"),
            ["theme=dark", "session=abc"]
        );
        assert_eq!(
            entry
                .response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value.as_str()),
            Some("theme=dark, session=abc")
        );

        server.stop();
        upstream_worker.join().unwrap();
    }

    #[test]
    fn rejects_invalid_target_and_listen_address() {
        let (output_tx, _) = mpsc::sync_channel(1);
        let (logs_tx, _) = mpsc::sync_channel(1);
        assert!(
            CaptureServer::new(
                "127.0.0.1:0".into(),
                Some("localhost:8080".into()),
                output_tx.clone(),
                logs_tx.clone()
            )
            .is_err()
        );
        assert!(CaptureServer::new("missing-port".into(), None, output_tx, logs_tx).is_err());
    }
}
