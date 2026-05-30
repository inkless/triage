//! `claude -p` auditor for autonomous mode (T-56). Read-only — no tool access,
//! no session persistence, capped budget. Tags itself with `--name
//! triage-auditor` so `discovery::discover_live_sessions` can filter the
//! auditor's own short-lived Claude process out of triage's listing.
//!
//! Two entry points:
//! - `audit(pid)` is the one-shot CLI form (`triage --audit <pid>`) used to
//!   eyeball the prompt + parse path. Verbose stderr.
//! - `run_audit(...)` is the threaded form used by the autonomous-mode loop.
//!   Returns a `Verdict`; appends a JSONL line to the audit log on every call.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde_json::Value;

use crate::{discovery, tmux, transcript};

const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a safety auditor for an autonomous coding agent.

The agent is paused on a yes/no permission prompt and is asking whether it can run a tool. Decide if the action is safe to AUTO-APPROVE without human review.

You will receive:
- Repo path the agent is working in
- Recent recap: a natural-language summary of recent work in the session (may be empty or stale; treat as best-available context, not authoritative)
- User intent: the most recent user prompt (may be a question, refinement, or directive — do NOT require it to be a green-light)
- Tool name + tool input: the specific action being requested

Decision policy:

APPROVE actions that are SAFE — meaning reversible AND scoped to the agent's repo, OR a routine read-only / inspection operation, OR a routine version-control operation (commit, branch, non-main push, PR view/create/edit, status, diff, log) — even when the user's exact intent is not spelled out in the most recent prompt. The recent recap establishes the work context; trust it. Examples that should APPROVE:
  - Read/Glob/Grep/Web-fetch
  - cargo/npm/pnpm/pip build, test, lint, format, run (anything compiling or testing inside the repo)
  - git status/diff/log/show/branch/checkout/add/commit, gh pr view/list/diff
  - gh pr create/edit, git push to non-main branches
  - File edits inside the repo

DENY actions that are clearly UNSAFE — destructive, exfiltrating, off-task, or out of scope:
  - rm -rf, dropping production tables, deleting branches, force-pushing to main
  - sending data to external endpoints not part of normal dev tooling
  - sudo, system-wide package installs, modifications to shared infrastructure
  - actions clearly inconsistent with the recent recap (e.g. a sql DROP when the recap is about UI work)

WAIT only when the ACTION ITSELF is in a genuine middle zone — not because the conversational context is incomplete. You will never see every prior message; do not WAIT just for that reason. Examples of legitimate WAIT:
  - First-time access to an unfamiliar external API or webhook
  - A `Bash` command with flags you cannot interpret confidently
  - A path outside the repo's working directory

Be confident on clear cases. The user opted into full APPROVE/DENY autonomy; over-WAITing defeats the point. Trust your judgment on safety; defer to humans only when the action genuinely warrants a second look.

Respond with EXACTLY two lines, no preamble, no trailing prose:
DECISION: APPROVE
REASON: <one sentence>

(Substitute APPROVE with DENY or WAIT as appropriate. The REASON line must be a single sentence.)"#;

/// Auditor session-name marker. The auditor passes `--name AUDITOR_NAME` to
/// claude; discovery filters sessions whose `name` matches so the auditor's
/// own Claude process never appears in triage's list (and never gets audited
/// recursively).
pub const AUDITOR_NAME: &str = "triage-auditor";

#[derive(Debug, Clone)]
pub struct Verdict {
    pub pid: u32,
    /// `APPROVE` | `DENY` | `WAIT` — `WAIT` is the safe default when parsing
    /// fails or the subprocess errors.
    pub decision: String,
    pub reason: String,
    /// Tool name we audited (echoed for logging / row annotation).
    pub tool_name: String,
    /// USD cost of this audit call as reported by `claude -p --output-format
    /// json` (`total_cost_usd`). None when the subprocess errored before
    /// emitting a result envelope (so cost wasn't billed either).
    pub cost_usd: Option<f64>,
    /// Wall-clock duration of the audit subprocess as reported by claude
    /// (`duration_ms`). Useful for spotting Sonnet rate-limit slowdowns.
    pub duration_ms: Option<u64>,
}

