use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::models::{Provider, Session};

#[derive(Debug, Deserialize)]
struct RawSession {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "startedAt", default)]
    started_at: u64,
    #[serde(rename = "updatedAt", default)]
    updated_at: u64,
    #[serde(rename = "waitingFor", default)]
    waiting_for: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

pub fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/sessions")
}

pub fn discover_live_sessions() -> Vec<Session> {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(raw) = serde_json::from_slice::<RawSession>(&bytes) else {
            continue;
        };
        if !pid_alive(raw.pid) {
            continue;
        }
        // Filter the auditor's own short-lived Claude process so it doesn't
        // appear as a row (and never gets recursively audited). The auditor
        // tags itself via `claude --name triage-auditor`.
        if raw.name.as_deref() == Some(crate::auditor::AUDITOR_NAME) {
            continue;
        }
        let mut session = Session::new(
            Provider::Claude,
            raw.pid,
            raw.session_id,
            PathBuf::from(raw.cwd),
            raw.name,
            raw.status.unwrap_or_else(|| "unknown".to_string()),
            raw.started_at,
            raw.updated_at,
            raw.waiting_for,
        );
        session.cli_version = raw.version;
        out.push(session);
    }
    out
}

pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) — ESRCH if dead, EPERM if alive but not ours.
    unsafe {
        let r = libc::kill(pid as libc::pid_t, 0);
        if r == 0 {
            return true;
        }
        let err = *libc::__error();
        err == libc::EPERM
    }
}

#[allow(dead_code)]
pub fn projects_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/projects")
}

/// Encode a cwd to its `~/.claude/projects/` subdirectory name.
/// Claude Code transforms both `/` and `.` to `-`, so e.g.
/// `/Users/guangda.zhang/.worktrees/x` → `-Users-guangda-zhang--worktrees-x`.
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}
