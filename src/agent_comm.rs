use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::models::{AttentionState, Provider, Session, session_display_label};
use crate::persist::{self, AliasKey};
use crate::{codex, snapshot, tmux, transcript};

const MAX_MESSAGE_CHARS: usize = 8000;

pub fn cli_agents(args: &[String]) -> i32 {
    match run_agents(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", e.message);
            e.code
        }
    }
}

pub fn cli_send(args: &[String]) -> i32 {
    match run_send(args) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("{}", e.message);
            e.code
        }
    }
}

#[derive(Debug)]
struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    fn usage(msg: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: msg.into(),
        }
    }

    fn denied(msg: impl Into<String>) -> Self {
        Self {
            code: 3,
            message: format!("denied: {}", msg.into()),
        }
    }

    fn delivery(msg: impl Into<String>) -> Self {
        Self {
            code: 4,
            message: msg.into(),
        }
    }

    fn runtime(msg: impl Into<String>) -> Self {
        Self {
            code: 1,
            message: msg.into(),
        }
    }
}

#[derive(Default)]
struct AgentsArgs {
    json: bool,
    provider: Option<String>,
    cwd: Option<PathBuf>,
    include_self: bool,
}

#[derive(Default)]
struct SendArgs {
    to: Option<String>,
    from: Option<String>,
    message: Option<String>,
    file: Option<PathBuf>,
    stdin: bool,
    positional: Vec<String>,
    dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
struct AgentRow {
    id: String,
    provider: String,
    name: String,
    cwd: String,
    state: String,
    can_receive: bool,
    deny_reason: Option<String>,
    pane_target: Option<String>,
    pane_id: Option<String>,
    session_id: String,
    updated_at_ms: u64,
    headline: Option<String>,
}

fn run_agents(args: &[String]) -> Result<(), CliError> {
    // `triage agents whoami [--json]` — introspect the caller's own row, which
    // the plain listing deliberately omits. Lets an agent learn how triage
    // sees it (pane id/target to use as a `--from` token, resolved name,
    // state) rather than just the bare $TMUX_PANE. Checked before the shared
    // --help so `agents whoami --help` reaches the subcommand's own usage.
    if args.first().map(String::as_str) == Some("whoami") {
        return run_whoami(&args[1..]);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", agents_usage(""));
        return Ok(());
    }
    let args = parse_agents_args(args)?;
    let mut sessions = load_snapshot()?;
    snapshot::sort_sessions(&mut sessions);
    let current_pane = (!args.include_self).then(current_tmux_pane_id).flatten();

    let rows = sessions
        .iter()
        .filter(|s| {
            current_pane
                .as_deref()
                .is_none_or(|pane_id| s.pane.as_ref().is_none_or(|p| p.pane_id != pane_id))
        })
        .filter(|s| {
            args.provider
                .as_deref()
                .is_none_or(|p| provider_matches(s.provider, p))
        })
        .filter(|s| args.cwd.as_ref().is_none_or(|cwd| &s.cwd == cwd))
        .map(agent_row)
        .collect::<Vec<_>>();

    if args.json {
        let json = serde_json::to_string_pretty(&rows)
            .map_err(|e| CliError::usage(format!("failed to render JSON: {e}")))?;
        println!("{json}");
    } else {
        for row in rows {
            let recv = if row.can_receive {
                "can-receive".to_string()
            } else {
                format!(
                    "blocked: {}",
                    row.deny_reason.as_deref().unwrap_or("cannot receive")
                )
            };
            println!(
                "{:<6} {:<2} {:<8} {:<24} {}",
                row.id, row.provider, row.state, row.name, recv
            );
            println!("       cwd: {}", row.cwd);
            if let Some(headline) = row.headline {
                println!(
                    "       headline: {}",
                    truncate_chars(&headline.replace('\n', " "), 100)
                );
            }
        }
    }

    Ok(())
}

fn run_whoami(args: &[String]) -> Result<(), CliError> {
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!("usage: triage agents whoami [--json]");
                return Ok(());
            }
            other => {
                return Err(CliError::usage(format!(
                    "unknown arg {other:?}\nusage: triage agents whoami [--json]"
                )));
            }
        }
    }

    let pane_id = current_tmux_pane_id()
        .ok_or_else(|| CliError::usage("not running inside a tmux pane (TMUX_PANE is unset)"))?;

    // The caller's own session is the one paired to this pane.
    let mut sessions = load_snapshot()?;
    snapshot::sort_sessions(&mut sessions);
    let row = sessions
        .iter()
        .find(|s| s.pane.as_ref().is_some_and(|p| p.pane_id == pane_id))
        .map(agent_row);

    if json {
        let value = match &row {
            Some(row) => serde_json::to_value(row),
            // Pane is real but triage doesn't track an agent session here
            // (e.g. a plain shell, or a session it couldn't pair). Still report
            // the pane so the caller has a usable identity.
            None => serde_json::to_value(serde_json::json!({
                "pane_id": pane_id,
                "tracked": false,
            })),
        }
        .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?
        );
        return Ok(());
    }

    match row {
        Some(row) => {
            let target = row.pane_target.as_deref().unwrap_or("?");
            println!(
                "pane:     {} ({})",
                row.pane_id.as_deref().unwrap_or(&pane_id),
                target
            );
            println!("agent:    {} {} {:?}", row.provider, row.state, row.name);
            println!("cwd:      {}", row.cwd);
            println!("session:  {}", row.session_id);
            if let Some(headline) = row.headline {
                println!(
                    "headline: {}",
                    truncate_chars(&headline.replace('\n', " "), 100)
                );
            }
        }
        None => {
            println!("pane:     {pane_id}");
            println!("(no agent session tracked on this pane)");
        }
    }
    Ok(())
}

