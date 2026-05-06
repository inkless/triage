use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use serde_json::Value;

/// Single tool-use approval request the hook is waiting on.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingApproval {
    pub uuid: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub tool_name: String,
    pub tool_input_brief: String,
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

pub fn alive_file() -> PathBuf {
    triage_dir().join(".alive")
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
pub fn read_pending() -> Vec<PendingApproval> {
    let dir = pending_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(uuid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
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
        let created_at = entry
            .metadata()
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .unwrap_or(SystemTime::now());
        out.push(PendingApproval {
            uuid: uuid.to_string(),
            session_id,
            cwd,
            tool_name,
            tool_input_brief,
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

/// Render a one-line preview of the tool input. Different tools have different
/// shapes — Bash has `command`, Edit has `file_path`+`old_string`, etc. We pull
/// the most-useful field per tool, falling back to a JSON-truncated form.
fn brief_tool_input(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    if let Some(cmd) = input.get("command").and_then(|s| s.as_str()) {
        return truncate(cmd, 120);
    }
    if let Some(path) = input.get("file_path").and_then(|s| s.as_str()) {
        return truncate(path, 120);
    }
    if let Some(url) = input.get("url").and_then(|s| s.as_str()) {
        return truncate(url, 120);
    }
    if let Some(s) = input.as_str() {
        return truncate(s, 120);
    }
    truncate(&input.to_string(), 120)
}

fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Match each pending approval to its session, preferring sessionId match.
/// Falls back to cwd match if sessionId doesn't line up (which can happen when
/// the user has `/clear`'d — the sessions JSON keeps the stale sessionId but
/// the hook payload uses the live one).
pub fn attach_to_sessions(approvals: Vec<PendingApproval>, sessions: &mut [crate::models::Session]) {
    for a in approvals {
        let by_id = sessions.iter_mut().find(|s| s.session_id == a.session_id);
        let target = match by_id {
            Some(s) => Some(s),
            None => sessions.iter_mut().find(|s| s.cwd == a.cwd),
        };
        if let Some(s) = target {
            s.pending_approvals.push(a);
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
