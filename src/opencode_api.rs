//! A minimal client for the local opencode server's HTTP API. The server
//! listens on loopback and publishes its URL and password in
//! `service.json`; a handful of plain HTTP/1.1 requests is all triage needs.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Why a message may not have reached the session.
#[derive(Debug)]
pub enum Failure {
    /// The server never got it, or answered that it did not take it.
    NotDelivered(io::Error),
    /// The request went out but no answer came back: it may have been taken.
    Unconfirmed(io::Error),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NotDelivered(e) | Failure::Unconfirmed(e) => e.fmt(f),
        }
    }
}

impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Failure::NotDelivered(e)
    }
}

fn io_timeout() -> Duration {
    if cfg!(debug_assertions)
        && let Some(ms) = std::env::var("TRIAGE_TEST_HTTP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    IO_TIMEOUT
}

#[derive(Deserialize)]
struct ServiceFile {
    url: String,
    pid: u32,
    password: String,
}

struct Service {
    addr: SocketAddr,
    host: String,
    password: String,
}

fn service_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
        });
    state.join("opencode/service.json")
}

fn load_service() -> io::Result<Service> {
    let path = service_path();
    let file: ServiceFile = serde_json::from_slice(&std::fs::read(&path)?)
        .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    let host = file
        .url
        .strip_prefix("http://")
        .map(|rest| rest.trim_end_matches('/'))
        .filter(|host| !host.contains('/'))
        .ok_or_else(|| io::Error::other(format!("unsupported server URL {:?}", file.url)))?;
    // The password goes out in every request, so only ever to this machine.
    let addr = host
        .to_socket_addrs()?
        .find(|addr| addr.ip().is_loopback())
        .ok_or_else(|| io::Error::other(format!("server URL {:?} is not loopback", file.url)))?;
    check_server(file.pid, addr.port())?;
    Ok(Service {
        addr,
        host: host.to_string(),
        password: file.password,
    })
}

/// The loopback server, ready for requests that give up after `timeout`.
pub struct Server {
    service: Service,
    timeout: Duration,
}

/// A tool call waiting on the user's permission.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Permission {
    pub id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub resources: Vec<String>,
    #[serde(default)]
    pub source: Option<PermissionSource>,
}

/// The tool call a permission request is for.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionSource {
    #[serde(rename = "messageID")]
    pub message_id: String,
    pub id: String,
}

impl Permission {
    pub fn summary(&self) -> String {
        format!("{}: {}", self.action, self.resources.join(" "))
    }
}

/// What the server says about a session's current turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub busy: bool,
    /// When the turn started or its newest message last changed; viewing the
    /// session does not count.
    pub progress_ms: u64,
    /// A tool call in the current turn that has not finished.
    pub running_tool: Option<String>,
    /// False when the messages read did not reach back to the turn's start.
    pub whole_turn_seen: bool,
    pub pending_permissions: usize,
    pub pending_forms: usize,
    /// Changes whenever the turn's messages do, streamed text included.
    pub stamp: String,
}

/// How many of the newest messages `turn` reads looking for the turn's start.
const TURN_WINDOW: usize = 50;

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

impl Server {
    pub fn load() -> io::Result<Self> {
        Ok(Self {
            service: load_service()?,
            timeout: io_timeout(),
        })
    }

    #[cfg(test)]
    pub fn at(addr: SocketAddr) -> Self {
        Self {
            service: Service {
                addr,
                host: addr.to_string(),
                password: "test".to_string(),
            },
            timeout: Duration::from_secs(2),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn pending_permissions(&self, session: &str) -> io::Result<Vec<Permission>> {
        check_session(session)?;
        self.get(&format!("/api/session/{session}/permission"))
    }

    /// Answers one pending request: "once" to approve, never "always".
    pub fn reply_permission(
        &self,
        session: &str,
        request: &str,
        approve: bool,
    ) -> Result<(), Failure> {
        check_session(session)?;
        if !request.starts_with("per_")
            || !request
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(Failure::NotDelivered(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid permission request id {request:?}"),
            )));
        }
        let decision = if approve { "once" } else { "reject" };
        let (status, _) = self.request(
            "POST",
            &format!("/api/session/{session}/permission/{request}/reply"),
            Some(&serde_json::json!({ "decision": decision })),
        )?;
        Ok(expect_success(status)?)
    }

