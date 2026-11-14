use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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

pub fn cli_approve(args: &[String]) -> i32 {
    match run_approve(args) {
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

const APPROVE_USAGE: &str = "usage: triage approve --to TARGET [--deny] [--dry-run]\n       TARGET: pane id, pane target, name, agent id or its last 8 characters";

struct ApproveArgs {
    selector: String,
    deny: bool,
    dry_run: bool,
}

fn parse_approve_args(args: &[String]) -> Result<Option<ApproveArgs>, CliError> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        return Ok(None);
    }
    let mut selector = None;
    let mut deny = false;
    let mut dry_run = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--to" if selector.is_none() => {
                selector = Some(
                    args.next()
                        .filter(|value| !value.starts_with('-'))
                        .ok_or_else(|| CliError::usage(APPROVE_USAGE))?
                        .clone(),
                );
            }
            "--deny" => deny = true,
            "--dry-run" => dry_run = true,
            _ => return Err(CliError::usage(APPROVE_USAGE)),
        }
    }
    let selector = selector.ok_or_else(|| CliError::usage(APPROVE_USAGE))?;
    Ok(Some(ApproveArgs {
        selector,
        deny,
        dry_run,
    }))
}

fn run_approve(args: &[String]) -> Result<String, CliError> {
    let Some(args) = parse_approve_args(args)? else {
        return Ok(APPROVE_USAGE.to_string());
    };
    let sessions = load_snapshot()?;
    let parents = tmux::build_ppid_map();
    let caller = &resolve_current_caller(&sessions, &parents)?;
    let sender = sender_label(caller);
    let store = Store::open_default();
    reconcile_quietly(&store);
    let target = resolve_send_target(&store, &sessions, &args.selector)?;
    if same_session(target, caller) {
        return Err(CliError::usage("cannot decide your own permission prompt"));
    }

    let verb = if args.deny { "deny" } else { "approve" };
    let prompt = match inspect_permission_prompt(target, args.deny) {
        Ok(prompt) => prompt,
        Err(reason) => {
            let verdict = format!("{verb}-refused");
            let entry = AuditEntry::new(
                &sender,
                &args.selector,
                target,
                &verdict,
                Some(&reason),
                None,
            );
            if let Err(error) = append_message_audit(&entry) {
                eprintln!("warning: failed to append approve audit: {error}");
            }
            return Err(CliError::denied(reason));
        }
    };
    let pane_id = target_id(target);
    let label = target_label(target);
    let headline = truncate_chars(&prompt.replace('\n', " "), 120);
    if args.dry_run {
        return Ok(format!(
            "dry-run: would {verb} {pane_id} ({label}): {headline}"
        ));
    }
    let key = if args.deny { "Escape" } else { "Enter" };
    tmux::send_keys(&pane_id, &[key])
        .map_err(|error| CliError::delivery(format!("{verb} failed: {error}")))?;
    let verdict = if args.deny { "denied" } else { "approved" };
    let entry = AuditEntry::new(
        &sender,
        &args.selector,
        target,
        verdict,
        None,
        Some(&prompt),
    );
    if let Err(error) = append_message_audit(&entry) {
        eprintln!("warning: failed to append approve audit: {error}");
    }
    Ok(format!("{verdict} {pane_id} ({label}): {headline}"))
}

/// Capture the target's live screen and prove it shows a permission prompt
/// whose highlighted option makes Enter (approve) or Escape (deny) correct.
/// Returns the prompt body for the audit trail.
fn inspect_permission_prompt(target: &Session, deny: bool) -> Result<String, String> {
    let Some(pane) = &target.pane else {
        return Err("target has no tmux pane".to_string());
    };
    // Pane text alone can't prove a live Codex approval (scrollback or quoted
    // source can mimic it); the rollout must also show an escalated call
    // waiting on one.
    if target.provider == Provider::Codex && !target.approval_prompt_pending {
        return Err("Codex session has no pending approval".to_string());
    }
    let Some(content) = tmux::capture_pane_visible_ansi(&pane.pane_id) else {
        return Err("cannot inspect target pane".to_string());
    };
    evaluate_approve_gate(target.provider, &content, deny).map_err(str::to_string)
}

fn evaluate_approve_gate(
    provider: Provider,
    pane_text: &str,
    deny: bool,
) -> Result<String, &'static str> {
    match provider {
        Provider::Claude => {
            let prompt = tmux::claude_permission_prompt_checked(pane_text)?;
            let one_time_yes =
                prompt.approval_selected && tmux::selected_option_is_one_time_yes(pane_text);
            if !deny && !one_time_yes {
                return Err("the highlighted option is not a one-time Yes");
            }
            Ok(prompt.body)
        }
        Provider::Codex => {
            let plain = tmux::strip_ansi(pane_text);
            if !tmux::has_codex_permission_prompt(&plain) {
                return Err("no permission prompt visible");
            }
            if !deny {
                match tmux::codex_selected_permission_choice(&plain) {
                    Some(tmux::CodexPromptChoice::Yes)
                        if tmux::selected_option_is_one_time_yes(&plain) => {}
                    Some(_) => return Err("the highlighted option is not a one-time Yes"),
                    None => return Err("prompt has no selected choice"),
                }
            }
            Ok(tmux::parse_codex_pending_full(&plain).unwrap_or_default())
        }
        Provider::Opencode | Provider::Pi => {
            Err("approve is not supported for opencode or pi agents")
        }
    }
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

