use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::{Config, SendMode};
use crate::mailbox::{self, Claim, DeliveredVia, HostId, Message, RenderItem, State, Store};
use crate::models::{AttentionState, Provider, Session, session_display_label};
use crate::persist::{self, AliasKey};
use crate::{classifier, codex, snapshot, tmux, transcript};

const MAX_MESSAGE_CHARS: usize = 8000;

pub fn cli_agents(args: &[String]) -> i32 {
    match run_agents(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", with_sandbox_hint(&e.message));
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
            eprintln!("{}", with_sandbox_hint(&e.message));
            e.code
        }
    }
}

pub fn cli_inbox(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--hook") => return crate::peer_hooks::cli(&args[1..]),
        Some("--helper") => return crate::transport::cli_helper(&args[1..]),
        _ => {}
    }
    match run_inbox(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", with_sandbox_hint(&e.message));
            e.code
        }
    }
}

pub fn cli_interrupt(args: &[String]) -> i32 {
    match run_interrupt(args) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("{}", with_sandbox_hint(&error.message));
            error.code
        }
    }
}

const INTERRUPT_USAGE: &str = "usage: triage interrupt --to TARGET [--dry-run]";

fn run_interrupt(args: &[String]) -> Result<String, CliError> {
    let (selector, dry_run) = parse_interrupt_args(args)?;
    let Some(selector) = selector else {
        return Ok(INTERRUPT_USAGE.to_string());
    };
    let sessions = load_snapshot()?;
    let initial = resolve_target(&sessions, &selector)?;
    interrupt_eligibility(initial, SystemTime::now()).map_err(CliError::denied)?;
    let stamp = transcript_stamp(initial)?;
    let pane_id = target_id(initial);

    let fresh = load_snapshot()?;
    let target = resolve_target(&fresh, &pane_id)?;
    if target.session_id != initial.session_id || target.pid != initial.pid {
        return Err(CliError::denied(
            "target session changed during interrupt checks",
        ));
    }
    interrupt_eligibility(target, SystemTime::now()).map_err(CliError::denied)?;
    let gate = evaluate_send_gate_with_capture(target, true);
    if !gate.can_send {
        return Err(CliError::denied(gate.reason));
    }
    if has_live_children(target)? {
        return Err(CliError::denied(
            "target has live child processes; tool activity may still be running",
        ));
    }
    if transcript_stamp(target)? != stamp {
        return Err(CliError::denied(
            "target transcript changed during interrupt checks; retry after inspecting progress",
        ));
    }
    if dry_run {
        return Ok(format!(
            "dry-run: would interrupt {} ({}) with Escape",
            pane_id,
            target_label(target)
        ));
    }
    tmux::send_keys(&pane_id, &["Escape"])
        .map_err(|error| CliError::delivery(format!("interrupt failed: {error}")))?;
    if let Err(error) = append_message_audit(&AuditEntry::new(
        "interrupt",
        &selector,
        target,
        "interrupted",
        None,
        None,
    )) {
        eprintln!("warning: failed to append interrupt audit: {error}");
    }
    Ok(format!(
        "interrupt sent to {pane_id}; queued input processing is not confirmed"
    ))
}

fn parse_interrupt_args(args: &[String]) -> Result<(Option<String>, bool), CliError> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Ok((None, false));
    }
    let mut selector = None;
    let mut dry_run = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--to" if selector.is_none() => {
                selector = Some(
                    args.next()
                        .filter(|value| !value.starts_with('-'))
                        .ok_or_else(|| CliError::usage(INTERRUPT_USAGE))?
                        .clone(),
                );
            }
            "--dry-run" => dry_run = true,
            _ => return Err(CliError::usage(INTERRUPT_USAGE)),
        }
    }
    if selector.is_none() {
        return Err(CliError::usage(INTERRUPT_USAGE));
    }
    Ok((selector, dry_run))
}

fn interrupt_eligibility(session: &Session, now: SystemTime) -> Result<(), &'static str> {
    if classifier::no_progress_age(session, now).is_none() {
        return Err(
            "requires an active Codex turn with no observable progress for at least 15 minutes",
        );
    }
    if session.last_tool_use.is_some() {
        return Err(
            "target has an unfinished tool call; quiet tools are not proof of a stalled turn",
        );
    }
    Ok(())
}

fn transcript_stamp(session: &Session) -> Result<(SystemTime, u64), CliError> {
    let path = session
        .transcript_path
        .as_ref()
        .ok_or_else(|| CliError::denied("target has no readable transcript"))?;
    let metadata = fs::metadata(path).map_err(|error| CliError::runtime(error.to_string()))?;
    Ok((
        metadata
            .modified()
            .map_err(|error| CliError::runtime(error.to_string()))?,
        metadata.len(),
    ))
}

