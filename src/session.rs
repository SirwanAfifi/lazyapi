use std::{
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::collections::VecDeque;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use serde::de::{Deserializer as _, Error as _, SeqAccess, Visitor};
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::model::{ExchangePart, LogEntry};

static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub fn load(path: &Path) -> Result<Vec<LogEntry>, String> {
    load_retaining(path, None)
}

/// Loads only the newest entries, keeping memory bounded for display-only sessions.
#[cfg(test)]
pub fn load_recent(path: &Path, limit: usize) -> Result<Vec<LogEntry>, String> {
    load_retaining(path, Some(limit))
}

#[cfg(test)]
fn load_retaining(path: &Path, limit: Option<usize>) -> Result<Vec<LogEntry>, String> {
    let mut retained = RetainedEntries::new(limit);
    visit(path, |entry| {
        retained.push(entry);
        Ok(())
    })?;
    Ok(retained.into_vec())
}

/// Visits session entries one at a time without materializing the complete input.
pub fn visit(
    path: &Path,
    mut visitor: impl FnMut(LogEntry) -> io::Result<()>,
) -> Result<(), String> {
    let first = first_non_whitespace(path)
        .map_err(|error| format!("could not read session {}: {error}", path.display()))?;
    let Some(first) = first else {
        return Ok(());
    };
    let file = File::open(path)
        .map_err(|error| format!("could not read session {}: {error}", path.display()))?;
    if first == b'[' {
        let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
        return deserializer
            .deserialize_seq(EntriesVisitor {
                visitor: &mut visitor,
            })
            .map_err(|error| format!("invalid session JSON in {}: {error}", path.display()));
    }

    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| {
            format!(
                "could not read session entry on line {} in {}: {error}",
                index + 1,
                path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = serde_json::from_str(&line).map_err(|error| {
            format!(
                "invalid session entry on line {} in {}: {error}",
                index + 1,
                path.display()
            )
        })?;
        visitor(entry).map_err(|error| {
            format!(
                "could not process session entry on line {} in {}: {error}",
                index + 1,
                path.display()
            )
        })?;
    }
    Ok(())
}

fn first_non_whitespace(path: &Path) -> io::Result<Option<u8>> {
    let file = File::open(path)?;
    for byte in BufReader::new(file).bytes() {
        let byte = byte?;
        if !byte.is_ascii_whitespace() {
            return Ok(Some(byte));
        }
    }
    Ok(None)
}

#[cfg(test)]
struct RetainedEntries {
    entries: VecDeque<LogEntry>,
    limit: Option<usize>,
}

#[cfg(test)]
impl RetainedEntries {
    fn new(limit: Option<usize>) -> Self {
        Self {
            entries: VecDeque::new(),
            limit,
        }
    }

    fn push(&mut self, entry: LogEntry) {
        if self.limit == Some(0) {
            return;
        }
        if self.limit.is_some_and(|limit| self.entries.len() == limit) {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    fn into_vec(self) -> Vec<LogEntry> {
        self.entries.into()
    }
}

struct EntriesVisitor<'a, F> {
    visitor: &'a mut F,
}

impl<'de, F> Visitor<'de> for EntriesVisitor<'_, F>
where
    F: FnMut(LogEntry) -> io::Result<()>,
{
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON array of captured exchanges")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(entry) = sequence.next_element()? {
            (self.visitor)(entry).map_err(A::Error::custom)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum SessionFormat {
    Json,
    JsonLines,
    Har,
}

impl SessionFormat {
    fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("har") => Self::Har,
            Some("jsonl" | "ndjson") => Self::JsonLines,
            _ => Self::Json,
        }
    }
}

/// Streams a complete session to a private temporary file and atomically publishes it on finish.
pub(crate) struct SessionRecorder {
    target: PathBuf,
    temporary: PathBuf,
    writer: Option<BufWriter<File>>,
    format: SessionFormat,
    entries: usize,
    committed: bool,
}

impl SessionRecorder {
    pub(crate) fn new(target: &Path) -> io::Result<Self> {
        let (temporary, file) = create_private_temporary_file(target)?;
        let mut recorder = Self {
            target: target.to_path_buf(),
            temporary,
            writer: Some(BufWriter::new(file)),
            format: SessionFormat::from_path(target),
            entries: 0,
            committed: false,
        };
        recorder.write_prefix()?;
        Ok(recorder)
    }

    pub(crate) fn push(&mut self, entry: &LogEntry) -> io::Result<()> {
        let format = self.format;
        let has_previous = self.entries > 0;
        let writer = self.writer()?;
        match format {
            SessionFormat::Json => {
                if has_previous {
                    writer.write_all(b",\n")?;
                }
                serde_json::to_writer(&mut *writer, entry).map_err(invalid_json)?;
            }
            SessionFormat::JsonLines => {
                serde_json::to_writer(&mut *writer, entry).map_err(invalid_json)?;
                writer.write_all(b"\n")?;
            }
            SessionFormat::Har => {
                if has_previous {
                    writer.write_all(b",\n")?;
                }
                serde_json::to_writer(&mut *writer, &har_entry(entry)).map_err(invalid_json)?;
            }
        }
        self.entries += 1;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        let format = self.format;
        {
            let writer = self.writer()?;
            match format {
                SessionFormat::Json => writer.write_all(b"\n]\n")?,
                SessionFormat::JsonLines => {}
                SessionFormat::Har => writer.write_all(b"\n]}}\n")?,
            }
            writer.flush()?;
            writer.get_ref().sync_all()?;
        }
        drop(self.writer.take());
        replace_file(&self.temporary, &self.target)?;
        self.committed = true;
        Ok(())
    }

    fn write_prefix(&mut self) -> io::Result<()> {
        let format = self.format;
        let writer = self.writer()?;
        match format {
            SessionFormat::Json => writer.write_all(b"[\n"),
            SessionFormat::JsonLines => Ok(()),
            SessionFormat::Har => {
                writer.write_all(
                    b"{\"log\":{\"version\":\"1.2\",\"creator\":{\"name\":\"LazyAPI\",\"version\":",
                )?;
                serde_json::to_writer(&mut *writer, env!("CARGO_PKG_VERSION"))
                    .map_err(invalid_json)?;
                writer.write_all(b"},\"entries\":[\n")
            }
        }
    }

    fn writer(&mut self) -> io::Result<&mut BufWriter<File>> {
        self.writer.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "session recorder is already closed",
            )
        })
    }
}