fn run_send(args: &[String]) -> Result<String, CliError> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(send_usage(""));
    }
    let args = parse_send_args(args)?;
    let selector = args
        .to
        .as_deref()
        .ok_or_else(|| CliError::usage(send_usage("missing --to")))?;
    let sender = resolve_sender(args.from.clone())?;
    let body = read_message_body(&args)?;
    let body = validate_body(&body)?;
    let formatted = format_message(&sender, &body);

    deliver_message(selector, &sender, &formatted, args.dry_run)
}

pub fn send_user_reply(selector: &str, body: &str) -> Result<String, String> {
    let body = format_user_reply(body).map_err(|e| e.message)?;
    deliver_message(selector, "user", &body, false).map_err(|e| e.message)
}

fn format_user_reply(body: &str) -> Result<String, CliError> {
    let body = validate_body(body)?;
    if body.contains('\n') {
        return Err(CliError::usage("reply must be a single line"));
    }
    Ok(body)
}

fn deliver_message(
    selector: &str,
    sender: &str,
    message: &str,
    dry_run: bool,
) -> Result<String, CliError> {
    let sessions = load_snapshot()?;
    let target = resolve_target(&sessions, selector)?;
    let gate = evaluate_send_gate(target);
    if !gate.can_send {
        let _ = append_message_audit(&AuditEntry::denied(sender, selector, target, &gate.reason));
        return Err(CliError::denied(gate.reason));
    }

    if dry_run {
        return Ok(format!(
            "dry-run: would send to {} ({})",
            target_id(target),
            target_label(target)
        ));
    }

    let pane = target
        .pane
        .as_ref()
        .ok_or_else(|| CliError::denied("target has no tmux pane"))?;
    tmux::paste_text_and_enter(&pane.pane_id, message)
        .map_err(|e| CliError::delivery(format!("send failed: {e}")))?;

    if let Err(e) = append_message_audit(&AuditEntry::sent(sender, selector, target, message)) {
        eprintln!("warning: failed to append agent-message audit: {e}");
    }

    let suffix = if target.state == AttentionState::Working {
        "; target is Working, input queued by terminal"
    } else {
        ""
    };
    Ok(format!(
        "sent to {} ({}{})",
        target_id(target),
        target_label(target),
        suffix
    ))
}