/// Resolve the system prompt at call time so users can iterate on it without
/// rebuilding. Resolution order:
///   1. `~/.config/triage/auditor-prompt.md` if present and non-empty
///   2. compiled-in `DEFAULT_SYSTEM_PROMPT`
///
/// Skip empty/whitespace-only files instead of silently passing them through.
/// An accidentally-empty file (e.g. `touch auditor-prompt.md`) would otherwise
/// wedge every audit with `API Error: 400 system: text content blocks must
/// contain non-whitespace text`.
fn load_system_prompt() -> (String, String) {
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(".config/triage/auditor-prompt.md");
        if let Ok(content) = std::fs::read_to_string(&p)
            && !content.trim().is_empty()
        {
            return (content, p.display().to_string());
        }
    }
    (
        DEFAULT_SYSTEM_PROMPT.to_string(),
        "built-in default".to_string(),
    )
}

/// Print the resolved system prompt and where it came from. `triage
/// --audit-prompt` is a zero-cost way to confirm what the auditor would feed
/// to claude before paying for a real call.
pub fn print_prompt() {
    let (prompt, source) = load_system_prompt();
    eprintln!("# source: {source}");
    println!("{prompt}");
}

/// Run the auditor against the given context and return a Verdict. Safe to
/// call from a worker thread. Always returns — failures collapse to
/// `decision = "WAIT"` with a reason describing the failure.
pub fn run_audit(
    pid: u32,
    cwd: &Path,
    recent_recap: Option<&str>,
    intent: &str,
    tool_name: &str,
    tool_input: &str,
) -> Verdict {
    let (system_prompt, _source) = load_system_prompt();
    let user_prompt = format!(
        "Repo:         {cwd}\n\
         Recent recap: {recap}\n\
         User intent:  {intent}\n\
         Tool:         {tool_name}\n\
         Tool input:\n{tool_input}\n",
        cwd = cwd.display(),
        recap = recent_recap.unwrap_or("(no recap available)"),
    );

    let raw_envelope = match run_claude(&system_prompt, &user_prompt) {
        Ok(out) => out,
        Err(e) => {
            let v = Verdict {
                pid,
                decision: "WAIT".to_string(),
                reason: format!("auditor subprocess failed: {e}"),
                tool_name: tool_name.to_string(),
                cost_usd: None,
                duration_ms: None,
            };
            let _ = append_audit_log(&v, cwd, intent, tool_input, "");
            return v;
        }
    };

    // `--output-format json` returns a single JSON envelope:
    //   {"type":"result","result":"DECISION: …\nREASON: …\n","total_cost_usd":0.024,"duration_ms":2072,...}
    // We parse for the user-visible fields (result text → DECISION/REASON)
    // and for cost/duration. If JSON parsing fails (e.g. claude printed an
    // error to stdout), we fall back to treating the whole stdout as the
    // text response so an out-of-band error message still surfaces.
    let envelope: Option<Value> = serde_json::from_str::<Value>(&raw_envelope).ok();
    let result_text = envelope
        .as_ref()
        .and_then(|v: &Value| v.get("result"))
        .and_then(|r: &Value| r.as_str())
        .map(|s: &str| s.to_string())
        .unwrap_or_else(|| raw_envelope.clone());
    let cost_usd = envelope
        .as_ref()
        .and_then(|v: &Value| v.get("total_cost_usd"))
        .and_then(|c: &Value| c.as_f64());
    let duration_ms = envelope
        .as_ref()
        .and_then(|v: &Value| v.get("duration_ms"))
        .and_then(|d: &Value| d.as_u64());
    let (decision, reason) = parse_verdict(&result_text);
    let v = Verdict {
        pid,
        decision,
        reason,
        tool_name: tool_name.to_string(),
        cost_usd,
        duration_ms,
    };
    let _ = append_audit_log(&v, cwd, intent, tool_input, &raw_envelope);
    v
}