fn has_live_children(session: &Session) -> Result<bool, CliError> {
    let output = Command::new("ps")
        .args(["-ww", "-A", "-o", "pid=,ppid=,stat=,etime=,args="])
        .output()
        .map_err(|error| CliError::runtime(format!("interrupt process check failed: {error}")))?;
    if !output.status.success() {
        return Err(CliError::runtime(format!(
            "interrupt process check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let quiet_age = classifier::no_progress_age(session, SystemTime::now())
        .ok_or_else(|| CliError::denied("target is no longer eligible for interrupt"))?;
    live_children_in_ps(
        session.pid,
        quiet_age.as_secs(),
        &String::from_utf8_lossy(&output.stdout),
    )
    .map_err(CliError::runtime)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BaselineService {
    MemoryLauncher,
    PlaywrightLauncher,
    SalesforceLauncher,
    Worker,
}

fn baseline_service(command: &[&str], parent: Option<BaselineService>) -> Option<BaselineService> {
    use BaselineService::*;
    let executable = std::path::Path::new(*command.first()?)
        .file_name()?
        .to_str()?;
    let args = &command[1..];
    match (parent, executable, args) {
        (None, "codex-code-mode-host", []) => Some(Worker),
        (
            None,
            "uv",
            [
                "tool",
                "uvx",
                "--from",
                "git+ssh://git@github.com/Affirm/ai-memory-bank-mcp",
                "mcp_memory_bank_setup",
            ],
        ) => Some(MemoryLauncher),
        (None, "npm", ["exec", package]) if package.starts_with("@playwright/mcp@") => {
            Some(PlaywrightLauncher)
        }
        (
            None,
            "npm",
            [
                "exec",
                package,
                "--orgs",
                "DEFAULT_TARGET_ORG",
                "--toolsets",
                "orgs,metadata,data,users",
            ],
        ) if package.starts_with("@salesforce/mcp@") => Some(SalesforceLauncher),
        (Some(MemoryLauncher), "python" | "python3", [script])
            if std::path::Path::new(script)
                .file_name()
                .is_some_and(|name| name == "mcp_memory_bank_setup") =>
        {
            Some(Worker)
        }
        (Some(PlaywrightLauncher), "node", [script])
            if script.ends_with("/node_modules/.bin/playwright-mcp") =>
        {
            Some(Worker)
        }
        (
            Some(SalesforceLauncher),
            "node",
            [
                script,
                "--orgs",
                "DEFAULT_TARGET_ORG",
                "--toolsets",
                "orgs,metadata,data,users",
            ],
        ) if script.ends_with("/node_modules/.bin/sf-mcp-server") => Some(Worker),
        _ => None,
    }
}

fn elapsed_seconds(raw: &str) -> Option<u64> {
    let (days, time) = match raw.split_once('-') {
        Some((days, time)) => (days.parse::<u64>().ok()?, time),
        None => (0, raw),
    };
    let parts = time
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] if days == 0 => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => return None,
    };
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    days.checked_mul(24)?
        .checked_add(hours)?
        .checked_mul(60)?
        .checked_add(minutes)?
        .checked_mul(60)?
        .checked_add(seconds)
}

fn live_children_in_ps(pid: u32, quiet_seconds: u64, text: &str) -> Result<bool, &'static str> {
    struct Process<'a> {
        pid: u32,
        parent: u32,
        state: &'a str,
        age: u64,
        command: Vec<&'a str>,
    }
    let mut processes = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 5 {
            return Err("interrupt process check returned malformed output");
        }
        processes.push(Process {
            pid: fields[0].parse().map_err(|_| "invalid process id")?,
            parent: fields[1].parse().map_err(|_| "invalid parent process id")?,
            state: fields[2],
            age: elapsed_seconds(fields[3]).ok_or("invalid process elapsed time")?,
            command: fields[4..].to_vec(),
        });
    }
    let root = processes
        .iter()
        .find(|process| process.pid == pid && !process.state.starts_with('Z'))
        .ok_or("interrupt target process is no longer live")?;
    let mut queue = vec![(pid, None)];
    let mut visited = std::collections::HashSet::from([pid]);
    while let Some((parent_pid, parent_service)) = queue.pop() {
        for process in processes
            .iter()
            .filter(|process| process.parent == parent_pid)
        {
            if !visited.insert(process.pid) {
                return Err("interrupt process tree contains a cycle");
            }
            if process.state.starts_with('Z') {
                queue.push((process.pid, Some(BaselineService::Worker)));
                continue;
            }
            // Only sleeping, recognized startup services qualify. Check every
            // descendant: a baseline MCP server can still launch a task/browser.
            let startup_service = process.state.starts_with('S')
                && root
                    .age
                    .checked_sub(process.age)
                    .is_some_and(|delta| delta <= 30)
                && process.age > quiet_seconds.saturating_add(2);
            let service = startup_service
                .then(|| baseline_service(&process.command, parent_service))
                .flatten();
            let Some(service) = service else {
                return Ok(true);
            };
            queue.push((process.pid, Some(service)));
        }
    }
    Ok(false)
}

