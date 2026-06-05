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
    #[serde(default)]
    kind: Option<String>,
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
        if let Some(session) = raw_to_session(raw) {
            out.push(session);
        }
    }
    out
}

/// Turn a parsed session JSON into a `Session`, or `None` if it should be
/// hidden from the TUI. The `pid_alive` check stays in the caller (it touches
/// the live process table); everything here is a pure function of the JSON, so
/// it's unit-testable without a real sessions dir.
fn raw_to_session(raw: RawSession) -> Option<Session> {
    // Skip Claude's warm background-pool processes. Newer Claude (2.1.16x)
    // keeps `--bg-spare` / `--bg-pty-host` daemons that each write their own
    // session JSON tagged `kind: "bg"`. They're not interactive sessions and
    // their pty-host often reparents to PID 1, detaching from the owning
    // pane's process tree — so they neither pid-walk to a pane nor get
    // collapsed by `dedup_sessions_by_pane` (pane-less rows never collide),
    // surfacing as phantom "no tmux pane" duplicates (TRI-137).
    if raw.kind.as_deref() == Some("bg") {
        return None;
    }
    // Filter the auditor's own short-lived Claude process so it doesn't appear
    // as a row (and never gets recursively audited). The auditor tags itself
    // via `claude --name triage-auditor`.
    if raw.name.as_deref() == Some(crate::auditor::AUDITOR_NAME) {
        return None;
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
    Some(session)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> RawSession {
        serde_json::from_str(json).expect("valid session JSON")
    }

    #[test]
    fn drops_bg_kind_sessions() {
        // A Claude `--bg-spare` daemon writes its own JSON tagged kind:"bg"
        // (TRI-137). It must not surface as a row.
        let raw = raw(r#"{"pid":31903,"sessionId":"a12ff1bb","cwd":"/repo/ux",
                "name":"Manage UAC work coordination","status":"idle",
                "kind":"bg","version":"2.1.162"}"#);
        assert!(raw_to_session(raw).is_none());
    }

    #[test]
    fn keeps_interactive_and_kindless_sessions() {
        let interactive = raw(r#"{"pid":12919,"sessionId":"02c00b0b","cwd":"/repo/ux",
                "name":"agent-UAC-24","status":"idle","kind":"interactive"}"#);
        let session = raw_to_session(interactive).expect("interactive kept");
        assert_eq!(session.pid, 12919);

        // Older Claude session JSONs predate the `kind` field — absent kind
        // must default to "keep", not silently drop every session.
        let kindless = raw(r#"{"pid":777,"sessionId":"old","cwd":"/repo/ux","name":"legacy"}"#);
        assert!(raw_to_session(kindless).is_some());
    }

    #[test]
    fn drops_auditor_sessions() {
        let auditor = raw(&format!(
            r#"{{"pid":42,"sessionId":"aud","cwd":"/repo/ux","name":"{}"}}"#,
            crate::auditor::AUDITOR_NAME
        ));
        assert!(raw_to_session(auditor).is_none());
    }
}