fn run_claude(system_prompt: &str, user_prompt: &str) -> io::Result<String> {
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--no-session-persistence",
        "--model",
        "claude-sonnet-4-6",
        "--tools",
        "",
        // Per-call budget cap. $0.05 was exhausted on routine calls because
        // Sonnet's first turn pays for the (~2KB) system prompt + tool_input
        // before generating any output. $1.00 is comfortable headroom for one
        // call; the auditor only fires once per Blocked spell so total daily
        // cost is bounded by Blocked-event count, not by this cap.
        "--max-budget-usd",
        "1.00",
        "--output-format",
        "json",
        "--name",
        AUDITOR_NAME,
    ]);
    cmd.arg("--system-prompt").arg(system_prompt);
    cmd.arg(user_prompt);
    cmd.stdin(Stdio::null());
    let output = cmd.output()?;
    if !output.status.success() {
        // Budget-exceeded and similar errors print to stdout, not stderr, and
        // the exit message is empty. Include both streams so audit-log entries
        // are diagnosable.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = match (stderr.trim(), stdout.trim()) {
            (s, "") => s.to_string(),
            ("", o) => o.to_string(),
            (s, o) => format!("stderr={s}; stdout={o}"),
        };
        return Err(io::Error::other(format!(
            "claude exited {}: {}",
            output.status, detail
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_verdict(raw: &str) -> (String, String) {
    let decision = raw.lines().find_map(|l| {
        l.trim()
            .strip_prefix("DECISION:")
            .map(|v| v.trim().to_string())
    });
    let reason = raw
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("REASON:")
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    match decision.as_deref() {
        Some("APPROVE" | "DENY" | "WAIT") => (decision.unwrap(), reason),
        Some(other) => (
            "WAIT".to_string(),
            format!("auditor returned unexpected decision {other:?}, defaulting to WAIT"),
        ),
        None => (
            "WAIT".to_string(),
            "auditor response had no DECISION line, defaulting to WAIT".to_string(),
        ),
    }
}

fn audit_log_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/triage/auto-decisions.jsonl"))
}

fn append_audit_log(
    v: &Verdict,
    cwd: &Path,
    intent: &str,
    tool_input: &str,
    raw: &str,
) -> io::Result<()> {
    let Some(path) = audit_log_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // One JSONL line per verdict. Include enough context that the user can
    // audit decisions later without cross-referencing other files.
    let entry = serde_json::json!({
        "ts": ts,
        "pid": v.pid,
        "cwd": cwd.display().to_string(),
        "intent": intent,
        "tool": v.tool_name,
        "tool_input": truncate(tool_input, 1000),
        "decision": v.decision,
        "reason": v.reason,
        // `total_cost_usd` and `duration_ms` from claude's JSON envelope.
        // Null on subprocess failures (no billing happened).
        "cost_usd": v.cost_usd,
        "duration_ms": v.duration_ms,
        "raw": truncate(raw, 1000),
    });
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{entry}")?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// One-shot CLI: `triage --audit <pid>`. Verbose for eyeballing the prompt /
/// parse path. Uses the same `run_audit` core as the autonomous loop.
pub fn audit(pid: u32) -> io::Result<()> {
    let mut sessions = discovery::discover_live_sessions();
    let panes = tmux::list_panes();
    let ppid_map = tmux::build_ppid_map();
    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, &ppid_map, 8);
    }
    let mut cache = transcript::DigestCache::new();
    transcript::assign_transcripts(&mut sessions, &mut cache);
    let now = SystemTime::now();
    for s in &mut sessions {
        transcript::enrich(s, now, &mut cache);
    }
    for s in &mut sessions {
        if s.status == "waiting"
            && s.last_tool_use.is_none()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane(&pane.target)
            && let Some(brief) = tmux::parse_pending_brief(&content)
        {
            let name = s
                .waiting_for
                .as_deref()
                .and_then(|w| w.strip_prefix("approve "))
                .unwrap_or("?")
                .to_string();
            s.last_tool_use = Some((name, brief));
        }
    }

    let s = sessions
        .iter()
        .find(|s| s.pid == pid)
        .ok_or_else(|| io::Error::other(format!("no live session with pid {pid}")))?;
    let (tool_name, tool_input) = s.last_tool_use.clone().ok_or_else(|| {
        io::Error::other(format!(
            "session {pid} has no captured tool_use to audit (status={}, waitingFor={:?})",
            s.status, s.waiting_for
        ))
    })?;
    let intent = s.last_prompt.as_deref().unwrap_or("(unknown)").trim();
    let recap = s.headline.as_deref();

    let (_, source) = load_system_prompt();
    eprintln!("=== auditor inputs ===");
    eprintln!("repo:   {}", s.cwd.display());
    eprintln!("recap:  {}", recap.unwrap_or("(no recap available)"));
    eprintln!("intent: {intent}");
    eprintln!("tool:   {tool_name}");
    eprintln!("input:  {tool_input}");
    eprintln!("=== system prompt source: {source} ===");
    eprintln!("=== spawning claude -p (sonnet, --name {AUDITOR_NAME}, no tools) ===");

    let v = run_audit(pid, &s.cwd, recap, intent, &tool_name, &tool_input);
    println!("DECISION: {}", v.decision);
    println!("REASON:   {}", v.reason);
    Ok(())
}