impl Drop for SessionRecorder {
    fn drop(&mut self) {
        if !self.committed {
            drop(self.writer.take());
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

#[cfg(test)]
pub fn save(path: &Path, entries: &[LogEntry]) -> Result<(), String> {
    let result = (|| {
        let mut recorder = SessionRecorder::new(path)?;
        for entry in entries {
            recorder.push(entry)?;
        }
        recorder.finish()
    })();
    result.map_err(|error| format!("could not save session {}: {error}", path.display()))
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn create_private_temporary_file(target: &Path) -> io::Result<(PathBuf, File)> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "session path has no file name")
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..1_000 {
        let sequence = SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(
            ".lazyapi-{}-{timestamp}-{sequence}.tmp",
            std::process::id()
        ));
        let temporary = parent.join(temporary_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    drop(file);
                    let _ = fs::remove_file(&temporary);
                    return Err(error);
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique session output file",
    ))
}

fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

fn har_entry(entry: &LogEntry) -> Value {
    let host = entry
        .request
        .header_value("host")
        .unwrap_or("lazyapi.local");
    let target = entry.query.as_ref().map_or_else(
        || entry.path.clone(),
        |query| format!("{}?{query}", entry.path),
    );
    let query: Vec<_> = entry
        .query
        .as_deref()
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .map(|(name, value)| json!({ "name": name, "value": value }))
                .collect()
        })
        .unwrap_or_default();
    let request_mime = entry.request.header_value("content-type").unwrap_or("");
    let response_mime = entry.response.header_value("content-type").unwrap_or("");
    let elapsed = u64::try_from(entry.latency_ms).unwrap_or(u64::MAX);

    let mut request = json!({
        "method": entry.method,
        "url": format!("http://{host}{target}"),
        "httpVersion": "HTTP/1.1",
        "headers": har_headers(&entry.request),
        "queryString": query,
        "cookies": [],
        "headersSize": -1,
        "bodySize": entry.request.size
    });
    if !entry.request.body.is_empty() {
        request["postData"] = json!({
            "mimeType": request_mime,
            "text": entry.request.body
        });
    }

    json!({
        "startedDateTime": entry.timestamp,
        "time": elapsed,
        "request": request,
        "response": {
            "status": entry.status,
            "statusText": status_text(entry.status),
            "httpVersion": "HTTP/1.1",
            "headers": har_headers(&entry.response),
            "cookies": [],
            "content": {
                "size": entry.response.size,
                "mimeType": response_mime,
                "text": entry.response.body
            },
            "redirectURL": entry.response.header_value("location").unwrap_or(""),
            "headersSize": -1,
            "bodySize": entry.response.size
        },
        "cache": {},
        "timings": {
            "send": 0,
            "wait": elapsed,
            "receive": 0
        }
    })
}