    /// Stops the running turn; false when the server had none to stop.
    pub fn interrupt(&self, session: &str) -> Result<bool, Failure> {
        check_session(session)?;
        let (status, body) = self.request(
            "POST",
            &format!("/api/session/{session}/interrupt"),
            Some(&serde_json::json!({})),
        )?;
        expect_success(status)?;
        Ok(serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("interrupted").and_then(Value::as_bool))
            .unwrap_or(true))
    }

    pub fn turn(&self, session: &str) -> io::Result<Turn> {
        check_session(session)?;
        let record: Value = self.get(&format!("/api/session/{session}"))?;
        let messages: Vec<Value> = self.get(&format!(
            "/api/session/{session}/message?limit={TURN_WINDOW}"
        ))?;
        let permissions: Vec<Value> = self.get(&format!("/api/session/{session}/permission"))?;
        let forms: Vec<Value> = self.get(&format!("/api/session/{session}/form"))?;
        Ok(turn_from(
            &record,
            &messages,
            permissions.len(),
            forms.len(),
        ))
    }

    /// The input of one tool call, as the model wrote it.
    pub fn tool_input(&self, session: &str, source: &PermissionSource) -> io::Result<Value> {
        check_session(session)?;
        let ok =
            |id: &str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !ok(&source.message_id) || !ok(&source.id) {
            return Err(io::Error::other("invalid tool call id"));
        }
        let message: Value = self.get(&format!(
            "/api/session/{session}/message/{}",
            source.message_id
        ))?;
        message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|part| part.get("id").and_then(Value::as_str) == Some(&source.id))
            .and_then(|part| part.pointer("/state/input"))
            .cloned()
            .ok_or_else(|| io::Error::other("the tool call is not in its message"))
    }

    /// The newest prompt the user typed.
    pub fn last_user_text(&self, session: &str) -> io::Result<Option<String>> {
        check_session(session)?;
        let messages: Vec<Value> = self.get(&format!(
            "/api/session/{session}/message?limit={TURN_WINDOW}"
        ))?;
        Ok(messages
            .iter()
            .filter(|m| m.get("type").and_then(Value::as_str) == Some("user"))
            .find_map(|m| m.get("text").and_then(Value::as_str))
            .map(str::to_string))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> io::Result<T> {
        let (status, body) = self.request("GET", path, None).map_err(|f| match f {
            Failure::NotDelivered(e) | Failure::Unconfirmed(e) => e,
        })?;
        expect_success(status)?;
        serde_json::from_slice::<Data<T>>(&body)
            .map(|d| d.data)
            .map_err(|e| io::Error::other(format!("unexpected answer from {path}: {e}")))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Vec<u8>), Failure> {
        let service = &self.service;
        let auth = base64(format!("opencode:{}", service.password).as_bytes());
        let (content, body) = match body {
            Some(body) => {
                let body = body.to_string();
                (
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\n",
                        body.len()
                    ),
                    body,
                )
            }
            None => (String::new(), String::new()),
        };
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {auth}\r\n\
             {content}Accept: application/json\r\nConnection: close\r\n\r\n{body}",
            host = service.host,
        );
        let mut stream =
            TcpStream::connect_timeout(&service.addr, self.timeout.min(CONNECT_TIMEOUT))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        // A request cut off part way may still have been taken.
        stream
            .write_all(request.as_bytes())
            .map_err(Failure::Unconfirmed)?;
        let mut response = Vec::new();
        let read = stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut response);
        // A server that answers and then holds the connection open still
        // answered.
        match parse_status(&response) {
            Some(status) => Ok((status, response_body(&response))),
            None => Err(Failure::Unconfirmed(read.err().unwrap_or_else(|| {
                io::Error::other("server sent no HTTP status line")
            }))),
        }
    }
}

fn check_session(session: &str) -> io::Result<()> {
    if crate::mailbox::is_opencode_session(session) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid opencode session id {session:?}"),
        ))
    }
}

fn expect_success(status: u16) -> io::Result<()> {
    if is_success(status) {
        Ok(())
    } else {
        Err(io::Error::other(format!("server answered {status}")))
    }
}