fn parse_agents_args(args: &[String]) -> Result<AgentsArgs, CliError> {
    let mut out = AgentsArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => out.json = true,
            "--include-self" => out.include_self = true,
            "--provider" => {
                i += 1;
                out.provider = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::usage(agents_usage("missing --provider value")))?
                        .clone(),
                );
            }
            "--cwd" => {
                i += 1;
                out.cwd =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        CliError::usage(agents_usage("missing --cwd value"))
                    })?));
            }
            "--help" | "-h" => return Err(CliError::usage(agents_usage(""))),
            other => {
                return Err(CliError::usage(agents_usage(format!(
                    "unknown arg {other:?}"
                ))));
            }
        }
        i += 1;
    }
    Ok(out)
}

fn parse_send_args(args: &[String]) -> Result<SendArgs, CliError> {
    let mut out = SendArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                i += 1;
                out.to = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::usage(send_usage("missing --to value")))?
                        .clone(),
                );
            }
            "--from" => {
                i += 1;
                out.from = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::usage(send_usage("missing --from value")))?
                        .clone(),
                );
            }
            "--message" | "-m" => {
                i += 1;
                out.message = Some(
                    args.get(i)
                        .ok_or_else(|| CliError::usage(send_usage("missing --message value")))?
                        .clone(),
                );
            }
            "--file" | "-f" => {
                i += 1;
                out.file =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        CliError::usage(send_usage("missing --file value"))
                    })?));
            }
            "--dry-run" => out.dry_run = true,
            "--help" | "-h" => return Err(CliError::usage(send_usage(""))),
            "-" => out.stdin = true,
            other if other.starts_with('-') => {
                return Err(CliError::usage(send_usage(format!(
                    "unknown arg {other:?}"
                ))));
            }
            other => out.positional.push(other.to_string()),
        }
        i += 1;
    }
    Ok(out)
}

fn load_snapshot() -> Result<Vec<Session>, CliError> {
    let panes = tmux::list_panes_checked()
        .map_err(|e| CliError::runtime(format!("tmux discovery unavailable: {e}")))?;
    let loaded = persist::load_state();
    let aliases: HashMap<AliasKey, String> = loaded.aliases.into_iter().collect();
    let mut digest_cache = transcript::DigestCache::new();
    let mut codex_cache = codex::CodexDigestCache::new();
    Ok(snapshot::discover_sessions_with_panes(
        SystemTime::now(),
        &mut digest_cache,
        &mut codex_cache,
        &aliases,
        panes,
    ))
}

fn resolve_target<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session, CliError> {
    let matches = sessions
        .iter()
        .filter(|s| selector_matches(s, selector))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(CliError::usage(format!(
            "target {selector:?} matched no agents"
        ))),
        many => {
            let ids = many.iter().map(|s| target_id(s)).collect::<Vec<_>>();
            Err(CliError::usage(format!(
                "target {selector:?} matched {} agents; use pane_id: {}",
                many.len(),
                ids.join(", ")
            )))
        }
    }
}

fn selector_matches(s: &Session, selector: &str) -> bool {
    if let Some(pane) = &s.pane
        && (pane.pane_id == selector || pane.target == selector)
    {
        return true;
    }
    if format!("{}:{}", s.provider.label(), s.session_id) == selector {
        return true;
    }
    s.name.as_deref() == Some(selector) || session_display_label(s) == selector
}

#[derive(Debug, Clone)]
struct GateResult {
    can_send: bool,
    reason: String,
}

