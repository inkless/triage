use std::collections::HashSet;
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
    #[serde(rename = "nameSource", default)]
    name_source: Option<String>,
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
    #[serde(rename = "jobId", default)]
    job_id: Option<String>,
    #[serde(rename = "parkedJobId", default)]
    parked_job_id: Option<String>,
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

    let mut raw_sessions = Vec::new();
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
        raw_sessions.push(raw);
    }

    let active_background_jobs = active_background_job_keys(&raw_sessions);
    let mut out = Vec::new();
    for raw in raw_sessions {
        if let Some(session) = raw_to_session_with_background_jobs(raw, &active_background_jobs) {
            out.push(session);
        }
    }
    out
}

fn active_background_job_keys(raw_sessions: &[RawSession]) -> HashSet<(String, String)> {
    raw_sessions
        .iter()
        .filter(|raw| raw.kind.as_deref() == Some("bg"))
        .filter(|raw| raw.status.as_deref() == Some("busy"))
        .filter_map(|raw| {
            let job_id = raw.job_id.as_deref()?.trim();
            if job_id.is_empty() {
                return None;
            }
            Some((raw.cwd.clone(), job_id.to_string()))
        })
        .collect()
}

/// Turn a parsed session JSON into a `Session`, or `None` if it should be
/// hidden from the TUI. The `pid_alive` check stays in the caller (it touches
/// the live process table); everything here is a pure function of the JSON, so
/// it's unit-testable without a real sessions dir.
#[cfg(test)]
fn raw_to_session(raw: RawSession) -> Option<Session> {
    raw_to_session_with_background_jobs(raw, &HashSet::new())
}

fn raw_to_session_with_background_jobs(
    raw: RawSession,
    active_background_jobs: &HashSet<(String, String)>,
) -> Option<Session> {
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
    let active_background_job = raw.parked_job_id.as_deref().is_some_and(|job_id| {
        active_background_jobs.contains(&(raw.cwd.clone(), job_id.to_string()))
    });
    let name_is_derived = raw.name_source.as_deref() == Some("derived");
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
    session.name_is_derived = name_is_derived;
    session.cli_version = raw.version;
    session.active_background_jobs = usize::from(active_background_job);
    Some(session)
}

pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) — ESRCH if dead, EPERM if alive but not ours.
    unsafe {
        let r = libc::kill(pid as libc::pid_t, 0);
        if r == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
                "name":"agent-UAC-24","nameSource":"derived","status":"idle",
                "kind":"interactive"}"#);
        let session = raw_to_session(interactive).expect("interactive kept");
        assert_eq!(session.pid, 12919);
        assert!(session.name_is_derived);

        // Older Claude session JSONs predate the `kind` field — absent kind
        // must default to "keep", not silently drop every session.
        let kindless = raw(r#"{"pid":777,"sessionId":"old","cwd":"/repo/ux","name":"legacy"}"#);
        let session = raw_to_session(kindless).expect("kindless kept");
        assert!(!session.name_is_derived);
    }

    #[test]
    fn drops_auditor_sessions() {
        let auditor = raw(&format!(
            r#"{{"pid":42,"sessionId":"aud","cwd":"/repo/ux","name":"{}"}}"#,
            crate::auditor::AUDITOR_NAME
        ));
        assert!(raw_to_session(auditor).is_none());
    }

    #[test]
    fn links_busy_background_job_to_parked_parent() {
        let parent = raw(r#"{"pid":42,"sessionId":"parent","cwd":"/repo/ux",
                "kind":"interactive","status":"idle","parkedJobId":"abc12345"}"#);
        let child = raw(r#"{"pid":43,"sessionId":"child","cwd":"/repo/ux",
                "kind":"bg","status":"busy","jobId":"abc12345"}"#);
        let jobs = active_background_job_keys(&[child]);

        let session =
            raw_to_session_with_background_jobs(parent, &jobs).expect("interactive parent kept");

        assert_eq!(session.active_background_jobs, 1);
    }

    #[test]
    fn does_not_link_idle_or_other_cwd_background_job() {
        let idle_child = raw(r#"{"pid":43,"sessionId":"idle","cwd":"/repo/ux",
                "kind":"bg","status":"idle","jobId":"abc12345"}"#);
        let other_cwd_child = raw(r#"{"pid":44,"sessionId":"other","cwd":"/repo/other",
                "kind":"bg","status":"busy","jobId":"abc12345"}"#);
        let jobs = active_background_job_keys(&[idle_child, other_cwd_child]);
        let parent = raw(r#"{"pid":42,"sessionId":"parent","cwd":"/repo/ux",
                "kind":"interactive","status":"idle","parkedJobId":"abc12345"}"#);

        let session =
            raw_to_session_with_background_jobs(parent, &jobs).expect("interactive parent kept");

        assert_eq!(session.active_background_jobs, 0);
    }
}