/// `messages` are newest first. The current turn is every message since the
/// last `idle` marker or the turn's start; only the model's own messages
/// count as progress, so queued prompts or mail neither hide a running tool
/// nor reset the quiet clock.
fn turn_from(
    record: &Value,
    messages: &[Value],
    pending_permissions: usize,
    pending_forms: usize,
) -> Turn {
    use std::hash::{Hash, Hasher};
    let time = |key: &str| {
        record
            .pointer(&format!("/time/{key}"))
            .and_then(Value::as_u64)
    };
    let busy = match (time("busy"), time("idle")) {
        (Some(busy), Some(idle)) => busy > idle,
        (Some(_), None) => true,
        _ => false,
    };
    let started = time("busy").unwrap_or(0);
    let mut progress_ms = started;
    let mut running_tool = None;
    let mut whole_turn_seen = messages.len() < TURN_WINDOW;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut seen_len = 0;
    for message in messages {
        let kind = message.get("type").and_then(Value::as_str);
        let created = message.pointer("/time/created").and_then(Value::as_u64);
        if kind == Some("idle") || created.is_some_and(|c| c < started) {
            whole_turn_seen = true;
            break;
        }
        if kind != Some("assistant") {
            continue;
        }
        progress_ms = progress_ms.max(newest_time(message));
        let text = message.to_string();
        seen_len += text.len();
        text.hash(&mut hasher);
        if running_tool.is_none() {
            running_tool = unfinished_tool(message);
        }
    }
    Turn {
        busy,
        progress_ms,
        running_tool,
        whole_turn_seen,
        pending_permissions,
        pending_forms,
        stamp: format!("{progress_ms}:{seen_len}:{:x}", hasher.finish()),
    }
}

fn unfinished_tool(message: &Value) -> Option<String> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool"))
        .find(|part| {
            !matches!(
                part.pointer("/state/status").and_then(Value::as_str),
                Some("completed" | "error")
            )
        })
        .map(|part| {
            part.get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string()
        })
}

/// The largest number under any `time` object, at any depth.
fn newest_time(value: &Value) -> u64 {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, v)| {
                let own = match (key.as_str(), v) {
                    ("time", Value::Object(times)) => {
                        times.values().filter_map(Value::as_u64).max().unwrap_or(0)
                    }
                    _ => 0,
                };
                own.max(newest_time(v))
            })
            .max()
            .unwrap_or(0),
        Value::Array(items) => items.iter().map(newest_time).max().unwrap_or(0),
        _ => 0,
    }
}

/// Adds `text` to the session as a synthetic (non-user) message, queued
/// behind any running turn, and lets the server start a turn when idle.
/// A server that rejects synthetic input gets it as a queued prompt.
pub fn post_message(session: &str, text: &str) -> Result<(), Failure> {
    check_session(session)?;
    let server = Server::load()?;
    let synthetic = serde_json::json!({
        "text": text,
        "description": "triage peer message",
        "delivery": "queue",
    });
    let (status, _) = server.request(
        "POST",
        &format!("/api/session/{session}/synthetic"),
        Some(&synthetic),
    )?;
    if is_success(status) {
        return Ok(());
    }
    if !matches!(status, 400 | 404 | 405 | 422) {
        return Err(io::Error::other(format!("server answered {status}")).into());
    }
    let prompt = serde_json::json!({ "text": text, "delivery": "queue" });
    let (status, _) = server.request(
        "POST",
        &format!("/api/session/{session}/prompt"),
        Some(&prompt),
    )?;
    Ok(expect_success(status)?)
}

/// A service.json left behind by a server that has gone would hand the
/// password and the mail to whatever now listens on that port, so the pid it
/// names must be a live opencode process that holds the listening socket.
fn check_server(pid: u32, port: u16) -> io::Result<()> {
    let info = crate::mailbox::proc_info(pid).map_err(|_| {
        io::Error::other(format!(
            "service.json names pid {pid}, which is not a running opencode server"
        ))
    })?;
    if info.comm != "opencode" && !crate::peer_hooks::is_test_pane_harness(pid) {
        return Err(io::Error::other(format!(
            "service.json names pid {pid}, which is not a running opencode server"
        )));
    }
    #[cfg(target_os = "linux")]
    if !owns_listener(pid, port) {
        return Err(io::Error::other(format!(
            "opencode pid {pid} does not own the listening port {port}"
        )));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = port;
    Ok(())
}

#[cfg(target_os = "linux")]
fn owns_listener(pid: u32, port: u16) -> bool {
    let inodes = listening_inodes(port);
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten().any(|fd| {
        std::fs::read_link(fd.path()).is_ok_and(|target| {
            target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .is_some_and(|inode| inodes.iter().any(|i| i == inode))
        })
    })
}

/// Socket inodes listening on `port`, from `/proc/net/tcp{,6}`.
#[cfg(target_os = "linux")]
fn listening_inodes(port: u16) -> Vec<String> {
    const LISTEN: &str = "0A";
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|table| {
            table
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    let local_port = fields.get(1)?.rsplit_once(':')?.1;
                    (u16::from_str_radix(local_port, 16).ok()? == port && *fields.get(3)? == LISTEN)
                        .then(|| fields.get(9).map(|inode| inode.to_string()))
                        .flatten()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// The body after the headers, de-chunked when the server streamed it.
fn response_body(response: &[u8]) -> Vec<u8> {
    let Some(split) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Vec::new();
    };
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let body = &response[split + 4..];
    let chunked = head
        .lines()
        .any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
    if !chunked {
        return body.to_vec();
    }
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&rest[..eol])
            .ok()
            .and_then(|line| usize::from_str_radix(line.split(';').next()?.trim(), 16).ok());
        let Some(size) = size.filter(|&n| n > 0) else {
            break;
        };
        let Some(chunk) = rest.get(eol + 2..eol + 2 + size) else {
            break;
        };
        out.extend_from_slice(chunk);
        rest = rest.get(eol + 4 + size..).unwrap_or_default();
    }
    out
}