fn evaluate_send_gate(s: &Session) -> GateResult {
    let Some(pane) = &s.pane else {
        return blocked("target has no tmux pane");
    };

    if s.provider == Provider::Claude && s.status == "waiting" {
        return blocked("target is waiting on a Claude permission prompt");
    }
    if s.pane_blocked {
        return blocked("target has a visible permission prompt");
    }
    // Capture WITH ANSI styling: the draft-input check needs it to tell
    // Claude's faint ghost/placeholder text from real input. The plain-text
    // permission matchers run on a stripped copy.
    if let Some(raw) = tmux::capture_pane_tail_ansi(&pane.pane_id, 80) {
        let plain = tmux::strip_ansi(&raw);
        if tmux::has_pending_permission_prompt(&plain) || tmux::has_codex_permission_prompt(&plain)
        {
            return blocked("target has a visible permission prompt");
        }
        // Real (non-faint) text in the composer means the user is mid-typing —
        // a paste would land on their draft and submit the mangled result.
        if tmux::has_draft_input(&raw) {
            return blocked("target has unsent text in its input box (user may be typing)");
        }
    }

    // Everything past the prompt checks is reachable. The only genuine
    // "do not send" conditions are the two above — no pane to paste into, and
    // a visible permission prompt (our keystrokes would answer it). Attention
    // state does NOT gate delivery: Working queues input; Stale is just a
    // >=24h-idle heuristic (a send wakes a long-idle-but-alive agent rather
    // than failing); Error/Unknown sit at a normal prompt and take input fine.
    GateResult {
        can_send: true,
        reason: String::new(),
    }
}

fn blocked(reason: &str) -> GateResult {
    GateResult {
        can_send: false,
        reason: reason.to_string(),
    }
}

fn agent_row(s: &Session) -> AgentRow {
    let gate = evaluate_send_gate(s);
    AgentRow {
        id: target_id(s),
        provider: s.provider.label().to_string(),
        name: session_display_label(s),
        cwd: s.cwd.display().to_string(),
        state: attention_state_name(s.state).to_string(),
        can_receive: gate.can_send,
        deny_reason: (!gate.can_send).then_some(gate.reason),
        pane_target: s.pane.as_ref().map(|p| p.target.clone()),
        pane_id: s.pane.as_ref().map(|p| p.pane_id.clone()),
        session_id: s.session_id.clone(),
        updated_at_ms: s.updated_at_ms,
        headline: s.headline.clone().or_else(|| s.last_prompt.clone()),
    }
}

fn provider_matches(provider: Provider, value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    match provider {
        Provider::Claude => matches!(value.as_str(), "cc" | "claude" | "claude-code"),
        Provider::Codex => matches!(value.as_str(), "cx" | "codex"),
    }
}

fn target_id(s: &Session) -> String {
    s.pane
        .as_ref()
        .map(|p| p.pane_id.clone())
        .unwrap_or_else(|| format!("{}:{}", s.provider.label(), s.session_id))
}

fn target_label(s: &Session) -> String {
    format!("{} {}", s.provider.label(), session_display_label(s))
}

fn attention_state_name(state: AttentionState) -> &'static str {
    match state {
        AttentionState::Error => "Error",
        AttentionState::Blocked => "Blocked",
        AttentionState::JustFinished => "JustFinished",
        AttentionState::Working => "Working",
        AttentionState::Fresh => "Fresh",
        AttentionState::IdleShort => "IdleShort",
        AttentionState::IdleLong => "IdleLong",
        AttentionState::Stale => "Stale",
        AttentionState::Unknown => "Unknown",
    }
}

fn read_message_body(args: &SendArgs) -> Result<String, CliError> {
    let source_count = args.message.is_some() as usize
        + args.file.is_some() as usize
        + args.stdin as usize
        + (!args.positional.is_empty()) as usize;
    if source_count == 0 {
        return Err(CliError::usage(send_usage("missing message body")));
    }
    if source_count > 1 {
        return Err(CliError::usage(send_usage(
            "choose only one message source: --message, --file, stdin '-', or positional text",
        )));
    }
    if let Some(message) = &args.message {
        return Ok(message.clone());
    }
    if let Some(file) = &args.file {
        return fs::read_to_string(file)
            .map_err(|e| CliError::usage(format!("failed to read {}: {e}", file.display())));
    }
    if args.stdin {
        let mut body = String::new();
        io::stdin()
            .read_to_string(&mut body)
            .map_err(|e| CliError::usage(format!("failed to read stdin: {e}")))?;
        return Ok(body);
    }
    Ok(args.positional.join(" "))
}