pub(crate) fn elapsed_seconds(raw: &str) -> Option<u64> {
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
    send_message_name: Option<String>,
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
    let parents = tmux::build_ppid_map();
    let caller = resolve_current_caller(&sessions, &parents).ok();
    let store = Store::open_default();
    reconcile_quietly(&store);

    let rows = sessions
        .iter()
        .filter(|s| {
            args.include_self
                || caller
                    .as_ref()
                    .is_none_or(|caller| !same_session(s, caller))
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

    let sessions = load_snapshot()?;
    let parents = tmux::build_ppid_map();
    let caller = &resolve_current_caller(&sessions, &parents)?;
    let store = Store::open_default();
    let row = agent_row(&store, caller);

    if json {
        let value = serde_json::to_value(&row)
            .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&value)
                .map_err(|e| CliError::runtime(format!("failed to render JSON: {e}")))?
        );
        return Ok(());
    }

    let target = row.pane_target.as_deref().unwrap_or("?");
    println!(
        "pane:     {} ({})",
        row.pane_id.as_deref().unwrap_or("none"),
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
    let caller = &resolve_current_caller(&sessions, &parents)?;
    let sender = sender_label(caller);
    let from_session = caller_session(caller);
    let body = read_message_body(&args)?;
    let body = validate_body(&body)?;
    let store = Store::open_default();
    reconcile_quietly(&store);
    let target = resolve_send_target(&store, &sessions, selector)?;
    if same_session(target, caller) {
        return Err(CliError::usage("cannot send a message to yourself"));
    }
    let from = agent_identity(&store, caller);
    let to = agent_identity(&store, target);

    match mode {
        SendMode::Legacy => {
            let formatted = format_message(&sender, &body);
            let result = deliver_to(
                target,
                selector,
                &sender,
                from_session,
                &formatted,
                args.dry_run,
            )?;
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
                return deliver_to(
                    target,
                    selector,
                    &sender,
                    from_session,
                    &formatted,
                    args.dry_run,
                );
            };
            let short = mailbox::short_id(&to.agent);
            let msg = new_message(&from, caller, &to, &body);
            if to.provider == Provider::Opencode {
                let label = target_label(target);
                if args.dry_run {
                    return Ok(format!(
                        "dry-run: would deliver from {sender} to {label} (agent {short}) through the opencode server"
                    ));
                }
                match deliver_through_opencode(&store, &msg) {
                    Ok(()) => {
                        let entry = AuditEntry::sent(&sender, selector, target, &body)
                            .with_from_session(from_session);
                        if let Err(e) = append_message_audit(&entry) {
                            eprintln!("warning: failed to append agent-message audit: {e}");
                        }
                        return Ok(format!(
                            "delivered to {label} (agent {short}) through the opencode server id={}",
                            msg.id
                        ));
                    }
                    Err(e) => eprintln!(
                        "warning: the opencode server did not take the message ({e}); pasting it instead"
                    ),
                }
            }
            if !hook_capable(&store, &to) {
                // A paste lands in the target's history as a user prompt, so it
                // uses the compact one-line form, not the fenced hook rendering.
                let text = format_message(&sender, &body);
                let result =
                    deliver_to(target, selector, &sender, from_session, &text, args.dry_run)?;
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

/// Hands the message to the opencode server in the same rendering hook
/// delivery uses, and records it delivered once the server accepted it.
fn deliver_through_opencode(store: &Store, msg: &Message) -> io::Result<()> {
    let item = RenderItem {
        msg,
        nonce: None,
        sent_before_clear: false,
        reader_is_claude: false,
    };
    let (text, _) = mailbox::render_batch(&[item], usize::MAX);
    crate::opencode_api::post_message(&msg.to.session_at_send, &text)?;
    if let Err(e) = store.mailbox(&msg.to.agent).and_then(|mailbox| {
        mailbox.record_delivered(msg, DeliveredVia::OpencodeServer, mailbox::now_ms())
    }) {
        eprintln!("warning: delivered, but failed to record the message in the mailbox: {e}");
    }
    Ok(())
}

/// A target takes mailbox delivery only while its host process is alive and
/// runs triage's hooks; anything else gets legacy paste.
fn hook_capable(store: &Store, to: &AgentIdentity) -> bool {
    to.host.is_some_and(|host| {
        store
            .read_session_host(host, to.provider, &to.session)
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
        Some(session) => session_in_caller_chain(&sessions, session, &parents)?.clone(),
        None => resolve_current_caller(&sessions, &parents)?,
    };
    let store = Store::open_default();
    reconcile_quietly(&store);
    let identity = agent_identity(&store, &caller).ok_or_else(|| match caller.provider {
        Provider::Opencode | Provider::Pi => CliError::runtime(format!(
            "cannot tell which {} session is calling: run triage inbox from the agent's own tool shell",
            caller.provider.name()
        )),
        Provider::Claude | Provider::Codex => CliError::runtime(
            "the calling session has no mailbox identity (session id is not a UUID)",
        ),
    })?;
    let is_current = caller.session_id == identity.session;
    let mailbox = store
        .mailbox(&identity.agent)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    match show {
        Some(id) => inbox_show(&mailbox, &id, caller.provider, is_current, json),
        None => inbox_list(&mailbox, &caller, &identity, is_current, json),
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
                reader_is_claude: caller.provider == Provider::Claude,
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
    reader: Provider,
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
            reader_is_claude: reader == Provider::Claude,
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
    if sessions
        .iter()
        .any(|s| s.provider == Provider::Codex && s.session_id == session_id)
        && resolve_current_caller(sessions, parents)?.session_id != session_id
    {
        return Err(CliError::denied(
            "the requested session is not the calling Codex thread",
        ));
    }
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
    provider: Provider,
}

fn agent_identity(store: &Store, s: &Session) -> Option<AgentIdentity> {
    if s.provider.pane_only() {
        return pane_agent_identity(store, s);
    }
    let host = mailbox::proc_info(s.pid).ok().map(|info| HostId {
        pid: s.pid,
        start: info.start,
    });
    let session = host
        .and_then(|host| store.read_session_host(host, s.provider, &s.session_id))
        .map(|record| record.current_session)
        .unwrap_or_else(|| s.session_id.clone());
    if !mailbox::is_uuid(&session) {
        return None;
    }
    let agent = store.agent_for(s.provider, &session);
    Some(AgentIdentity {
        agent,
        session,
        host,
        provider: s.provider,
    })
}

/// An opencode or pi agent is its session. A row only matched to a pane, or
/// an id of the wrong shape for its harness, has no mailbox identity. A pi
/// session's host is the live pi process whose extension registered it.
fn pane_agent_identity(store: &Store, s: &Session) -> Option<AgentIdentity> {
    let valid = match s.provider {
        Provider::Opencode => mailbox::is_opencode_session(&s.session_id),
        Provider::Pi => mailbox::is_uuid(&s.session_id),
        Provider::Claude | Provider::Codex => false,
    };
    valid.then(|| AgentIdentity {
        agent: s.session_id.clone(),
        session: s.session_id.clone(),
        host: (s.provider == Provider::Pi)
            .then(|| live_pi_host(store, &s.session_id))
            .flatten(),
        provider: s.provider,
    })
}

fn live_pi_host_session(store: &Store, pid: u32) -> Option<String> {
    let host = HostId {
        pid,
        start: mailbox::proc_info(pid).ok()?.start,
    };
    store
        .read_host(host)
        .filter(|record| record.provider == Provider::Pi)
        .map(|record| record.current_session)
}

fn live_pi_host(store: &Store, session: &str) -> Option<HostId> {
    store
        .hosts()
        .into_iter()
        .filter(|(_, r)| r.provider == Provider::Pi && r.current_session == session)
        .map(|(host, _)| host)
        .find(|&host| mailbox::liveness(host) == mailbox::Liveness::Alive)
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
            send_message_name: caller.send_message_name.clone(),
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

/// What the agent is told when the auto-mode auditor denies its tool call.
/// The denial is delivered by pressing Escape, which Claude Code records as
/// "the user rejected this tool use ... wait for the user", so without this
/// the agent stops for a human who never acted and nobody learns why.
fn auditor_denial_text(tool_name: &str, reason: &str) -> String {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    // The pane-scrape path often cannot name the tool ("?").
    let call = match tool_name {
        "" | "?" => "tool call".to_string(),
        name => format!("{name} call"),
    };
    format!(
        "triage's auto-mode auditor, not your user, denied your {call}: {}. \
         The \"user rejected\" message was this denial, so no human is waiting on you. \
         Rewrite the action so it can be reviewed (plain, one step, no obfuscation), \
         or ask your lead or user before retrying.",
        truncate_chars(&reason, 400)
    )
}

const DENIAL_NOTICE_ATTEMPTS: u32 = 20;
const DENIAL_NOTICE_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// Tell the agent in `pane_id` that the auditor denied its call, through the
/// same guarded paste as `triage send`. The agent only becomes receivable once
/// the interrupted tool call settles, so a gate refusal is retried briefly;
/// anything else, or a gate still closed after that (a human typing a draft),
/// is left to the message audit log.
pub fn send_auditor_denial(pane_id: &str, tool_name: &str, reason: &str) -> Result<String, String> {
    let message = format_message("triage auditor", &auditor_denial_text(tool_name, reason));
    let mut last = String::new();
    for attempt in 0..DENIAL_NOTICE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(DENIAL_NOTICE_RETRY);
        }
        match deliver_message(pane_id, "triage auditor", &message, false) {
            Ok(done) => return Ok(done),
            Err(e) if e.code == 3 => last = e.message,
            Err(e) => return Err(e.message),
        }
    }
    Err(last)
}

fn format_user_reply(body: &str) -> Result<String, CliError> {
    let body = validate_body(body)?;
    if body.contains('\n') {
        return Err(CliError::usage("reply must be a single line"));
    }
    Ok(body)
}

/// Claude and pi backstop wake for a session whose waiter is gone: the pointer
/// text a Codex session gets, pasted only if the session is idle and the
/// usual send gate passes.
pub fn paste_wake_pointer(host_pid: u32, session: &str, text: &str) -> Result<(), String> {
    let sessions = load_snapshot().map_err(|e| e.message)?;
    // A pi row's pid is its pane, not the pi process.
    let target = sessions
        .iter()
        .find(|s| s.pid == host_pid || (s.provider == Provider::Pi && s.session_id == session))
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
    deliver_to(target, selector, sender, None, message, dry_run)
}

fn deliver_to(
    target: &Session,
    selector: &str,
    sender: &str,
    from_session: Option<&str>,
    message: &str,
    dry_run: bool,
) -> Result<String, CliError> {
    let gate = evaluate_send_gate(target);
    if !gate.can_send {
        let _ = append_message_audit(
            &AuditEntry::denied(sender, selector, target, &gate.reason)
                .with_from_session(from_session),
        );
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

    if let Err(e) = append_message_audit(
        &AuditEntry::sent(sender, selector, target, message).with_from_session(from_session),
    ) {
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

/// opencode and pi get only the provider-agnostic checks: a visible
/// permission prompt blocks, but there is no composer parsing, and a pane
/// that cannot be captured is still sendable.
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
        send_message_name: s.send_message_name.clone(),
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
        Provider::Opencode => matches!(value.as_str(), "oc" | "opencode"),
        Provider::Pi => value == "pi",
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

/// Process facts the pane-only caller check needs, injectable for tests.
struct PaneProbe<'a> {
    is_pane_root: &'a dyn Fn(u32) -> bool,
    is_harness: &'a dyn Fn(u32, Provider) -> bool,
    /// `(OPENCODE_SESSION_ID, TMUX_PANE)` from the caller's environment.
    opencode_env: Option<(String, String)>,
    opencode_session_dir: &'a dyn Fn(&str) -> Option<PathBuf>,
    opencode_server_pid: Option<u32>,
    opencode_sessions_since: &'a dyn Fn(&Path, u64) -> Option<Vec<String>>,
    /// `PI_SESSION_ID`, unless pi marked the session ephemeral by leaving
    /// `PI_SESSION_FILE` unset.
    pi_session: Option<String>,
    pi_session_in_cwd: &'a dyn Fn(&Path, &str) -> bool,
    /// The session triage's pi extension registered for a live pi process.
    pi_host_session: &'a dyn Fn(u32) -> Option<String>,
    /// Whether a live pi process other than the given one hosts the session.
    pi_hosted_elsewhere: &'a dyn Fn(&str, u32) -> bool,
}

fn resolve_caller(
    sessions: &[Session],
    pid: u32,
    parents: &HashMap<u32, u32>,
    codex_thread: Option<&str>,
) -> Result<Session, CliError> {
    let pane_roots = std::cell::OnceCell::new();
    let opencode_env = std::env::var("OPENCODE_SESSION_ID")
        .ok()
        .zip(std::env::var("TMUX_PANE").ok())
        .filter(|(session, pane)| !session.is_empty() && !pane.is_empty());
    let probe = PaneProbe {
        opencode_server_pid: opencode_env
            .as_ref()
            .and_then(|_| crate::pane_agents::opencode_server_pid()),
        opencode_env,
        is_pane_root: &|pid| {
            pane_roots
                .get_or_init(|| tmux::list_panes().into_keys().collect::<HashSet<u32>>())
                .contains(&pid)
        },
        is_harness: &is_pane_harness,
        opencode_session_dir: &crate::pane_agents::opencode_session_dir,
        opencode_sessions_since: &crate::pane_agents::opencode_sessions_since,
        pi_session: std::env::var("PI_SESSION_ID")
            .ok()
            .filter(|id| !id.is_empty())
            .filter(|_| std::env::var_os("PI_SESSION_FILE").is_some_and(|f| !f.is_empty())),
        pi_session_in_cwd: &crate::pane_agents::pi_session_in_cwd,
        pi_host_session: &|pid| live_pi_host_session(&Store::open_default(), pid),
        pi_hosted_elsewhere: &|session, pid| {
            live_pi_host(&Store::open_default(), session).is_some_and(|host| host.pid != pid)
        },
    };
    resolve_caller_with(sessions, pid, parents, codex_thread, &probe)
}

fn is_pane_harness(pid: u32, provider: Provider) -> bool {
    if crate::peer_hooks::is_test_pane_harness(pid) {
        return true;
    }
    mailbox::proc_info(pid).is_ok_and(|info| is_harness_comm(&info.comm, provider))
}

/// pi must be named `pi`, as discovery requires: a bare `node` could be any
/// Node process.
fn is_harness_comm(comm: &str, provider: Provider) -> bool {
    match provider {
        Provider::Opencode => comm == "opencode" || comm == "opencode-pty",
        Provider::Pi => comm == "pi",
        Provider::Claude | Provider::Codex => false,
    }
}

/// opencode and pi callers are found by tmux pane: the caller must be, or
/// descend from, a harness process inside exactly one tracked pane. That
/// names the pane, not which of the harness's sessions is speaking; the
/// session comes only from the caller's environment (`pane_caller_session`).
fn check_pane_only_caller(
    walked: &[u32],
    pane_root: u32,
    parents: &HashMap<u32, u32>,
    provider: Provider,
    probe: &PaneProbe,
) -> Result<u32, CliError> {
    let Some(&harness) = walked
        .iter()
        .chain([&pane_root])
        .find(|&&pid| (probe.is_harness)(pid, provider))
    else {
        return Err(CliError::denied(format!(
            "calling process is in a {} pane but not under its {} process",
            provider.name(),
            provider.name()
        )));
    };
    let mut above = Vec::new();
    let mut current = pane_root;
    let mut seen = HashSet::new();
    while let Some(&parent) = parents.get(&current) {
        if parent <= 1 || !seen.insert(parent) {
            break;
        }
        above.push(parent);
        current = parent;
    }
    if walked
        .iter()
        .chain(&above)
        .any(|&pid| (probe.is_pane_root)(pid))
    {
        return Err(CliError::denied(
            "calling process sits under more than one tmux pane; cannot tell which agent is sending",
        ));
    }
    Ok(harness)
}

/// opencode runs tool shells under its shared `opencode serve` process, not
/// inside the pane, so no process walk reaches the pane. It does hand each
/// shell `OPENCODE_SESSION_ID` and the client's `TMUX_PANE`. Accept that pair
/// only when the pane is a tracked opencode pane; the caller sits in no tmux
/// pane and descends from an opencode process and from the server whose pid
/// opencode records in its `service.json`; and opencode's database places the
/// session in the pane's directory. The pane claim itself is not proven: a
/// tool shell of that server can name any opencode pane in its session's
/// directory, and any session there. So the caller speaks as the session only
/// when it is the one top-level session in that directory written to since
/// the pane's opencode started; otherwise it is identified by its pane.
fn resolve_opencode_caller(
    sessions: &[Session],
    walked: &[u32],
    probe: &PaneProbe,
) -> Result<Option<Session>, CliError> {
    let Some((session_id, pane_id)) = &probe.opencode_env else {
        return Ok(None);
    };
    let mut panes = sessions.iter().filter(|s| {
        s.provider == Provider::Opencode && s.pane.as_ref().is_some_and(|p| &p.pane_id == pane_id)
    });
    let (Some(session), None) = (panes.next(), panes.next()) else {
        return Err(CliError::denied(format!(
            "TMUX_PANE {pane_id} is not a tracked opencode pane"
        )));
    };
    if walked.iter().any(|&pid| (probe.is_pane_root)(pid)) {
        return Err(CliError::denied(
            "calling process sits in a tmux pane, but opencode tool shells run outside panes",
        ));
    }
    if !walked
        .iter()
        .any(|&pid| (probe.is_harness)(pid, Provider::Opencode))
    {
        return Err(CliError::denied(
            "calling process is not under an opencode process",
        ));
    }
    if !probe
        .opencode_server_pid
        .is_some_and(|server| walked.contains(&server))
    {
        return Err(CliError::denied(
            "calling process is not under the opencode server named in opencode's service.json",
        ));
    }
    let dir = session_id
        .starts_with("ses_")
        .then(|| (probe.opencode_session_dir)(session_id))
        .flatten();
    if dir.as_deref() != Some(session.cwd.as_path()) {
        return Err(CliError::denied(format!(
            "OPENCODE_SESSION_ID {session_id} is not a session in {pane_id}'s directory"
        )));
    }
    let only_recent = session
        .harness_started_ms
        .and_then(|since| (probe.opencode_sessions_since)(&session.cwd, since))
        .is_some_and(|ids| ids == [session_id.as_str()]);
    let mut caller = session.clone();
    caller.session_id = if only_recent {
        session_id.clone()
    } else {
        pane_id.clone()
    };
    Ok(Some(caller))
}

/// A pi caller speaks as the session its `PI_SESSION_ID` names only when
/// that is a pi session recorded for the pane's directory and nothing else
/// could own it: the session triage's extension registered for the caller's
/// pi process, or, with no registration, a session no other live pi process
/// holds while this is the only pi pane in the directory. An id that
/// contradicts those records is refused; one that can't be told apart from
/// another pane's leaves the caller identified by its pane alone.
fn pane_caller_session(
    sessions: &[Session],
    session: &Session,
    harness: u32,
    probe: &PaneProbe,
) -> Result<Session, CliError> {
    let Some(pane) = &session.pane else {
        return Err(CliError::denied("calling pane agent has no tmux pane"));
    };
    let mut caller = session.clone();
    caller.session_id = pane.pane_id.clone();
    let Some(id) = probe
        .pi_session
        .as_ref()
        .filter(|_| session.provider == Provider::Pi)
    else {
        return Ok(caller);
    };
    if !(probe.pi_session_in_cwd)(&session.cwd, id) {
        return Err(CliError::denied(format!(
            "PI_SESSION_ID {id} is not a pi session in {}",
            session.cwd.display()
        )));
    }
    match (probe.pi_host_session)(harness) {
        Some(hosted) if &hosted != id => {
            return Err(CliError::denied(format!(
                "PI_SESSION_ID {id} is not the session this pi process runs"
            )));
        }
        Some(_) => {}
        None => {
            if (probe.pi_hosted_elsewhere)(id, harness) {
                return Err(CliError::denied(format!(
                    "PI_SESSION_ID {id} belongs to another pi process"
                )));
            }
            let pi_panes_here = sessions
                .iter()
                .filter(|s| s.provider == Provider::Pi && s.pane.is_some() && s.cwd == session.cwd)
                .count();
            if pi_panes_here != 1 {
                return Ok(caller);
            }
        }
    }
    caller.session_id = id.clone();
    Ok(caller)
}

/// The caller's own session id when it is known: always for Claude and
/// Codex, and for opencode and pi only when their environment named it.
fn caller_session(caller: &Session) -> Option<&str> {
    let pane_grade = caller.provider.pane_only()
        && caller
            .pane
            .as_ref()
            .is_none_or(|pane| pane.pane_id == caller.session_id);
    (!pane_grade).then_some(caller.session_id.as_str())
}

/// Pane-only callers come back with `session_id` set to the session their
/// environment names, or to their pane id when it names none.
fn resolve_caller_with(
    sessions: &[Session],
    pid: u32,
    parents: &HashMap<u32, u32>,
    codex_thread: Option<&str>,
    probe: &PaneProbe,
) -> Result<Session, CliError> {
    let mut current = pid;
    let mut seen = std::collections::HashSet::new();
    let mut walked = Vec::new();
    while current > 1 && seen.insert(current) {
        let mut matches = sessions.iter().filter(|session| session.pid == current);
        if let Some(session) = matches.next() {
            if let Some(thread) = codex_thread
                && session.provider == Provider::Codex
            {
                let mut selected = sessions.iter().filter(|s| {
                    s.pid == current && s.provider == Provider::Codex && s.session_id == thread
                });
                if let Some(selected_session) = selected.next()
                    && selected.next().is_none()
                {
                    return Ok(selected_session.clone());
                }
                return Err(CliError::denied(
                    "CODEX_THREAD_ID does not identify a unique thread on the calling Codex process",
                ));
            }
            if matches.next().is_some() {
                return Err(CliError::denied(
                    "calling process matches multiple agent sessions; refresh session discovery before sending",
                ));
            }
            if session.provider.pane_only() {
                let harness =
                    check_pane_only_caller(&walked, current, parents, session.provider, probe)?;
                return pane_caller_session(sessions, session, harness, probe);
            }
            return Ok(session.clone());
        }
        walked.push(current);
        let Some(parent) = parents.get(&current) else {
            break;
        };
        current = *parent;
    }
    if let Some(session) = resolve_opencode_caller(sessions, &walked, probe)? {
        return Ok(session);
    }
    Err(CliError::denied(
        "cannot identify the calling agent session; run triage send from a tracked agent's tool shell and ensure process discovery is available",
    ))
}

fn resolve_current_caller(
    sessions: &[Session],
    parents: &HashMap<u32, u32>,
) -> Result<Session, CliError> {
    let thread = std::env::var("CODEX_THREAD_ID").ok();
    resolve_caller(sessions, std::process::id(), parents, thread.as_deref())
}

/// How a caller is named in the delivered text and the audit log. Pane-only
/// providers are named by provider and pane (`opencode:%30`): unique and
/// checkable, but not a session identity.
fn sender_label(caller: &Session) -> String {
    match (&caller.pane, caller.provider.pane_only()) {
        (Some(pane), true) => format!("{}:{}", caller.provider.name(), pane.pane_id),
        _ => format!("{} ({})", session_display_label(caller), target_id(caller)),
    }
}

/// A pane-only caller may carry a session its pane's row was never matched
/// to, so for those providers the pane is what makes two rows the same agent.
fn same_session(a: &Session, b: &Session) -> bool {
    if a.provider.pane_only() && a.provider == b.provider {
        return a.pane.as_ref().map(|p| &p.pane_id) == b.pane.as_ref().map(|p| &p.pane_id);
    }
    a.provider == b.provider && a.pid == b.pid && a.session_id == b.session_id
}

#[derive(Serialize)]
struct AuditEntry {
    ts: u64,
    from: String,
    from_session: Option<String>,
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
            from_session: None,
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

    fn with_from_session(mut self, session: Option<&str>) -> Self {
        self.from_session = session.map(str::to_string);
        self
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
    let usage = "usage: triage agents [--json] [--include-self] [--provider cc|cx|oc|opencode|pi] [--cwd PATH]\n       triage agents whoami [--json]";
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
            resolve_caller(&sessions, 400, &parents, None)
                .unwrap()
                .session_id,
            "caller"
        );
    }

    #[test]
    fn sender_requires_unique_tracked_ancestor() {
        let caller = session(AttentionState::Working);
        let sessions = vec![caller.clone(), caller];
        let parents = HashMap::from([(400, 123)]);
        assert!(resolve_caller(&sessions, 400, &parents, None).is_err());
        assert!(resolve_caller(&sessions[..1], 400, &HashMap::new(), None).is_err());
        assert!(
            resolve_caller(
                &sessions[..1],
                400,
                &HashMap::from([(400, 401), (401, 400)]),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn codex_thread_hint_selects_a_sibling_on_the_nearest_host() {
        let mut first = session(AttentionState::Working);
        first.provider = Provider::Codex;
        first.session_id = "first".into();
        let mut second = first.clone();
        second.session_id = "second".into();
        let sessions = vec![first, second];
        let parents = HashMap::from([(400, 123)]);
        for thread in ["first", "second"] {
            assert_eq!(
                resolve_caller(&sessions, 400, &parents, Some(thread))
                    .unwrap()
                    .session_id,
                thread
            );
        }
        assert!(resolve_caller(&sessions, 400, &parents, None).is_err());
        assert!(resolve_caller(&sessions, 400, &parents, Some("stale")).is_err());
        assert!(resolve_caller(&sessions[..1], 400, &parents, Some("second")).is_err());
    }

    #[test]
    fn codex_thread_hint_cannot_select_a_different_process() {
        let mut caller = session(AttentionState::Working);
        caller.provider = Provider::Codex;
        caller.session_id = "caller".into();
        let mut other = caller.clone();
        other.pid = 200;
        other.session_id = "other".into();
        let sessions = vec![caller, other];
        assert!(
            resolve_caller(
                &sessions,
                400,
                &HashMap::from([(400, 123), (123, 200)]),
                Some("other")
            )
            .is_err()
        );
        assert!(resolve_caller(&sessions, 400, &HashMap::new(), Some("caller")).is_err());
    }

    #[test]
    fn codex_thread_hint_does_not_override_a_nearer_claude_caller() {
        let caller = session(AttentionState::Working);
        assert_eq!(
            resolve_caller(&[caller], 400, &HashMap::from([(400, 123)]), Some("other"))
                .unwrap()
                .provider,
            Provider::Claude
        );
    }

    fn pane_only_session(provider: Provider, pane_root: u32) -> Session {
        let mut s = session(AttentionState::IdleShort);
        s.provider = provider;
        s.pid = pane_root;
        s.session_id = "ses_x".into();
        s.harness_started_ms = Some(1_000);
        if let Some(pane) = s.pane.as_mut() {
            pane.pid = pane_root;
            pane.pane_id = "%30".into();
        }
        s
    }

    fn resolve_pane_only(
        sessions: &[Session],
        parents: &HashMap<u32, u32>,
        harness: u32,
        pane_roots: &[u32],
    ) -> Result<String, String> {
        let probe = PaneProbe {
            is_pane_root: &|pid| pane_roots.contains(&pid),
            is_harness: &|pid, _| pid == harness,
            opencode_env: None,
            opencode_session_dir: &|_| None,
            opencode_server_pid: None,
            opencode_sessions_since: &|_, _| None,
            pi_session: None,
            pi_session_in_cwd: &|_, _| false,
            pi_host_session: &|_| None,
            pi_hosted_elsewhere: &|_, _| false,
        };
        resolve_caller_with(sessions, 400, parents, None, &probe)
            .map(|caller| identified(&caller))
            .map_err(|e| e.message)
    }

    #[test]
    fn pane_only_caller_is_named_by_provider_and_pane() {
        // triage(400) → bash(350) → opencode(300) → fish(200, pane root)
        let parents = HashMap::from([(400, 350), (350, 300), (300, 200), (200, 1)]);
        for (provider, sender) in [
            (Provider::Opencode, "opencode:%30 -"),
            (Provider::Pi, "pi:%30 -"),
        ] {
            let sessions = [pane_only_session(provider, 200)];
            assert_eq!(
                resolve_pane_only(&sessions, &parents, 300, &[200]).as_deref(),
                Ok(sender)
            );
        }
    }

    #[test]
    fn pi_run_as_the_pane_root_process_can_send() {
        // `tmux new-window pi`: the shell execs pi, so pi is the pane root.
        let parents = HashMap::from([(400, 350), (350, 200), (200, 1)]);
        let sessions = [pane_only_session(Provider::Pi, 200)];
        assert_eq!(
            resolve_pane_only(&sessions, &parents, 200, &[200]).as_deref(),
            Ok("pi:%30 -")
        );
    }

    #[test]
    fn pi_harness_is_named_pi_not_node() {
        assert!(is_harness_comm("pi", Provider::Pi));
        assert!(!is_harness_comm("node", Provider::Pi));
        assert!(is_harness_comm("opencode-pty", Provider::Opencode));
    }

    #[test]
    fn pane_only_caller_must_descend_from_the_harness() {
        // A shell job in the pane, not under the harness process.
        let parents = HashMap::from([(400, 350), (350, 200), (200, 1)]);
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        let err = resolve_pane_only(&sessions, &parents, 300, &[200]).unwrap_err();
        assert!(err.contains("not under its opencode process"), "{err}");
    }

    #[test]
    fn pane_only_caller_under_two_panes_is_ambiguous() {
        let parents = HashMap::from([(400, 350), (350, 300), (300, 200), (200, 150), (150, 1)]);
        let sessions = [pane_only_session(Provider::Pi, 200)];
        for roots in [&[200, 150][..], &[200, 350][..]] {
            let err = resolve_pane_only(&sessions, &parents, 300, roots).unwrap_err();
            assert!(err.contains("more than one tmux pane"), "{err}");
        }
    }

    /// The sender label and the session the caller is known by (`-` when
    /// only its pane is).
    fn identified(caller: &Session) -> String {
        format!(
            "{} {}",
            sender_label(caller),
            caller_session(caller).unwrap_or("-")
        )
    }

    fn resolve_pi_env(sessions: &[Session], env: Option<&str>) -> Result<String, String> {
        resolve_pi_hosted(sessions, env, None, None)
    }

    /// `hosted`: the session registered for the caller's pi process (300);
    /// `elsewhere`: a session another live pi process holds.
    fn resolve_pi_hosted(
        sessions: &[Session],
        env: Option<&str>,
        hosted: Option<&str>,
        elsewhere: Option<&str>,
    ) -> Result<String, String> {
        // triage(400) → bash(350) → pi(300) → fish(200, pane root)
        let parents = HashMap::from([(400, 350), (350, 300), (300, 200), (200, 1)]);
        let probe = PaneProbe {
            is_pane_root: &|pid| pid == 200,
            is_harness: &|pid, _| pid == 300,
            opencode_env: None,
            opencode_session_dir: &|_| None,
            opencode_server_pid: None,
            opencode_sessions_since: &|_, _| None,
            pi_session: env.map(str::to_string),
            pi_session_in_cwd: &|cwd, id| {
                cwd == Path::new("/tmp/project") && matches!(id, "pi-known" | "pi-other")
            },
            pi_host_session: &|pid| hosted.filter(|_| pid == 300).map(str::to_string),
            pi_hosted_elsewhere: &|id, pid| pid == 300 && elsewhere == Some(id),
        };
        resolve_caller_with(sessions, 400, &parents, None, &probe)
            .map(|caller| identified(&caller))
            .map_err(|e| e.message)
    }

    #[test]
    fn pi_caller_speaks_as_the_session_its_env_names() {
        let row = pane_only_session(Provider::Pi, 200);
        assert_eq!(
            resolve_pi_env(std::slice::from_ref(&row), Some("pi-known")).as_deref(),
            Ok("pi:%30 pi-known")
        );
        // Even when the row itself was never matched to a session.
        let mut unmatched = row.clone();
        unmatched.session_id = "%30".into();
        assert_eq!(
            resolve_pi_env(&[unmatched], Some("pi-known")).as_deref(),
            Ok("pi:%30 pi-known")
        );
        // The row's own guess is never taken as the caller's session.
        assert_eq!(resolve_pi_env(&[row], None).as_deref(), Ok("pi:%30 -"));
    }

    #[test]
    fn pi_caller_must_name_the_session_its_own_pi_process_runs() {
        let row = pane_only_session(Provider::Pi, 200);
        let rows = std::slice::from_ref(&row);
        assert_eq!(
            resolve_pi_hosted(rows, Some("pi-known"), Some("pi-known"), None).as_deref(),
            Ok("pi:%30 pi-known")
        );
        let err = resolve_pi_hosted(rows, Some("pi-other"), Some("pi-known"), None).unwrap_err();
        assert!(
            err.contains("not the session this pi process runs"),
            "{err}"
        );
        let err = resolve_pi_hosted(rows, Some("pi-other"), None, Some("pi-other")).unwrap_err();
        assert!(err.contains("belongs to another pi process"), "{err}");
    }

    #[test]
    fn two_unregistered_pi_panes_in_one_directory_leave_the_caller_at_pane_grade() {
        let row = pane_only_session(Provider::Pi, 200);
        let mut twin = pane_only_session(Provider::Pi, 900);
        twin.pane.as_mut().unwrap().pane_id = "%31".into();
        assert_eq!(
            resolve_pi_env(&[row.clone(), twin.clone()], Some("pi-known")).as_deref(),
            Ok("pi:%30 -")
        );
        // The caller's own registration still settles it.
        assert_eq!(
            resolve_pi_hosted(&[row, twin], Some("pi-known"), Some("pi-known"), None).as_deref(),
            Ok("pi:%30 pi-known")
        );
    }

    #[test]
    fn pi_session_env_naming_no_session_in_the_pane_cwd_is_refused() {
        let row = pane_only_session(Provider::Pi, 200);
        let err = resolve_pi_env(&[row], Some("pi-elsewhere")).unwrap_err();
        assert!(err.contains("is not a pi session in /tmp/project"), "{err}");
    }

    #[test]
    fn a_pane_only_caller_is_itself_even_under_another_session_id() {
        let row = pane_only_session(Provider::Pi, 200);
        let mut caller = row.clone();
        caller.session_id = "pi-known".into();
        assert!(same_session(&row, &caller));
        let mut other_pane = row.clone();
        other_pane.pane.as_mut().unwrap().pane_id = "%31".into();
        assert!(!same_session(&other_pane, &caller));
    }

    /// A tool shell under the shared opencode server, not in any pane:
    /// triage(400) → bash(350) → opencode-pty(300) → server(250) → init.
    struct OpencodeCall<'a> {
        session: &'a str,
        pane: &'a str,
        harness: u32,
        server: Option<u32>,
        pane_roots: &'a [u32],
        session_dir: &'a str,
        /// Top-level sessions in the directory since the pane's opencode started.
        recent: &'a [&'a str],
    }

    impl Default for OpencodeCall<'_> {
        fn default() -> Self {
            Self {
                session: "ses_x",
                pane: "%30",
                harness: 300,
                server: Some(250),
                pane_roots: &[],
                session_dir: "/tmp/project",
                recent: &["ses_x"],
            }
        }
    }

    fn resolve_opencode_env(sessions: &[Session], call: OpencodeCall) -> Result<String, String> {
        let parents = HashMap::from([(400, 350), (350, 300), (300, 250), (250, 1)]);
        let dir = PathBuf::from(call.session_dir);
        let probe = PaneProbe {
            is_pane_root: &|pid| call.pane_roots.contains(&pid),
            is_harness: &|pid, provider| provider == Provider::Opencode && pid == call.harness,
            opencode_env: Some((call.session.to_string(), call.pane.to_string())),
            opencode_session_dir: &|_| Some(dir.clone()),
            opencode_server_pid: call.server,
            opencode_sessions_since: &|cwd, since| {
                (cwd == Path::new("/tmp/project") && since == 1_000)
                    .then(|| call.recent.iter().map(|id| id.to_string()).collect())
            },
            pi_session: None,
            pi_session_in_cwd: &|_, _| false,
            pi_host_session: &|_| None,
            pi_hosted_elsewhere: &|_, _| false,
        };
        resolve_caller_with(sessions, 400, &parents, None, &probe)
            .map(|caller| identified(&caller))
            .map_err(|e| e.message)
    }

    #[test]
    fn opencode_caller_is_found_from_its_session_and_pane_env() {
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        assert_eq!(
            resolve_opencode_env(&sessions, OpencodeCall::default()).as_deref(),
            Ok("opencode:%30 ses_x")
        );
        // A second session in the pane's directory is still this pane.
        let other = OpencodeCall {
            session: "ses_y",
            recent: &["ses_y"],
            ..Default::default()
        };
        assert_eq!(
            resolve_opencode_env(&sessions, other).as_deref(),
            Ok("opencode:%30 ses_y")
        );
    }

    #[test]
    fn opencode_caller_speaks_as_its_session_only_when_no_other_could_be_it() {
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        for recent in [&["ses_x", "ses_y"][..], &["ses_y"][..], &[][..]] {
            let call = OpencodeCall {
                recent,
                ..Default::default()
            };
            assert_eq!(
                resolve_opencode_env(&sessions, call).as_deref(),
                Ok("opencode:%30 -"),
                "{recent:?}"
            );
        }
        let mut unknown_start = pane_only_session(Provider::Opencode, 200);
        unknown_start.harness_started_ms = None;
        assert_eq!(
            resolve_opencode_env(&[unknown_start], OpencodeCall::default()).as_deref(),
            Ok("opencode:%30 -")
        );
    }

    #[test]
    fn opencode_env_is_refused_when_it_does_not_add_up() {
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        for (call, want) in [
            (
                OpencodeCall {
                    pane: "%99",
                    ..Default::default()
                },
                "not a tracked opencode pane",
            ),
            (
                OpencodeCall {
                    harness: 999,
                    ..Default::default()
                },
                "not under an opencode process",
            ),
            (
                OpencodeCall {
                    session: "ses_y",
                    session_dir: "/elsewhere",
                    ..Default::default()
                },
                "not a session in %30's directory",
            ),
        ] {
            let err = resolve_opencode_env(&sessions, call).unwrap_err();
            assert!(err.contains(want), "{err}");
        }
        let mut pi = pane_only_session(Provider::Pi, 200);
        pi.session_id = "ses_x".into();
        assert!(resolve_opencode_env(&[pi], OpencodeCall::default()).is_err());
    }

    #[test]
    fn opencode_env_from_inside_a_pane_is_refused() {
        // A renamed `opencode` binary in some other pane claiming %30.
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        let call = OpencodeCall {
            pane_roots: &[350],
            ..Default::default()
        };
        let err = resolve_opencode_env(&sessions, call).unwrap_err();
        assert!(err.contains("sits in a tmux pane"), "{err}");
    }

    #[test]
    fn opencode_env_requires_the_recorded_opencode_server() {
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        for server in [None, Some(999)] {
            let call = OpencodeCall {
                server,
                ..Default::default()
            };
            let err = resolve_opencode_env(&sessions, call).unwrap_err();
            assert!(err.contains("not under the opencode server"), "{err}");
        }
    }

    #[test]
    fn opencode_session_id_is_always_checked_against_the_database() {
        // The pane's own session id earns no shortcut past the database.
        let sessions = [pane_only_session(Provider::Opencode, 200)];
        let call = OpencodeCall {
            session_dir: "/elsewhere",
            ..Default::default()
        };
        let err = resolve_opencode_env(&sessions, call).unwrap_err();
        assert!(err.contains("not a session in %30's directory"), "{err}");

        // An unnamed pane's placeholder id is the pane id, not a session.
        let mut unnamed = pane_only_session(Provider::Opencode, 200);
        unnamed.session_id = "%30".into();
        let call = OpencodeCall {
            session: "%30",
            ..Default::default()
        };
        let err = resolve_opencode_env(&[unnamed], call).unwrap_err();
        assert!(err.contains("not a session in %30's directory"), "{err}");
    }

    #[test]
    fn a_claude_caller_with_opencode_env_stays_claude() {
        // Claude (300) launched from an opencode tool shell inherits its env.
        let mut claude = session(AttentionState::Working);
        claude.pid = 300;
        let sessions = [claude, pane_only_session(Provider::Opencode, 200)];
        let call = OpencodeCall {
            harness: 250,
            ..Default::default()
        };
        assert_eq!(
            resolve_opencode_env(&sessions, call).as_deref(),
            Ok("target (%42) sid")
        );
    }

    #[test]
    fn claude_and_codex_callers_keep_their_session_labels() {
        let caller = session(AttentionState::Working);
        assert_eq!(sender_label(&caller), "target (%42)");
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

    const CLAUDE_SHELL_PROMPT: &str = include_str!("../tests/fixtures/claude-prompts/shell.txt");

    #[test]
    fn approve_gate_accepts_claude_prompt_with_yes_selected() {
        let body = evaluate_approve_gate(Provider::Claude, CLAUDE_SHELL_PROMPT, false).unwrap();

        assert!(body.contains("cargo test"));
    }

    #[test]
    fn approve_gate_refuses_claude_prompt_with_no_selected_but_allows_deny() {
        let pane = CLAUDE_SHELL_PROMPT
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  3. No", "❯ 3. No");

        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &pane, false),
            Err("the highlighted option is not a one-time Yes")
        );
        assert!(evaluate_approve_gate(Provider::Claude, &pane, true).is_ok());
    }

    #[test]
    fn approve_gate_refuses_persistent_yes_but_allows_deny() {
        let claude = CLAUDE_SHELL_PROMPT
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("  2. Yes,", "❯ 2. Yes,");
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &claude, false),
            Err("the highlighted option is not a one-time Yes")
        );
        assert!(evaluate_approve_gate(Provider::Claude, &claude, true).is_ok());

        let codex = CODEX_PROMPT
            .replace("› 1. Yes", "  1. Yes")
            .replace("  2. Yes, and", "› 2. Yes, and");
        assert_eq!(
            evaluate_approve_gate(Provider::Codex, &codex, false),
            Err("the highlighted option is not a one-time Yes")
        );
        assert!(evaluate_approve_gate(Provider::Codex, &codex, true).is_ok());
    }

    #[test]
    fn codex_target_without_a_pending_approval_is_refused_before_the_pane_is_read() {
        let mut codex = session(AttentionState::Working);
        codex.provider = Provider::Codex;
        codex.approval_prompt_pending = false;

        assert_eq!(
            inspect_permission_prompt(&codex, false),
            Err("Codex session has no pending approval".to_string())
        );
    }

    #[test]
    fn approve_gate_refuses_claude_user_choice_prompts() {
        for pane in [
            include_str!("../tests/fixtures/claude-prompts/layout-question.txt"),
            include_str!("../tests/fixtures/claude-prompts/multi-question.txt"),
        ] {
            assert!(evaluate_approve_gate(Provider::Claude, pane, false).is_err());
            assert!(evaluate_approve_gate(Provider::Claude, pane, true).is_err());
        }
    }

    #[test]
    fn approve_gate_names_the_check_that_failed() {
        let trailing = format!("{CLAUDE_SHELL_PROMPT}[Opus 5.5] statusline\n");
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &trailing, false),
            Err("extra lines below the 'Esc to cancel' footer")
        );
        let choice = format!("☐ pick one\n{CLAUDE_SHELL_PROMPT}");
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &choice, false),
            Err("a user-choice screen (☐/☒/✔ Submit line) is visible")
        );
        let header = CLAUDE_SHELL_PROMPT
            .replace("Run shell command", "Something else")
            .replace("Do you want to proceed?", "Continue?");
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &header, false),
            Err("no tool header or 'Do you want ...' question above the options")
        );
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, "$ ls\n", false),
            Err("no 'Esc to cancel' footer on screen")
        );
    }

    // A tall heredoc pushed the "Bash command" header off the top of the screen.
    const CLAUDE_TALL_PROMPT: &str = "\
 │ 4. Repeat with `=off`: nothing changes from today.
 │ EOF
 │ git push -q -u origin feature 2>&1 | tail -2; gh pr create --draft --title \"x\"
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Parser skipped input between top-level statements
 Permission rule Bash(gh pr create:*) requires confirmation for this command.
 Do you want to proceed?
 ❯ 1. Yes
   2. No
 Esc to cancel · Tab to amend