/// Inside a sandbox (Codex's, typically) triage can neither list processes
/// nor write its state dir; say how to get out rather than leave an agent to
/// guess.
fn with_sandbox_hint(message: &str) -> String {
    if message.contains("Operation not permitted") {
        format!(
            "{message}\nhint: this looks like a sandbox blocking triage; run the command outside it (in Codex, request escalated permissions)"
        )
    } else {
        message.to_string()
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

    fn wait_timeout(msg: impl Into<String>) -> Self {
        Self {
            code: 5,
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
    message: Option<String>,
    file: Option<PathBuf>,
    stdin: bool,
    positional: Vec<String>,
    dry_run: bool,
    mode: Option<SendMode>,
    wait_secs: Option<u64>,
}

const DEFAULT_WAIT_SECS: u64 = 60;

#[derive(Debug, Clone, Serialize)]
struct AgentRow {
    id: String,
    agent_id: Option<String>,
    provider: String,
    name: String,
    cwd: String,
    state: String,
    can_receive: bool,
    no_progress_seconds: Option<u64>,
    pending_tool: bool,
    deny_reason: Option<String>,
    pane_target: Option<String>,
    pane_id: Option<String>,
    session_id: String,
    updated_at_ms: u64,
    active_background_jobs: usize,
    headline: Option<String>,
}

fn run_agents(args: &[String]) -> Result<(), CliError> {
    // `triage agents whoami [--json]` — introspect the caller's own row, which
    // the plain listing deliberately omits. Lets an agent learn how triage
    // sees it (pane id/target, resolved name,
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
    let store = Store::open_default();
    reconcile_quietly(&store);

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
        .map(|s| agent_row(&store, s))
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
            if let Some(seconds) = row.no_progress_seconds {
                println!("       no progress {}m", seconds / 60);
            }
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
    let store = Store::open_default();
    let row = sessions
        .iter()
        .find(|s| s.pane.as_ref().is_some_and(|p| p.pane_id == pane_id))
        .map(|s| agent_row(&store, s));

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
    let mode = args.mode.unwrap_or_else(|| Config::load().send.mode);
    let sessions = load_snapshot()?;
    let parents = tmux::build_ppid_map();
    let caller = resolve_caller(&sessions, std::process::id(), &parents)?;
    let sender = format!("{} ({})", session_display_label(caller), target_id(caller));
    let body = read_message_body(&args)?;
    let body = validate_body(&body)?;
    let store = Store::open_default();
    reconcile_quietly(&store);
    let target = resolve_send_target(&store, &sessions, selector)?;
    if target.pid == caller.pid {
        return Err(CliError::usage("cannot send a message to yourself"));
    }
    let from = agent_identity(&store, caller);
    let to = agent_identity(&store, target);

    match mode {
        SendMode::Legacy => {
            let formatted = format_message(&sender, &body);
            let result = deliver_to(target, selector, &sender, &formatted, args.dry_run)?;
            if !args.dry_run
                && let (Some(from), Some(to)) = (&from, &to)
            {
                let msg = new_message(from, caller, to, &body);
                if let Err(e) = store
                    .mailbox(&to.agent)
                    .and_then(|mailbox| mailbox.record_pasted(&msg, mailbox::now_ms()))
                {
                    eprintln!("warning: failed to record the message in the mailbox: {e}");
                }
            }
            Ok(result)
        }
        SendMode::Mailbox => {
            let (Some(from), Some(to)) = (from, to) else {
                let formatted = format_message(&sender, &body);
                return deliver_to(target, selector, &sender, &formatted, args.dry_run);
            };
            let short = mailbox::short_id(&to.agent);
            let msg = new_message(&from, caller, &to, &body);
            if !hook_capable(&store, &to) {
                // A paste lands in the target's history as a user prompt, so it
                // uses the compact one-line form, not the fenced hook rendering.
                let text = format_message(&sender, &body);
                let result = deliver_to(target, selector, &sender, &text, args.dry_run)?;
                if !args.dry_run
                    && let Err(e) = store
                        .mailbox(&to.agent)
                        .and_then(|mailbox| mailbox.record_pasted(&msg, mailbox::now_ms()))
                {
                    eprintln!("warning: failed to record the message in the mailbox: {e}");
                }
                return Ok(format!(
                    "{result} (target has no triage hooks, so the message was pasted)"
                ));
            }
            if args.dry_run {
                return Ok(format!(
                    "dry-run: would queue from {sender} to {} (agent {short})",
                    target_label(target)
                ));
            }
            let recipient = store
                .mailbox(&to.agent)
                .map_err(|e| CliError::runtime(e.to_string()))?;
            recipient
                .enqueue(&msg)
                .map_err(|e| CliError::delivery(format!("failed to queue message: {e}")))?;
            if let Err(e) = crate::transport::nudge_helper(&store, &to.agent) {
                eprintln!("warning: queued, but could not start the wake helper: {e}");
            }
            let label = target_label(target);
            match args.wait_secs {
                None => Ok(format!("queued for {label} (agent {short}) id={}", msg.id)),
                Some(secs) => wait_for_delivery(&store, &recipient, &msg.id, secs)
                    .map(|how| format!("{how} to {label} (agent {short}) id={}", msg.id)),
            }
        }
    }
}

fn reconcile_quietly(store: &Store) {
    if let Err(e) = crate::reconcile::reconcile_all(store) {
        store.log_hook_error("reconcile", &e);
    }
}

/// A target takes mailbox delivery only while its host process is alive and
/// runs triage's hooks; anything else gets legacy paste.
fn hook_capable(store: &Store, to: &AgentIdentity) -> bool {
    to.host.is_some_and(|host| {
        store
            .read_host(host)
            .is_some_and(|record| record.hook_version == crate::peer_hooks::HOOK_VERSION)
            && mailbox::liveness(host) == mailbox::Liveness::Alive
    })
}

fn wait_for_delivery(
    store: &Store,
    mailbox: &mailbox::Mailbox,
    id: &str,
    secs: u64,
) -> Result<&'static str, CliError> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(250);
    const RECONCILE_EVERY: u32 = 8;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut tick = 0u32;
    loop {
        match mailbox.locate(id) {
            Some(State::Delivered | State::Read) => return Ok("delivered"),
            Some(State::Notified) => return Ok("delivered (head only)"),
            Some(State::Undeliverable) => {
                return Err(CliError::delivery(format!(
                    "message {id} bounced: the target session ended before reading it"
                )));
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(CliError::wait_timeout(format!(
                "timed out after {secs}s; message {id} is still queued"
            )));
        }
        tick += 1;
        if tick.is_multiple_of(RECONCILE_EVERY) {
            reconcile_quietly(store);
        }
        std::thread::sleep(POLL);
    }
}

fn run_inbox(args: &[String]) -> Result<(), CliError> {
    let mut json = false;
    let mut show: Option<String> = None;
    let mut session_arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--help" | "-h" => {
                println!("{}", inbox_usage(""));
                return Ok(());
            }
            "show" if show.is_none() && i == 0 => {
                i += 1;
                let id = args
                    .get(i)
                    .ok_or_else(|| CliError::usage(inbox_usage("missing message id")))?;
                if !mailbox::is_uuid(id) {
                    return Err(CliError::usage(format!("invalid message id {id:?}")));
                }
                show = Some(id.clone());
            }
            "--session" if show.is_some() => {
                i += 1;
                let session = args
                    .get(i)
                    .ok_or_else(|| CliError::usage(inbox_usage("missing --session value")))?;
                if !mailbox::is_uuid(session) {
                    return Err(CliError::usage(format!("invalid session id {session:?}")));
                }
                session_arg = Some(session.clone());
            }
            other => {
                return Err(CliError::usage(inbox_usage(format!(
                    "unknown arg {other:?}"
                ))));
            }
        }
        i += 1;
    }

    let sessions = load_snapshot()?;
    let parents = tmux::build_ppid_map();
    let caller = match &session_arg {
        Some(session) => session_in_caller_chain(&sessions, session, &parents)?,
        None => resolve_caller(&sessions, std::process::id(), &parents)?,
    };
    let store = Store::open_default();
    reconcile_quietly(&store);
    let identity = agent_identity(&store, caller).ok_or_else(|| {
        CliError::runtime("the calling session has no mailbox identity (session id is not a UUID)")
    })?;
    let is_current = caller.session_id == identity.session;
    let mailbox = store
        .mailbox(&identity.agent)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    match show {
        Some(id) => inbox_show(&mailbox, &id, is_current, json),
        None => inbox_list(&mailbox, caller, &identity, is_current, json),
    }
}

