use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Pending files older than this are stale. The hook itself only waits a few
/// seconds before falling back to Claude's native permission flow, so anything
/// that survives longer is from a hook process that died without running its
/// cleanup trap (cancelled tool call, SIGKILL, crash). We auto-delete on read
/// so orphaned files don't keep showing a fake pending approval.
const PENDING_TTL: Duration = Duration::from_secs(30);

/// Single tool-use approval request the hook is waiting on.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingApproval {
    pub uuid: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub tool_name: String,
    /// One-line summary used by the UI (truncated to 200–400 chars depending
    /// on tool type). NOT suitable for feeding the autonomous-mode auditor —
    /// truncating the body of a `gh pr create` or a multi-paragraph Edit makes
    /// it impossible to decide on safety. Use `tool_input_full` for that.
    pub tool_input_brief: String,
    /// Full `tool_input` JSON serialization, untruncated. The autonomous-mode
    /// auditor needs the whole command (especially for Bash heredocs and Edit
    /// new_string content) to make a confident decision. Empty string if the
    /// hook payload had no `tool_input` field.
    pub tool_input_full: String,
    pub created_at: SystemTime,
    pub pending_path: PathBuf,
}

pub fn triage_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/triage")
}

pub fn pending_dir() -> PathBuf {
    triage_dir().join("pending")
}

pub fn decisions_dir() -> PathBuf {
    triage_dir().join("decisions")
}

/// Auto-mode handshake dir. When the autonomous-mode auditor starts processing
/// a hook-captured request, triage writes `claims/<uuid>.json` here. The hook
/// extends its short timeout when it sees its uuid claimed, so the auditor
/// has time to reach a verdict (~10–25s on Sonnet) instead of the hook
/// defaulting to Claude's native permission flow at 3s.
pub fn claims_dir() -> PathBuf {
    triage_dir().join("claims")
}

pub fn alive_file() -> PathBuf {
    triage_dir().join(".alive")
}

/// Best-effort claim write. The hook reads `claims/<uuid>.json` and extends
/// its deadline when present; absence means triage isn't going to decide, so
/// the hook can bail to Claude's native flow.
pub fn write_claim(uuid: &str) {
    let dir = claims_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(format!("{uuid}.json")), b"{}");
}

/// Best-effort claim removal. Called after the auditor sends its verdict
/// (so the hook reacts to claim absence as "WAIT — let Claude handle it",
/// or to decision-file presence as "auditor decided, use this").
pub fn remove_claim(uuid: &str) {
    let _ = fs::remove_file(claims_dir().join(format!("{uuid}.json")));
}

/// Drop guard: writes our pid to `.alive` on construction, removes on drop.
/// The hook reads this to decide whether triage is intercepting tool calls.
pub struct AliveGuard;

impl AliveGuard {
    pub fn install() -> Self {
        let dir = triage_dir();
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(alive_file(), std::process::id().to_string());
        AliveGuard
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(alive_file());
    }
}