fn validate_body(body: &str) -> Result<String, CliError> {
    let body = body.trim_end_matches('\n').to_string();
    if body.trim().is_empty() {
        return Err(CliError::usage("message body is empty"));
    }
    if body.chars().count() > MAX_MESSAGE_CHARS {
        return Err(CliError::usage(format!(
            "message body exceeds {MAX_MESSAGE_CHARS} characters"
        )));
    }
    for c in body.chars() {
        if c == '\n' || c == '\t' || !c.is_control() {
            continue;
        }
        return Err(CliError::usage(format!(
            "message body contains unsupported control character U+{:04X}",
            c as u32
        )));
    }
    Ok(body)
}

fn format_message(sender: &str, body: &str) -> String {
    if body.contains('\n') {
        format!("[triage message from {sender}]\n{body}")
    } else {
        format!("[triage message from {sender}] {body}")
    }
}

fn resolve_sender(explicit: Option<String>) -> Result<String, CliError> {
    let raw = explicit
        .or_else(|| std::env::var("TRIAGE_AGENT").ok())
        .or_else(current_tmux_window_name)
        .unwrap_or_else(|| "unknown".to_string());
    let sender = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if sender.is_empty() {
        return Err(CliError::usage("sender is empty"));
    }
    validate_sender(&sender)?;
    Ok(sender)
}

fn validate_sender(sender: &str) -> Result<(), CliError> {
    if sender.chars().count() > 80 {
        return Err(CliError::usage("sender is too long"));
    }
    for c in sender.chars() {
        if !c.is_control() {
            continue;
        }
        return Err(CliError::usage(format!(
            "sender contains unsupported control character U+{:04X}",
            c as u32
        )));
    }
    Ok(())
}

fn current_tmux_window_name() -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#W"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn current_tmux_pane_id() -> Option<String> {
    std::env::var("TMUX_PANE")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            let out = Command::new("tmux")
                .args(["display-message", "-p", "#{pane_id}"])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!pane.is_empty()).then_some(pane)
        })
}

#[derive(Serialize)]
struct AuditEntry {
    ts: u64,
    from: String,
    selector: String,
    target_id: String,
    target_provider: String,
    target_name: String,
    target_cwd: String,
    verdict: String,
    deny_reason: Option<String>,
    message_preview: Option<String>,
    message_len: Option<usize>,
}

impl AuditEntry {
    fn sent(sender: &str, selector: &str, target: &Session, message: &str) -> Self {
        Self::new(sender, selector, target, "sent", None, Some(message))
    }

    fn denied(sender: &str, selector: &str, target: &Session, reason: &str) -> Self {
        Self::new(sender, selector, target, "denied", Some(reason), None)
    }

    fn new(
        sender: &str,
        selector: &str,
        target: &Session,
        verdict: &str,
        deny_reason: Option<&str>,
        message: Option<&str>,
    ) -> Self {
        Self {
            ts: unix_secs(),
            from: sender.to_string(),
            selector: selector.to_string(),
            target_id: target_id(target),
            target_provider: target.provider.label().to_string(),
            target_name: session_display_label(target),
            target_cwd: target.cwd.display().to_string(),
            verdict: verdict.to_string(),
            deny_reason: deny_reason.map(str::to_string),
            message_preview: message.map(|m| truncate_chars(&m.replace('\n', " "), 120)),
            message_len: message.map(|m| m.chars().count()),
        }
    }
}

fn append_message_audit(entry: &AuditEntry) -> io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let path = PathBuf::from(home).join(".config/triage/agent-messages.jsonl");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = s.chars().take(max).collect::<String>();
    out.push_str("...");
    out
}

fn agents_usage(prefix: impl Into<String>) -> String {
    let prefix = prefix.into();
    let usage = "usage: triage agents [--json] [--include-self] [--provider cc|cx] [--cwd PATH]\n       triage agents whoami [--json]";
    if prefix.is_empty() {
        usage.to_string()
    } else {
        format!("{prefix}\n{usage}")
    }
}