/// Only the agent's current session commits: a stale session (cleared, or
/// superseded in another host) may read its mail but must not mark it
/// delivered on the new session's behalf.
fn inbox_list(
    mailbox: &mailbox::Mailbox,
    caller: &Session,
    identity: &AgentIdentity,
    is_current: bool,
    json: bool,
) -> Result<(), CliError> {
    let runtime = |e: io::Error| CliError::runtime(format!("mailbox error: {e}"));
    let mut claimed = Vec::new();
    let mut messages = Vec::new();
    for id in mailbox.ids(State::Pending) {
        if !is_current {
            if let Ok(msg) = mailbox.read(State::Pending, &id) {
                messages.push(msg);
            }
            continue;
        }
        let claim = Claim {
            nonce: mailbox::new_nonce(),
            claimed_at_ms: mailbox::now_ms(),
            waiter_pid: std::process::id(),
            waiter_start: mailbox::proc_info(std::process::id()).map_or(0, |i| i.start),
            transcript_path: String::new(),
            transcript_offset: 0,
            printed_at_ms: None,
            head_only: false,
        };
        let host = identity.host.unwrap_or(HostId {
            pid: caller.pid,
            start: 0,
        });
        if let Some(inflight) = mailbox
            .claim(&id, &caller.session_id, host, &claim)
            .map_err(runtime)?
        {
            messages.push(mailbox.read_inflight(&inflight).map_err(runtime)?);
            claimed.push(inflight);
        }
    }

    let output = if json {
        serde_json::to_string_pretty(&messages)
            .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?
    } else if messages.is_empty() {
        "No pending messages.".to_string()
    } else {
        let items: Vec<RenderItem> = messages
            .iter()
            .map(|msg| RenderItem {
                msg,
                nonce: None,
                sent_before_clear: msg.to.session_at_send != identity.session,
            })
            .collect();
        let (mut text, _) = mailbox::render_batch(&items, usize::MAX);
        if !is_current {
            text.push_str(
                "\n\n(read-only: this session is not the agent's current session, so nothing was marked delivered)",
            );
        }
        text
    };
    let printed = writeln!(io::stdout().lock(), "{output}").and_then(|()| io::stdout().flush());
    for inflight in &claimed {
        let settled = if printed.is_ok() {
            mailbox.commit(
                inflight,
                State::Delivered,
                DeliveredVia::Inbox,
                mailbox::now_ms(),
            )
        } else {
            mailbox.revert(inflight)
        };
        settled.map_err(runtime)?;
    }
    printed.map_err(|e| CliError::runtime(format!("failed to print messages: {e}")))
}