";

    #[test]
    fn approve_gate_accepts_prompt_whose_tool_header_scrolled_off_screen() {
        let body = evaluate_approve_gate(Provider::Claude, CLAUDE_TALL_PROMPT, false).unwrap();
        assert!(body.contains("gh pr create"));

        // Without the question line there is no anchor: refuse.
        let no_question = CLAUDE_TALL_PROMPT.replace(" Do you want to proceed?\n", "");
        assert_eq!(
            evaluate_approve_gate(Provider::Claude, &no_question, false),
            Err("no tool header or 'Do you want ...' question above the options")
        );
    }

    #[test]
    fn approve_gate_refuses_when_no_prompt_is_visible() {
        let pane = "$ cargo test\n   Compiling triage\n❯ \n";

        assert!(evaluate_approve_gate(Provider::Claude, pane, false).is_err());
        assert!(evaluate_approve_gate(Provider::Codex, pane, false).is_err());
    }

    #[test]
    fn approve_gate_refuses_prompt_text_quoted_in_scrollback() {
        // The prompt UI is no longer the last thing on screen.
        let pane = format!("{CLAUDE_SHELL_PROMPT}\nthe agent kept working\n");

        assert!(evaluate_approve_gate(Provider::Claude, &pane, false).is_err());
    }

    const CODEX_PROMPT: &str = "  Would you like to run the following command?\n\n  $ cargo test\n\n› 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `cargo test` (p)\n  3. No, and tell Codex what to do differently (esc)\n\n  Press enter to confirm or esc to cancel\n";

    #[test]
    fn approve_gate_accepts_codex_prompt_only_with_yes_selected() {
        let body = evaluate_approve_gate(Provider::Codex, CODEX_PROMPT, false).unwrap();
        assert!(body.contains("cargo test"));

        let no_selected = CODEX_PROMPT
            .replace("› 1. Yes", "  1. Yes")
            .replace("  3. No", "› 3. No");
        assert_eq!(
            evaluate_approve_gate(Provider::Codex, &no_selected, false),
            Err("the highlighted option is not a one-time Yes")
        );
        assert!(evaluate_approve_gate(Provider::Codex, &no_selected, true).is_ok());
    }

    #[test]
    fn approve_args_parse_flags_and_reject_garbage() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        let parsed = parse_approve_args(&args(&["--to", "UPR-329", "--deny", "--dry-run"]))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.selector, "UPR-329");
        assert!(parsed.deny && parsed.dry_run);

        assert!(parse_approve_args(&args(&["--help"])).unwrap().is_none());
        for bad in [
            &["--deny"][..],
            &["--to"],
            &["--to", "--deny"],
            &["--to", "%1", "extra"],
        ] {
            assert_eq!(parse_approve_args(&args(bad)).err().unwrap().code, 2);
        }
    }

    #[test]
    fn auditor_denial_says_it_was_not_the_user_and_carries_the_reason() {
        let text = auditor_denial_text("Bash", "obfuscated\nshell   code");
        assert!(text.contains("not your user"));
        assert!(text.contains("denied your Bash call: obfuscated shell code."));
        assert!(!text.contains('\n'));
        assert!(auditor_denial_text("?", "r").contains("denied your tool call: r."));
        let long = auditor_denial_text("Bash", &"x".repeat(2000));
        assert!(long.chars().count() < 800);
    }
}