/// Read every pending file. Returns one PendingApproval per file. Files we
/// can't parse are skipped silently — the hook owns lifecycle, so a malformed
/// file means triage just won't surface it (the hook will time out and Claude
/// will fall back to its own prompt).
///
/// Side effect: deletes pending files older than `PENDING_TTL`. The hook
/// itself falls back after a few seconds, so anything that survives longer is
/// from a process that died without running its cleanup trap (cancelled tool
/// call, SIGKILL, crash).
pub fn read_pending() -> Vec<PendingApproval> {
    let dir = pending_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(uuid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let created_at = entry
            .metadata()
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .unwrap_or(now);
        if now
            .duration_since(created_at)
            .is_ok_and(|age| age > PENDING_TTL)
        {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(decisions_dir().join(format!("{uuid}.json")));
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let session_id = v
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let cwd = v
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        let tool_name = v
            .get("tool_name")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        let tool_input_brief = brief_tool_input(v.get("tool_input"));
        let tool_input_full = v
            .get("tool_input")
            .map(|val| {
                if let Some(s) = val.as_str() {
                    s.to_string()
                } else {
                    val.to_string()
                }
            })
            .unwrap_or_default();
        out.push(PendingApproval {
            uuid: uuid.to_string(),
            session_id,
            cwd,
            tool_name,
            tool_input_brief,
            tool_input_full,
            created_at,
            pending_path: path,
        });
    }
    out.sort_by_key(|p| p.created_at);
    out
}

/// Write the decision JSON the hook is polling for. The hook reads this and
/// emits it as its own stdout, which Claude consumes.
pub fn approve(uuid: &str) {
    write_decision(uuid, r#"{"decision":"approve"}"#);
}

pub fn deny(uuid: &str, reason: &str) {
    let payload = serde_json::json!({
        "decision": "block",
        "reason": reason,
    });
    write_decision(uuid, &payload.to_string());
}

fn write_decision(uuid: &str, body: &str) {
    let dir = decisions_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{uuid}.json"));
    let _ = fs::write(path, body);
}

/// Render a preview of the tool input. The hook payload has the full tool
/// argument JSON, so we can show meaningfully more than what we'd parse from
/// the pane: command + description for Bash, file path + edit summary for
/// Edit/Write, etc. Headline wraps to 4 lines so we lift the truncation cap.
pub fn brief_tool_input(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    if let Some(cmd) = input.get("command").and_then(|s| s.as_str()) {
        // Bash: show command + description on the same line so the row's
        // wrap_text can split them across visual lines naturally.
        let desc = input
            .get("description")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty());
        return match desc {
            Some(d) => truncate(&format!("{cmd}  ·  {d}"), 400),
            None => truncate(cmd, 400),
        };
    }
    if let Some(path) = input.get("file_path").and_then(|s| s.as_str()) {
        // Edit/Write: path + a short hint of what's changing. Edit has
        // `old_string`; Write has `content`. Truncate hard since long diffs
        // would dominate the row.
        let detail = input
            .get("old_string")
            .or_else(|| input.get("content"))
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| truncate(s, 120));
        return match detail {
            Some(d) => truncate(&format!("{path}  ·  {d}"), 400),
            None => truncate(path, 200),
        };
    }
    if let Some(url) = input.get("url").and_then(|s| s.as_str()) {
        return truncate(url, 200);
    }
    if let Some(s) = input.as_str() {
        return truncate(s, 200);
    }
    truncate(&input.to_string(), 200)
}

pub fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Match each pending approval to its session.
///
/// 1. Prefer `session_id` match — exact identity when sessions JSON is fresh.
/// 2. Fall back to cwd match. After `/clear` the sessions JSON keeps the
///    stale sessionId pointing at the pre-clear file, while the hook payload
///    carries the live sessionId — so a direct sessionId lookup misses.
///    When multiple sessions share a cwd (e.g. two lakehouse panes), prefer
///    one whose tmux pane is currently active over arbitrary first-match.
/// 3. If still ambiguous, attach to the first cwd-matching session — better
///    than dropping the approval entirely.
pub fn attach_to_sessions(approvals: Vec<PendingApproval>, sessions: &mut [crate::models::Session]) {
    for a in approvals {
        // 1. session_id exact match.
        if let Some(idx) = sessions.iter().position(|s| s.session_id == a.session_id) {
            sessions[idx].pending_approvals.push(a);
            continue;
        }
        // 2. cwd match, preferring an active pane.
        let cwd_matches: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (s.cwd == a.cwd).then_some(i))
            .collect();
        let chosen = cwd_matches
            .iter()
            .copied()
            .find(|&i| sessions[i].pane.as_ref().is_some_and(|p| p.active))
            .or_else(|| cwd_matches.first().copied());
        if let Some(idx) = chosen {
            sessions[idx].pending_approvals.push(a);
        }
    }
}

/// Print the `~/.claude/settings.json` snippet the user needs to add.
pub fn print_install_hint() {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join("../../scripts/hooks/triage-preuse.sh"))
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<repo>/scripts/hooks/triage-preuse.sh".to_string());
    println!("Add the following to ~/.claude/settings.json (merge with any existing hooks):");
    println!();
    println!("{{");
    println!("  \"hooks\": {{");
    println!("    \"PreToolUse\": [");
    println!("      {{");
    println!("        \"matcher\": \".*\",");
    println!("        \"hooks\": [");
    println!("          {{ \"type\": \"command\", \"command\": \"{path}\" }}");
    println!("        ]");
    println!("      }}");
    println!("    ]");
    println!("  }}");
    println!("}}");
}