fn inbox_show(
    mailbox: &mailbox::Mailbox,
    id: &str,
    is_current: bool,
    json: bool,
) -> Result<(), CliError> {
    let states = [
        State::Notified,
        State::Pending,
        State::Delivered,
        State::Read,
        State::Undeliverable,
        State::Pasted,
    ];
    let Some((state, msg)) = states
        .into_iter()
        .find_map(|state| mailbox.read(state, id).ok().map(|msg| (state, msg)))
    else {
        return Err(CliError::usage(format!(
            "no message {id} in this agent's mailbox"
        )));
    };
    let output = if json {
        serde_json::to_string_pretty(&msg)
            .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?
    } else {
        let item = RenderItem {
            msg: &msg,
            nonce: None,
            sent_before_clear: false,
        };
        mailbox::render_batch(&[item], usize::MAX).0
    };
    writeln!(io::stdout().lock(), "{output}")
        .and_then(|()| io::stdout().flush())
        .map_err(|e| CliError::runtime(format!("failed to print message: {e}")))?;
    if state == State::Notified && is_current {
        mailbox
            .transition(
                id,
                State::Notified,
                State::Read,
                Some((DeliveredVia::InboxShow, mailbox::now_ms())),
            )
            .map_err(|e| CliError::runtime(format!("mailbox error: {e}")))?;
    }
    Ok(())
}

/// `--session` names a session explicitly, but only one whose process is an
/// ancestor of this command, so a caller can't act as an unrelated agent.
fn session_in_caller_chain<'a>(
    sessions: &'a [Session],
    session_id: &str,
    parents: &HashMap<u32, u32>,
) -> Result<&'a Session, CliError> {
    let mut chain = std::collections::HashSet::new();
    let mut current = std::process::id();
    while current > 1 && chain.insert(current) {
        match parents.get(&current) {
            Some(parent) => current = *parent,
            None => break,
        }
    }
    sessions
        .iter()
        .find(|s| s.session_id == session_id && chain.contains(&s.pid))
        .ok_or_else(|| {
            CliError::denied(format!(
                "session {session_id} is not an agent session this command runs under"
            ))
        })
}