fn send_usage(prefix: impl Into<String>) -> String {
    let prefix = prefix.into();
    let usage = "usage: triage send --to TARGET [--from NAME] (--message TEXT | --file PATH | - | TEXT...) [--dry-run]";
    if prefix.is_empty() {
        usage.to_string()
    } else {
        format!("{prefix}\n{usage}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Pane, Provider, Session};

    fn session(state: AttentionState) -> Session {
        let mut s = Session::new(
            Provider::Claude,
            123,
            "sid".to_string(),
            PathBuf::from("/tmp/project"),
            Some("target".to_string()),
            "idle".to_string(),
            0,
            0,
            None,
        );
        s.state = state;
        s.pane = Some(Pane {
            target: "main:1.0".to_string(),
            tmux_session: "main".to_string(),
            window_name: "target".to_string(),
            pane_id: "%42".to_string(),
            pid: 100,
            tty: "/dev/ttys001".to_string(),
            current_command: "claude".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            active: false,
        });
        s
    }

    #[test]
    fn working_without_prompt_is_allowed() {
        let s = session(AttentionState::Working);

        let gate = evaluate_send_gate(&s);

        assert!(gate.can_send);
    }

    #[test]
    fn visible_prompt_is_denied() {
        // pane_blocked is the real Blocked trigger — a permission prompt is up,
        // so keystrokes would answer it.
        let mut s = session(AttentionState::Blocked);
        s.pane_blocked = true;

        let gate = evaluate_send_gate(&s);

        assert!(!gate.can_send);
        assert_eq!(gate.reason, "target has a visible permission prompt");
    }

    #[test]
    fn stale_is_allowed() {
        // Stale is a >=24h-idle heuristic, not an unreachability signal: a
        // long-idle-but-alive agent must remain sendable (a send wakes it).
        let gate = evaluate_send_gate(&session(AttentionState::Stale));
        assert!(gate.can_send);
        assert!(gate.reason.is_empty());
    }

    #[test]
    fn error_and_unknown_are_allowed() {
        // No prompt is up in these states — the pane sits at a normal prompt
        // and takes queued input fine.
        assert!(evaluate_send_gate(&session(AttentionState::Error)).can_send);
        assert!(evaluate_send_gate(&session(AttentionState::Unknown)).can_send);
    }

    #[test]
    fn no_pane_is_denied() {
        let mut s = session(AttentionState::IdleShort);
        s.pane = None;

        let gate = evaluate_send_gate(&s);

        assert!(!gate.can_send);
        assert_eq!(gate.reason, "target has no tmux pane");
    }

    #[test]
    fn waiting_status_is_denied_even_if_state_allowed() {
        let mut s = session(AttentionState::IdleShort);
        s.status = "waiting".to_string();

        let gate = evaluate_send_gate(&s);

        assert!(!gate.can_send);
        assert_eq!(
            gate.reason,
            "target is waiting on a Claude permission prompt"
        );
    }

    #[test]
    fn ambiguous_name_is_rejected() {
        let a = session(AttentionState::IdleShort);
        let mut b = session(AttentionState::IdleShort);
        b.pid = 456;
        b.session_id = "sid2".to_string();
        b.pane.as_mut().unwrap().pane_id = "%43".to_string();

        let err = resolve_target(&[a, b], "target").unwrap_err();

        assert_eq!(err.code, 2);
        assert!(err.message.contains("matched 2 agents"));
    }

    #[test]
    fn message_validation_allows_multiline_and_rejects_escape() {
        assert_eq!(validate_body("hello\nthere").unwrap(), "hello\nthere");

        let err = validate_body("hello\u{1b}").unwrap_err();

        assert_eq!(err.code, 2);
        assert!(err.message.contains("U+001B"));
    }

    #[test]
    fn multiline_format_puts_prefix_on_own_line() {
        let formatted = format_message("TRI-112", "hello\nthere");

        assert_eq!(formatted, "[triage message from TRI-112]\nhello\nthere");
    }

    #[test]
    fn user_reply_keeps_raw_text_without_peer_prefix() {
        let formatted = format_user_reply("hello agent\n").unwrap();

        assert_eq!(formatted, "hello agent");
    }

    #[test]
    fn user_reply_rejects_multiline_body() {
        let err = format_user_reply("hello\nthere").unwrap_err();

        assert_eq!(err.code, 2);
        assert_eq!(err.message, "reply must be a single line");
    }
}
