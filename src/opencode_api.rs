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

#[derive(Deserialize)]
struct ServiceFile {
    url: String,
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
    Ok(Service {
        addr,
        host: host.to_string(),
        password: file.password,
    })
}

/// Adds `text` to the session as a synthetic (non-user) message, queued
/// behind any running turn, and lets the server start a turn when idle.
/// A server that rejects synthetic input gets it as a queued prompt.
pub fn post_message(session: &str, text: &str) -> io::Result<()> {
    if !crate::mailbox::is_opencode_session(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid opencode session id {session:?}"),
        ));
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
        return Err(io::Error::other(format!("server answered {status}")));
    }
    let prompt = serde_json::json!({ "text": text, "delivery": "queue" });
    let status = post(&service, &format!("/api/session/{session}/prompt"), &prompt)?;
    if is_success(status) {
        Ok(())
    } else {
        Err(io::Error::other(format!("server answered {status}")))
    }
}

fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn post(service: &Service, path: &str, body: &serde_json::Value) -> io::Result<u16> {
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
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut response)
        .or_else(|e| {
            // A server that answers and then holds the connection open still
            // answered.
            if response.is_empty() { Err(e) } else { Ok(0) }
        })?;
    parse_status(&response).ok_or_else(|| io::Error::other("server sent no HTTP status line"))
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