fn inbox_usage(prefix: impl Into<String>) -> String {
    let prefix = prefix.into();
    let usage =
        "usage: triage inbox [--json]\n       triage inbox show ID [--session UUID] [--json]";
    if prefix.is_empty() {
        usage.to_string()
    } else {
        format!("{prefix}\n{usage}")
    }
}

/// The stable mailbox identity behind a live session: its host process, the
/// host's current session and that session's lineage root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentIdentity {
    agent: String,
    session: String,
    host: Option<HostId>,
}

fn agent_identity(store: &Store, s: &Session) -> Option<AgentIdentity> {
    let host = mailbox::proc_info(s.pid).ok().map(|info| HostId {
        pid: s.pid,
        start: info.start,
    });
    let session = host
        .and_then(|host| store.read_host(host))
        .map(|record| record.current_session)
        .unwrap_or_else(|| s.session_id.clone());
    if !mailbox::is_uuid(&session) {
        return None;
    }
    let agent = store
        .lineage_root(&session)
        .unwrap_or_else(|| session.clone());
    Some(AgentIdentity {
        agent,
        session,
        host,
    })
}

pub(crate) fn session_agent_id(store: &Store, s: &Session) -> Option<String> {
    agent_identity(store, s).map(|identity| identity.agent)
}

fn new_message(from: &AgentIdentity, caller: &Session, to: &AgentIdentity, body: &str) -> Message {
    Message {
        v: 1,
        id: mailbox::new_message_id(),
        created_at_ms: mailbox::now_ms(),
        from: mailbox::Sender {
            agent: from.agent.clone(),
            session: from.session.clone(),
            provider: caller.provider,
            label: session_display_label(caller),
        },
        to: mailbox::Recipient {
            agent: to.agent.clone(),
            session_at_send: to.session.clone(),
        },
        body: body.to_string(),
        attempt: 0,
        bounce_of: None,
        delivered_at_ms: None,
        delivered_via: None,
    }
}

fn resolve_send_target<'a>(
    store: &Store,
    sessions: &'a [Session],
    selector: &str,
) -> Result<&'a Session, CliError> {
    let by_agent = |s: &Session| {
        agent_identity(store, s).is_some_and(|identity| {
            identity.agent == selector
                || identity.session == selector
                || mailbox::short_id(&identity.agent) == selector
        })
    };
    let matches = sessions
        .iter()
        .filter(|s| selector_matches(s, selector) || by_agent(s))
        .collect::<Vec<_>>();
    single_target(matches, selector)
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

/// Claude backstop wake for a session whose waiter is gone: the same pointer
/// text a Codex session gets, pasted only if the session is idle and the
/// usual send gate passes.
pub fn paste_wake_pointer(host_pid: u32, text: &str) -> Result<(), String> {
    let sessions = load_snapshot().map_err(|e| e.message)?;
    let target = sessions
        .iter()
        .find(|s| s.pid == host_pid)
        .ok_or_else(|| format!("no tracked session for pid {host_pid}"))?;
    if target.status == "busy" {
        return Ok(());
    }
    let gate = evaluate_send_gate(target);
    if !gate.can_send {
        return Ok(());
    }
    let pane = target
        .pane
        .as_ref()
        .ok_or_else(|| "target has no tmux pane".to_string())?;
    tmux::paste_text_and_enter(&pane.pane_id, text).map_err(|e| e.to_string())
}

fn deliver_message(
    selector: &str,
    sender: &str,
    message: &str,
    dry_run: bool,
) -> Result<String, CliError> {
    let sessions = load_snapshot()?;
    let target = resolve_target(&sessions, selector)?;
    deliver_to(target, selector, sender, message, dry_run)
}

