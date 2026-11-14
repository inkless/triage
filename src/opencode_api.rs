//! A minimal client for the local opencode server's HTTP API. The server
//! listens on loopback and publishes its URL and password in
//! `service.json`; a handful of plain HTTP/1.1 requests is all triage needs.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

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

/// Adds `text` to the session as a synthetic (non-user) message, queued
/// behind any running turn, and lets the server start a turn when idle.
/// A server that rejects synthetic input gets it as a queued prompt.
pub fn post_message(session: &str, text: &str) -> Result<(), Failure> {
    if !crate::mailbox::is_opencode_session(session) {
        return Err(Failure::NotDelivered(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid opencode session id {session:?}"),
        )));
    }
    let service = load_service()?;
    let synthetic = serde_json::json!({
        "text": text,
        "description": "triage peer message",
        "delivery": "queue",
    });
    let status = post(
        &service,
        &format!("/api/session/{session}/synthetic"),
        &synthetic,
    )?;
    if is_success(status) {
        return Ok(());
    }
    if !matches!(status, 400 | 404 | 405 | 422) {
        return Err(io::Error::other(format!("server answered {status}")).into());
    }
    let prompt = serde_json::json!({ "text": text, "delivery": "queue" });
    let status = post(&service, &format!("/api/session/{session}/prompt"), &prompt)?;
    if is_success(status) {
        Ok(())
    } else {
        Err(io::Error::other(format!("server answered {status}")).into())
    }
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

fn post(service: &Service, path: &str, body: &serde_json::Value) -> Result<u16, Failure> {
    let body = body.to_string();
    let auth = base64(format!("opencode:{}", service.password).as_bytes());
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic {auth}\r\n\
         Content-Type: application/json\r\nContent-Length: {len}\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n{body}",
        host = service.host,
        len = body.len(),
    );
    let mut stream = TcpStream::connect_timeout(&service.addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(io_timeout()))?;
    stream.set_write_timeout(Some(io_timeout()))?;
    // A request cut off part way may still have been taken.
    stream
        .write_all(request.as_bytes())
        .map_err(Failure::Unconfirmed)?;
    let mut response = Vec::new();
    let read = stream.take(MAX_RESPONSE_BYTES).read_to_end(&mut response);
    // A server that answers and then holds the connection open still
    // answered.
    match parse_status(&response) {
        Some(status) => Ok(status),
        None => Err(Failure::Unconfirmed(read.err().unwrap_or_else(|| {
            io::Error::other("server sent no HTTP status line")
        }))),
    }
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
}
