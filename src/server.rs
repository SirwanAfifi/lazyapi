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
use url::Url;

use crate::model::{ExchangePart, LogEntry};

const MAX_CAPTURED_BODY_BYTES: usize = 2 << 20;
const RESPONSE_TRUNCATION_NOTICE: &[u8] = b"\n... response body truncated by LazyAPI ...";

#[derive(Clone)]
struct ServerConfig {
    listen_addr: String,
    target: Option<Url>,
}

pub struct CaptureServer {
    config: ServerConfig,
    output_tx: SyncSender<String>,
    logs_tx: SyncSender<LogEntry>,
    shutdown: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
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
            },
            output_tx,
            logs_tx,
            shutdown: None,
            worker: None,
            active_addr: None,
        })
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
            emit_output(&output_tx, format!("Proxying requests to {target}"));
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
    }

    pub fn restart(&mut self) -> Result<String, String> {
        self.stop();
        self.start()
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_server(
    server: Server,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
    output_tx: SyncSender<String>,
    logs_tx: SyncSender<LogEntry>,
) {
    let client = Client::builder().timeout(Duration::from_secs(30)).build();
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

    while !shutdown.load(Ordering::Acquire) {
        match server.recv_timeout(Duration::from_millis(100)) {
            Ok(Some(request)) => handle_request(request, &config, &client, &output_tx, &logs_tx),
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
}

fn handle_request(
    mut request: Request,
    config: &ServerConfig,
    client: &Client,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let started_at = Instant::now();
    let method = request.method().as_str().to_string();
    let request_url = request.url().to_string();
    let (path, query) = split_request_url(&request_url);
    let query = query.map(str::to_string);
    let request_headers = flatten_request_headers(&request);
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
                request_headers,
                request_body,
                started_at,
            ),
            400,
            format!("could not read request body: {error}"),
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
            request_headers,
            request_body,
            started_at,
        );
        context.body_size = body_size;
        context.body_truncated = true;
        finish_with_error(
            request,
            context,
            413,
            "request body exceeds 2 MiB capture limit".into(),
            output_tx,
            logs_tx,
        );
        return;
    }

    let context = RequestContext::new(
        method,
        path.clone(),
        query,
        request_headers,
        request_body,
        started_at,
    );
    if let Some(target) = &config.target {
        proxy_request(request, context, target, client, output_tx, logs_tx);
    } else {
        let body = serde_json::to_vec(&serde_json::json!({
            "captured": true,
            "method": context.method,
            "path": path,
        }))
        .unwrap_or_else(|_| b"{\"error\":\"could not create capture response\"}".to_vec());
        finish_response(
            request,
            context,
            200,
            vec![("Content-Type".into(), "application/json".into())],
            body,
            output_tx,
            logs_tx,
        );
    }
}

struct RequestContext {
    method: String,
    path: String,
    query: Option<String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    body_size: usize,
    body_truncated: bool,
    started_at: Instant,
}

impl RequestContext {
    fn new(
        method: String,
        path: String,
        query: Option<String>,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
        started_at: Instant,
    ) -> Self {
        let body_size = body.len();
        Self {
            method,
            path,
            query,
            headers,
            body,
            body_size,
            body_truncated: false,
            started_at,
        }
    }
}