fn deliver_to(
    target: &Session,
    selector: &str,
    sender: &str,
    message: &str,
    dry_run: bool,
) -> Result<String, CliError> {
    let gate = evaluate_send_gate(target);
    if !gate.can_send {
        let _ = append_message_audit(&AuditEntry::denied(sender, selector, target, &gate.reason));
        return Err(CliError::denied(gate.reason));
    }

    if dry_run {
        return Ok(format!(
            "dry-run: would send from {sender} to {} ({})",
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

    let suffix = if target.state == AttentionState::NoProgress {
        "; no recent agent progress, input submitted but processing is unconfirmed"
    } else if target.state == AttentionState::Working {
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
                return Err(CliError::usage(
                    "--from is no longer supported; triage derives the sender from the calling agent session. Remove --from and any manual sender prefix.",
                ));
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
            "--wait" => out.wait_secs = Some(DEFAULT_WAIT_SECS),
            wait if wait.starts_with("--wait=") => {
                let secs = &wait["--wait=".len()..];
                out.wait_secs = Some(secs.parse().map_err(|_| {
                    CliError::usage(send_usage(format!("invalid --wait value {secs:?}")))
                })?);
            }
            "--mode" => {
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| CliError::usage(send_usage("missing --mode value")))?;
                out.mode = Some(SendMode::parse(value).ok_or_else(|| {
                    CliError::usage(send_usage(format!("unknown --mode {value:?}")))
                })?);
            }
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
    let sessions = snapshot::discover_sessions_with_panes(
        SystemTime::now(),
        &mut digest_cache,
        &mut codex_cache,
        &aliases,
        panes,
    );
    if !codex_cache.discovery_errors.is_empty() {
        return Err(CliError::runtime(format!(
            "Codex discovery unavailable: {}",
            codex_cache.discovery_errors.join("; ")
        )));
    }
    Ok(sessions)
}

fn resolve_target<'a>(sessions: &'a [Session], selector: &str) -> Result<&'a Session, CliError> {
    let matches = sessions
        .iter()
        .filter(|s| selector_matches(s, selector))
        .collect::<Vec<_>>();
    single_target(matches, selector)
}

fn single_target<'a>(matches: Vec<&'a Session>, selector: &str) -> Result<&'a Session, CliError> {
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
    evaluate_send_gate_with_capture(s, s.provider == Provider::Codex)
}

fn evaluate_send_gate_with_capture(s: &Session, require_capture: bool) -> GateResult {
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
        if s.provider == Provider::Codex {
            match tmux::codex_composer_has_draft(&raw) {
                Some(false) => {}
                Some(true) => {
                    return blocked(
                        "target has unsent text or an unrecognized composer continuation",
                    );
                }
                None => return blocked("cannot recognize the Codex composer"),
            }
        }
        // Real (non-faint) text in the composer means the user is mid-typing —
        // a paste would land on their draft and submit the mangled result.
        if s.provider == Provider::Claude && tmux::has_draft_input(&raw) {
            return blocked("target has unsent text in its input box (user may be typing)");
        }
    } else if require_capture {
        return blocked("cannot inspect target pane");
    }

    // Working sessions may queue input; attention state does not gate delivery.
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