fn har_headers(part: &ExchangePart) -> Vec<Value> {
    part.iter_headers()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use serde_json::Value;
    use tempfile::{NamedTempFile, tempdir};

    use super::{SessionRecorder, load, load_recent, save};
    use crate::model::{ExchangePart, HeaderValue, LogEntry};

    fn entry(method: &str, path: &str) -> LogEntry {
        LogEntry {
            method: method.into(),
            path: path.into(),
            ..LogEntry::default()
        }
    }

    #[test]
    fn saves_and_loads_json_sessions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.json");
        let entries = vec![LogEntry {
            method: "GET".into(),
            path: "/health".into(),
            status: 200,
            ..LogEntry::default()
        }];

        save(&path, &entries).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, "/health");
    }

    #[test]
    fn loads_json_lines_for_interoperability() {
        let mut file = NamedTempFile::new().unwrap();
        let first = serde_json::to_string(&LogEntry {
            method: "GET".into(),
            path: "/one".into(),
            ..LogEntry::default()
        })
        .unwrap();
        let second = serde_json::to_string(&LogEntry {
            method: "POST".into(),
            path: "/two".into(),
            ..LogEntry::default()
        })
        .unwrap();
        writeln!(file, "{first}\n{second}").unwrap();

        let loaded = load(file.path()).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].method, "POST");
    }

    #[test]
    fn streams_json_lines_and_retains_only_the_requested_tail() {
        let directory = tempdir().unwrap();
        let json_path = directory.path().join("session.json");
        let jsonl_path = directory.path().join("session.jsonl");
        let entries: Vec<_> = (0..5)
            .map(|index| entry("GET", &format!("/{index}")))
            .collect();

        save(&json_path, &entries).unwrap();
        save(&jsonl_path, &entries).unwrap();
        assert!(fs::read_to_string(&jsonl_path).unwrap().starts_with('{'));
        for path in [&json_path, &jsonl_path] {
            let recent = load_recent(path, 2).unwrap();
            assert_eq!(recent.len(), 2);
            assert_eq!(recent[0].path, "/3");
            assert_eq!(recent[1].path, "/4");
        }
    }

    #[test]
    fn exports_har_when_requested() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.har");
        let entries = vec![LogEntry {
            method: "GET".into(),
            path: "/health".into(),
            query: Some("verbose=true".into()),
            status: 200,
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            ..LogEntry::default()
        }];

        save(&path, &entries).unwrap();
        let document: Value = serde_json::from_reader(fs::File::open(path).unwrap()).unwrap();
        assert_eq!(document["log"]["version"], "1.2");
        assert_eq!(document["log"]["entries"][0]["request"]["method"], "GET");
    }

    #[test]
    fn har_preserves_repeated_headers() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("headers.har");
        let entries = vec![LogEntry {
            method: "GET".into(),
            path: "/cookies".into(),
            response: ExchangePart {
                headers: std::collections::BTreeMap::from([(
                    "Set-Cookie".into(),
                    "theme=dark, session=abc".into(),
                )]),
                header_values: vec![
                    HeaderValue::new("Set-Cookie", "theme=dark"),
                    HeaderValue::new("Set-Cookie", "session=abc"),
                ],
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        }];

        save(&path, &entries).unwrap();
        let document: Value = serde_json::from_reader(fs::File::open(path).unwrap()).unwrap();
        let headers = document["log"]["entries"][0]["response"]["headers"]
            .as_array()
            .unwrap();
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0]["value"], "theme=dark");
        assert_eq!(headers[1]["value"], "session=abc");
    }

    #[test]
    fn old_sessions_fall_back_to_flattened_headers() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"[
                {{
                    "method": "GET",
                    "path": "/legacy",
                    "status": 200,
                    "timestamp": "2026-01-01T00:00:00.000Z",
                    "request": {{"headers": {{"Host": "legacy.test"}}, "body": ""}},
                    "response": {{"headers": {{"X-Legacy": "present"}}, "body": ""}},
                    "latencyMs": 1
                }}
            ]"#
        )
        .unwrap();

        let entries = load(file.path()).unwrap();
        assert!(entries[0].request.header_values.is_empty());
        assert_eq!(entries[0].request.header_value("host"), Some("legacy.test"));
        let directory = tempdir().unwrap();
        let path = directory.path().join("legacy.har");
        save(&path, &entries).unwrap();
        let document: Value = serde_json::from_reader(fs::File::open(path).unwrap()).unwrap();
        assert_eq!(
            document["log"]["entries"][0]["response"]["headers"][0]["value"],
            "present"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_sessions_are_private_and_replace_existing_files_atomically() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("private.json");
        let first = entry("GET", "/first");
        save(&path, std::slice::from_ref(&first)).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&path, "existing session").unwrap();
        let mut recorder = SessionRecorder::new(&path).unwrap();
        recorder.push(&entry("POST", "/replacement")).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "existing session");
        recorder.finish().unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].path, "/replacement");
    }
}