fn proxy_request(
    request: Request,
    context: RequestContext,
    target: &Url,
    client: &Client,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let mut target_url = target.clone();
    let joined_path = format!(
        "{}/{}",
        target.path().trim_end_matches('/'),
        context.path.trim_start_matches('/')
    );
    target_url.set_path(&joined_path);
    let joined_query = match (target.query(), context.query.as_deref()) {
        (Some(base), Some(request)) => Some(format!("{base}&{request}")),
        (Some(base), None) => Some(base.to_string()),
        (None, Some(request)) => Some(request.to_string()),
        (None, None) => None,
    };
    target_url.set_query(joined_query.as_deref());

    let method = match reqwest::Method::from_bytes(context.method.as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            finish_with_error(
                request,
                context,
                502,
                format!("could not create upstream request: {error}"),
                output_tx,
                logs_tx,
            );
            return;
        }
    };

    let forwarded_host = find_header(&context.headers, "host").unwrap_or_default();
    let mut upstream = client
        .request(method, target_url)
        .body(context.body.clone());
    for (name, value) in &context.headers {
        if !is_hop_by_hop(name) && !name.eq_ignore_ascii_case("host") {
            upstream = upstream.header(name, value);
        }
    }
    upstream = upstream
        .header("X-Forwarded-Host", forwarded_host)
        .header("X-Forwarded-Proto", "http");

    let response = match upstream.send() {
        Ok(response) => response,
        Err(error) => {
            emit_output(output_tx, format!("Upstream request failed: {error}"));
            finish_with_error(
                request,
                context,
                502,
                "upstream request failed".into(),
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
    let response_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()) && name.as_str() != "content-length")
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.to_string(), value.to_string()))
        })
        .collect();
    let flattened_headers = flatten_pairs(&response_headers);
    let headers: Vec<_> = response_headers
        .into_iter()
        .filter_map(|(name, value)| Header::from_bytes(name, value).ok())
        .collect();
    let captured = Arc::new(Mutex::new(CapturedBody::default()));
    let reader = CapturingReader::new(response, Arc::clone(&captured));
    let response = Response::new(StatusCode(status), headers, reader, content_length, None);
    if let Err(error) = request.respond(response) {
        emit_output(output_tx, format!("Upstream response interrupted: {error}"));
    }
    let captured_response = captured
        .lock()
        .map(|body| body.snapshot())
        .unwrap_or_default();
    emit_log_entry(
        context,
        status,
        CapturedResponse {
            headers: flattened_headers,
            ..captured_response
        },
        output_tx,
        logs_tx,
    );
}

fn finish_with_error(
    request: Request,
    context: RequestContext,
    status: u16,
    message: String,
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
        output_tx,
        logs_tx,
    );
}

fn finish_response(
    request: Request,
    context: RequestContext,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let response_headers = flatten_pairs(&headers);
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
        emit_output(output_tx, format!("Could not write response: {error}"));
    }

    emit_log_entry(
        context,
        status,
        CapturedResponse {
            headers: response_headers,
            body: captured_response,
            size: response_size,
            truncated: response_truncated,
        },
        output_tx,
        logs_tx,
    );
}

fn emit_log_entry(
    context: RequestContext,
    status: u16,
    captured_response: CapturedResponse,
    output_tx: &SyncSender<String>,
    logs_tx: &SyncSender<LogEntry>,
) {
    let entry = LogEntry {
        method: context.method,
        path: context.path,
        query: context.query,
        status,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        request: ExchangePart {
            headers: context.headers,
            body: String::from_utf8_lossy(&context.body).into_owned(),
            size: context.body_size,
            truncated: context.body_truncated,
        },
        response: ExchangePart {
            headers: captured_response.headers,
            body: String::from_utf8_lossy(&captured_response.body).into_owned(),
            size: captured_response.size,
            truncated: captured_response.truncated,
        },
        latency_ms: context.started_at.elapsed().as_millis(),
    };
    let summary = format!(
        "{} {} -> {} ({}ms)",
        entry.method, entry.path, entry.status, entry.latency_ms
    );
    if logs_tx.try_send(entry).is_err() {
        emit_output(
            output_tx,
            "Structured log buffer full; dropped one request".into(),
        );
    }
    emit_output(output_tx, summary);
}

#[derive(Default)]
struct CapturedResponse {
    headers: BTreeMap<String, String>,
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

fn flatten_request_headers(request: &Request) -> BTreeMap<String, String> {
    let mut flattened = BTreeMap::new();
    for header in request.headers() {
        append_header(
            &mut flattened,
            header.field.as_str().to_string(),
            header.value.as_str().to_string(),
        );
    }
    flattened
}

fn flatten_pairs(headers: &[(String, String)]) -> BTreeMap<String, String> {
    let mut flattened = BTreeMap::new();
    for (name, value) in headers {
        append_header(&mut flattened, name.clone(), value.clone());
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

fn find_header(headers: &BTreeMap<String, String>, wanted: &str) -> Option<String> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.clone())
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
    use std::{sync::mpsc, thread, time::Duration};

    use tiny_http::{Header, Response, Server, StatusCode};

    use super::{CaptureServer, MAX_CAPTURED_BODY_BYTES};

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
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            observed_tx.send((url, forwarded_host, body)).unwrap();

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
            .body("payload")
            .send()
            .unwrap();
        assert_eq!(response.status(), 201);
        assert_eq!(response.headers()["X-Upstream"], "yes");
        assert_eq!(response.text().unwrap(), "created");

        let (url, forwarded_host, body) = observed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(url, "/api/widgets/7?source=test&expand=owner");
        assert!(!forwarded_host.is_empty());
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