fn agent_row(store: &Store, s: &Session) -> AgentRow {
    let gate = evaluate_send_gate(s);
    AgentRow {
        id: target_id(s),
        agent_id: agent_identity(store, s).map(|identity| identity.agent),
        provider: s.provider.label().to_string(),
        name: session_display_label(s),
        cwd: s.cwd.display().to_string(),
        state: attention_state_name(s.state).to_string(),
        can_receive: gate.can_send,
        no_progress_seconds: classifier::no_progress_age(s, SystemTime::now())
            .map(|age| age.as_secs()),
        pending_tool: s.provider == Provider::Codex && s.last_tool_use.is_some(),
        deny_reason: (!gate.can_send).then_some(gate.reason),
        pane_target: s.pane.as_ref().map(|p| p.target.clone()),
        pane_id: s.pane.as_ref().map(|p| p.pane_id.clone()),
        session_id: s.session_id.clone(),
        updated_at_ms: s.updated_at_ms,
        active_background_jobs: s.active_background_jobs,
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
        AttentionState::NoProgress => "NoProgress",
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

const LEGACY_MESSAGE_PREFIX: &str = "[triage message from ";
pub const PEER_MESSAGE_PREFIX: &str = "📨 Peer message from ";
pub const WAKE_POINTER_PREFIX: &str = "📨 triage: ";

/// Text triage injected into an agent's input rather than something the user
/// typed; transcript parsers must not treat it as a prompt.
pub fn is_triage_delivery(text: &str) -> bool {
    let text = text.trim_start();
    [
        LEGACY_MESSAGE_PREFIX,
        PEER_MESSAGE_PREFIX,
        WAKE_POINTER_PREFIX,
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

pub(crate) fn format_message(sender: &str, body: &str) -> String {
    if body.contains('\n') {
        format!("{LEGACY_MESSAGE_PREFIX}{sender}]\n{body}")
    } else {
        format!("{LEGACY_MESSAGE_PREFIX}{sender}] {body}")
    }
}

fn resolve_caller<'a>(
    sessions: &'a [Session],
    pid: u32,
    parents: &HashMap<u32, u32>,
) -> Result<&'a Session, CliError> {
    let mut current = pid;
    let mut seen = std::collections::HashSet::new();
    while current > 1 && seen.insert(current) {
        let mut matches = sessions.iter().filter(|session| session.pid == current);
        if let Some(session) = matches.next() {
            if matches.next().is_some() {
                return Err(CliError::denied(
                    "calling process matches multiple agent sessions; refresh session discovery before sending",
                ));
            }
            return Ok(session);
        }
        let Some(parent) = parents.get(&current) else {
            break;
        };
        current = *parent;
    }
    Err(CliError::denied(
        "cannot identify the calling agent session; run triage send from a tracked agent's tool shell and ensure process discovery is available",
    ))
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
    let usage = "usage: triage send --to TARGET (--message TEXT | --file PATH | - | TEXT...) [--mode legacy|mailbox] [--wait[=SECS]] [--dry-run]\n       TARGET: pane id, pane target, name, agent id or its last 8 characters";
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

    #[test]
    fn sender_is_nearest_ancestor_not_another_session_in_same_window() {
        let mut caller = session(AttentionState::Working);
        caller.pid = 200;
        caller.session_id = "caller".into();
        let mut other = caller.clone();
        other.pid = 300;
        other.session_id = "other".into();
        let sessions = vec![other, caller];
        let parents = HashMap::from([(400, 350), (350, 200), (200, 300)]);
        assert_eq!(
            resolve_caller(&sessions, 400, &parents).unwrap().session_id,
            "caller"
        );
    }

    #[test]
    fn sender_requires_unique_tracked_ancestor() {
        let caller = session(AttentionState::Working);
        let sessions = vec![caller.clone(), caller];
        let parents = HashMap::from([(400, 123)]);
        assert!(resolve_caller(&sessions, 400, &parents).is_err());
        assert!(resolve_caller(&sessions[..1], 400, &HashMap::new()).is_err());
        assert!(
            resolve_caller(
                &sessions[..1],
                400,
                &HashMap::from([(400, 401), (401, 400)])
            )
            .is_err()
        );
    }

    #[test]
    fn sender_override_is_rejected_with_migration_guidance() {
        let args = ["--to", "%42", "--from", "someone", "--message", "hello"].map(str::to_string);
        let error = parse_send_args(&args).err().unwrap();
        assert!(error.message.contains("--from is no longer supported"));
    }

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
    fn interrupt_distinguishes_baseline_services_from_task_descendants() {
        let baseline = "100 1 S+ 19:00:00 codex
101 100 S 19:00:00 /opt/homebrew/bin/uv tool uvx --from git+ssh://git@github.com/Affirm/ai-memory-bank-mcp mcp_memory_bank_setup
102 101 S 18:59:58 /cache/bin/python /cache/bin/mcp_memory_bank_setup
103 100 S 19:00:00 npm exec @playwright/mcp@0.0.68
104 103 S 18:59:59 node /cache/node_modules/.bin/playwright-mcp
105 100 S 19:00:00 npm exec @salesforce/mcp@0.25.0 --orgs DEFAULT_TARGET_ORG --toolsets orgs,metadata,data,users
106 105 S 18:59:59 node /cache/node_modules/.bin/sf-mcp-server --orgs DEFAULT_TARGET_ORG --toolsets orgs,metadata,data,users
107 100 S 18:59:51 /release/bin/codex-code-mode-host
";
        assert_eq!(live_children_in_ps(100, 900, baseline), Ok(false));
        for child in [
            "108 100 S 00:01 sleep 60",
            "108 107 S 00:01 node task.js",
            "108 104 S 00:01 /bin/chromium",
            "108 102 S 00:01 /bin/python task.py",
            "108 100 S 19:00:00 node old-task.js",
            "108 100 S 00:01 /release/bin/codex-code-mode-host",
            "108 100 S 19:00:00 npm exec unrelated-mcp",
            "108 100 S 19:00:00 node /cache/node_modules/.bin/playwright-mcp",
        ] {
            assert_eq!(
                live_children_in_ps(100, 900, &format!("{baseline}{child}")),
                Ok(true),
                "{child}"
            );
        }
        assert_eq!(
            live_children_in_ps(100, 900, &baseline.replace("107 100 S", "107 100 R")),
            Ok(true)
        );
        assert_eq!(
            live_children_in_ps(100, 900, &baseline.replace("18:59:51", "18:59:29")),
            Ok(true)
        );
        assert_eq!(live_children_in_ps(100, 19 * 3600, baseline), Ok(true));
        assert!(live_children_in_ps(999, 900, baseline).is_err());
        assert!(live_children_in_ps(100, 900, "100 1 S invalid codex").is_err());
    }

    #[test]
    fn process_elapsed_time_formats() {
        assert_eq!(elapsed_seconds("00:01"), Some(1));
        assert_eq!(elapsed_seconds("19:08:09"), Some(68889));
        assert_eq!(elapsed_seconds("2-01:02:03"), Some(176523));
        for invalid in ["", "invalid", "1-02:03", "00:60", "1:99:00"] {
            assert_eq!(elapsed_seconds(invalid), None);
        }
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