fn parse_status(response: &[u8]) -> Option<u16> {
    let line = response.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    parts.next()?.starts_with("HTTP/").then_some(())?;
    parts.next()?.parse().ok()
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A loopback stand-in for the server: answers each request with
/// `respond(method, path)` and hands back `(method, path, body)` of each.
#[cfg(test)]
pub fn fake_server(
    respond: impl Fn(&str, &str) -> (u16, String) + Send + 'static,
) -> (Server, std::sync::mpsc::Receiver<(String, String, String)>) {
    use std::io::BufRead;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut words = line.split_whitespace();
            let method = words.next().unwrap_or("").to_string();
            let path = words.next().unwrap_or("").to_string();
            let mut len = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end().to_ascii_lowercase();
                if header.is_empty() {
                    break;
                }
                if let Some(v) = header.strip_prefix("content-length: ") {
                    len = v.parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let (status, answer) = respond(&method, &path);
            let sent = tx.send((method, path, String::from_utf8_lossy(&body).into_owned()));
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{answer}",
                answer.len()
            )
            .unwrap();
            if sent.is_err() {
                break;
            }
        }
    });
    (Server::at(addr), rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"opencode:p4ss?>"), "b3BlbmNvZGU6cDRzcz8+");
    }

    #[test]
    fn reads_the_status_code() {
        assert_eq!(parse_status(b"HTTP/1.1 204 No Content\r\n\r\n"), Some(204));
        assert_eq!(parse_status(b"garbage"), None);
    }
    fn record() -> Value {
        serde_json::json!({"time": {"busy": 100, "idle": 50, "updated": 900, "viewed": 999}})
    }

    fn assistant(status: &str, text: &str) -> Value {
        serde_json::json!({"id": "msg_2", "type": "assistant", "time": {"created": 110},
            "content": [{"type": "text", "text": text},
                        {"type": "tool", "name": "shell", "state": {"status": status}, "time": {"created": 120}}]})
    }

    #[test]
    fn a_queued_message_does_not_hide_a_running_tool_or_count_as_progress() {
        let queued = serde_json::json!({"id": "msg_3", "type": "user", "text": "mail", "time": {"created": 800}});
        let idle = serde_json::json!({"id": "msg_0", "type": "idle", "time": {"created": 50}});
        let turn = turn_from(
            &record(),
            &[queued, assistant("running", "building"), idle],
            0,
            0,
        );
        assert!(turn.busy);
        assert_eq!(turn.running_tool.as_deref(), Some("shell"));
        assert_eq!(turn.progress_ms, 120);
        assert!(turn.whole_turn_seen);
    }

    #[test]
    fn streamed_text_changes_the_stamp() {
        let a = turn_from(&record(), &[assistant("completed", "par")], 0, 0);
        let b = turn_from(&record(), &[assistant("completed", "partial")], 0, 0);
        assert_ne!(a.stamp, b.stamp);
    }

    #[test]
    fn a_turn_longer_than_the_window_is_not_fully_seen() {
        let many = vec![assistant("completed", "x"); TURN_WINDOW];
        assert!(!turn_from(&record(), &many, 0, 0).whole_turn_seen);
    }

    #[test]
    fn reads_a_plain_or_chunked_body() {
        assert_eq!(
            response_body(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"),
            b"{}"
        );
        assert_eq!(
            response_body(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n"
            ),
            b"{\"a\":1}"
        );
    }
}
