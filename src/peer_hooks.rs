//! Hook entrypoints (`triage inbox --hook …`) that deliver mailbox mail into a
//! running agent. They run inside the agent's hook machinery: never load
//! config, never spawn subprocesses, never write to stderr except the Claude
//! waiter's delivery, and always exit 0 on error (logging to `hook.log`).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use serde_json::Value;

use crate::mailbox::{
    self, Claim, DeliveredVia, Fit, HostId, HostRecord, Inflight, Liveness, LockKind, Mailbox,
    Message, RenderItem, State, Store, WaiterOwner,
};
use crate::models::Provider;

pub const HOOK_VERSION: &str = "v1";
/// Matches the `timeout` of the installed waiter entries.
pub const WAITER_TIMEOUT_SECS: u64 = 86_400;
const WAITER_DEADLINE_MARGIN: Duration = Duration::from_secs(60);
const WAITER_HANDOFF_WAIT: Duration = Duration::from_secs(10);
const RESCAN_INTERVAL: Duration = Duration::from_secs(1);
const DRAIN_TIME_LIMIT: Duration = Duration::from_secs(3);
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024;
const MAX_HOST_HOPS: usize = 32;
const EXIT_WAKE: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    SessionStart,
    Stop,
    PostToolUse,
}

struct HookArgs {
    provider: Provider,
    event: Event,
    wait: bool,
}

struct Payload {
    session: String,
    transcript_path: Option<PathBuf>,
}

/// `triage inbox --hook <claude|codex> <event> [--wait] --triage-hook=v1`.
/// Returns the process exit code.
pub fn cli(args: &[String]) -> i32 {
    let store = Store::open_default();
    let context = args.join(" ");
    let result = parse_args(args).and_then(|hook| {
        let mut stdin = String::new();
        io::stdin()
            .take(MAX_PAYLOAD_BYTES)
            .read_to_string(&mut stdin)?;
        run(&store, &hook, &stdin)
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            store.log_hook_error(&context, &e);
            0
        }
    }
}

fn parse_args(args: &[String]) -> io::Result<HookArgs> {
    let bad = |msg: &str| io::Error::new(io::ErrorKind::InvalidInput, msg.to_string());
    let provider = match args.first().map(String::as_str) {
        Some("claude") => Provider::Claude,
        Some("codex") => Provider::Codex,
        _ => return Err(bad("expected claude or codex")),
    };
    let event = match args.get(1).map(String::as_str) {
        Some("session-start") => Event::SessionStart,
        Some("stop") => Event::Stop,
        Some("post-tool-use") => Event::PostToolUse,
        _ => return Err(bad("unknown hook event")),
    };
    let mut wait = false;
    let mut version = None;
    for arg in &args[2..] {
        match arg.as_str() {
            "--wait" => wait = true,
            other => match other.strip_prefix("--triage-hook=") {
                Some(v) => version = Some(v),
                None => return Err(bad("unknown hook argument")),
            },
        }
    }
    if version != Some(HOOK_VERSION) {
        return Err(bad("hook entry version does not match this triage binary"));
    }
    Ok(HookArgs {
        provider,
        event,
        wait,
    })
}

fn parse_payload(stdin: &str) -> io::Result<Payload> {
    let value: Value = serde_json::from_str(stdin)?;
    let session = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| mailbox::is_uuid(s))
        .or_else(|| {
            std::env::var("CODEX_THREAD_ID")
                .ok()
                .filter(|s| mailbox::is_uuid(s))
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "payload has no valid session_id",
            )
        })?;
    let transcript_path = value
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    Ok(Payload {
        session,
        transcript_path,
    })
}

fn run(store: &Store, hook: &HookArgs, stdin: &str) -> io::Result<i32> {
    let payload = parse_payload(stdin)?;
    let host = find_host(hook.provider)
        .ok_or_else(|| io::Error::other("no harness process among this hook's ancestors"))?;
    let agent = register(store, host, hook.provider, &payload.session, hook.event)?;
    let mailbox = store.mailbox(&agent)?;
    match (hook.provider, hook.event) {
        (Provider::Claude, Event::SessionStart | Event::Stop) => {
            confirm_waiter_deliveries(&mailbox, payload.transcript_path.as_deref())?;
            if !hook.wait {
                return Ok(0);
            }
            wait_for_mail(store, &mailbox, &agent, host, &payload)
        }
        (Provider::Claude, Event::PostToolUse) => drain(
            store,
            &mailbox,
            host,
            &payload,
            "PostToolUse",
            DeliveredVia::ClaudePostToolUse,
        ),
        (Provider::Codex, _) => Err(io::Error::other("Codex hooks are not supported yet")),
    }
}

/// The nearest ancestor that is the harness process: for Claude a pid with a
/// `~/.claude/sessions/<pid>.json` record, for Codex a process named `codex`.
fn find_host(provider: Provider) -> Option<HostId> {
    let sessions_dir = crate::discovery::sessions_dir();
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..MAX_HOST_HOPS {
        if pid <= 1 {
            return None;
        }
        let info = mailbox::proc_info(pid).ok()?;
        let is_host = match provider {
            Provider::Claude => sessions_dir.join(format!("{pid}.json")).exists(),
            Provider::Codex => info.comm == "codex",
        };
        if is_host {
            return Some(HostId {
                pid,
                start: info.start,
            });
        }
        pid = info.ppid;
    }
    None
}

/// Links the session into its agent's lineage and records the host's current
/// session. Returns the agent id.
fn register(
    store: &Store,
    host: HostId,
    provider: Provider,
    session: &str,
    event: Event,
) -> io::Result<String> {
    let record = store.read_host(host);
    let agent = match store.lineage_root(session) {
        Some(root) => root,
        None => {
            let root = match &record {
                Some(r) if r.current_session != session => store
                    .lineage_root(&r.current_session)
                    .unwrap_or_else(|| r.current_session.clone()),
                _ => session.to_string(),
            };
            store.link_lineage(session, &root)?
        }
    };
    let current_session = match (&record, event) {
        (Some(r), Event::Stop | Event::PostToolUse) => r.current_session.clone(),
        _ => session.to_string(),
    };
    store.write_host(
        host,
        &HostRecord {
            v: 1,
            provider,
            hook_version: HOOK_VERSION.to_string(),
            current_session,
            updated_at_ms: mailbox::now_ms(),
        },
    )?;
    if agent != session {
        merge_unlinked_mail(store, session, &agent)?;
    }
    Ok(agent)
}

/// Mail sent before a session was linked is addressed to the session id
/// itself (the fallback agent id); fold it into the root's mailbox.
fn merge_unlinked_mail(store: &Store, session: &str, agent: &str) -> io::Result<()> {
    let from = store.mailbox(session)?;
    let to = store.mailbox(agent)?;
    for id in from.ids(State::Pending) {
        if let Ok(msg) = from.read(State::Pending, &id) {
            to.enqueue(&msg)?;
            from.remove(State::Pending, &id)?;
        }
    }
    Ok(())
}

fn host_current_session(store: &Store, host: HostId) -> Option<String> {
    store.read_host(host).map(|record| record.current_session)
}

fn wait_for_mail(
    store: &Store,
    mailbox: &Mailbox,
    agent: &str,
    host: HostId,
    payload: &Payload,
) -> io::Result<i32> {
    block_sigterm();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(WAITER_TIMEOUT_SECS) - WAITER_DEADLINE_MARGIN;
    let _lock = loop {
        if let Some(lock) = store.try_lock(LockKind::Waiter, agent)? {
            break lock;
        }
        let owner_is_current = store
            .read_waiter_owner(agent)
            .zip(host_current_session(store, host))
            .is_some_and(|(owner, current)| owner.session == current);
        if owner_is_current || started.elapsed() >= WAITER_HANDOFF_WAIT {
            return Ok(0);
        }
        std::thread::sleep(RESCAN_INTERVAL);
    };
    store.write_waiter_owner(
        agent,
        &WaiterOwner {
            session: payload.session.clone(),
            pid: std::process::id(),
        },
    )?;

    let (tx, rx) = mpsc::channel();
    let pending_dir = store.root().join("mail").join(agent).join("pending");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&pending_dir)?;
    let mut watcher = notify::recommended_watcher(move |_| {
        let _ = tx.send(());
    })
    .map_err(io::Error::other)?;
    watcher
        .watch(&pending_dir, RecursiveMode::NonRecursive)
        .map_err(io::Error::other)?;

    loop {
        if mailbox::liveness(host) == Liveness::Dead
            || host_current_session(store, host).as_deref() != Some(payload.session.as_str())
            || Instant::now() >= deadline
        {
            return Ok(0);
        }
        if !mailbox.ids(State::Pending).is_empty() {
            let offset = payload
                .transcript_path
                .as_deref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len());
            let claim_template = Claim {
                nonce: String::new(),
                claimed_at_ms: 0,
                waiter_pid: std::process::id(),
                waiter_start: mailbox::proc_info(std::process::id()).map_or(0, |info| info.start),
                transcript_path: payload
                    .transcript_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                transcript_offset: offset,
                printed_at_ms: None,
                head_only: false,
            };
            if let Some(batch) = claim_batch(mailbox, &payload.session, host, &claim_template)? {
                let mut stderr = io::stderr().lock();
                stderr.write_all(batch.text.as_bytes())?;
                stderr.flush()?;
                let printed = mailbox::now_ms();
                for (inflight, claim) in &batch.claims {
                    let mut claim = claim.clone();
                    claim.printed_at_ms = Some(printed);
                    mailbox.write_claim(inflight, &claim)?;
                }
                return Ok(EXIT_WAKE);
            }
        }
        let _ = rx.recv_timeout(RESCAN_INTERVAL);
    }
}

struct Batch {
    text: String,
    claims: Vec<(Inflight, Claim)>,
}

/// Claims pending mail oldest first for as long as it fits the delivery
/// budget. A message that doesn't fit is released unshown.
fn claim_batch(
    mailbox: &Mailbox,
    session: &str,
    host: HostId,
    template: &Claim,
) -> io::Result<Option<Batch>> {
    let mut claimed: Vec<(Inflight, Claim, Message)> = Vec::new();
    for id in mailbox.ids(State::Pending) {
        let claim = Claim {
            nonce: mailbox::new_nonce(),
            claimed_at_ms: mailbox::now_ms(),
            ..template.clone()
        };
        let Some(inflight) = mailbox.claim(&id, session, host, &claim)? else {
            continue;
        };
        let msg = match mailbox.read_inflight(&inflight) {
            Ok(msg) => msg,
            Err(e) => {
                mailbox.release(&inflight)?;
                return Err(e);
            }
        };
        claimed.push((inflight, claim, msg));
        let (_, fits) = render(&claimed, session);
        if fits.last() == Some(&Fit::Deferred) {
            let (inflight, _, _) = claimed.pop().expect("just pushed");
            mailbox.release(&inflight)?;
            break;
        }
    }
    if claimed.is_empty() {
        return Ok(None);
    }
    let (text, fits) = render(&claimed, session);
    let mut claims = Vec::with_capacity(claimed.len());
    for ((inflight, mut claim, _), fit) in claimed.into_iter().zip(fits) {
        if fit == Fit::Head {
            claim.head_only = true;
            mailbox.write_claim(&inflight, &claim)?;
        }
        claims.push((inflight, claim));
    }
    Ok(Some(Batch { text, claims }))
}

fn render(claimed: &[(Inflight, Claim, Message)], session: &str) -> (String, Vec<Fit>) {
    let items: Vec<RenderItem> = claimed
        .iter()
        .map(|(_, claim, msg)| RenderItem {
            msg,
            nonce: Some(&claim.nonce),
            sent_before_clear: msg.to.session_at_send != session,
        })
        .collect();
    mailbox::render_batch(&items, mailbox::DELIVERY_BUDGET_CHARS)
}

/// Sync drain: hand pending mail to the harness as `additionalContext` and
/// commit once it has been written out.
fn drain(
    store: &Store,
    mailbox: &Mailbox,
    host: HostId,
    payload: &Payload,
    hook_event_name: &str,
    via: DeliveredVia,
) -> io::Result<i32> {
    let started = Instant::now();
    if host_current_session(store, host).as_deref() != Some(payload.session.as_str())
        || mailbox.ids(State::Pending).is_empty()
    {
        return Ok(0);
    }
    let template = Claim {
        nonce: String::new(),
        claimed_at_ms: 0,
        waiter_pid: std::process::id(),
        waiter_start: 0,
        transcript_path: String::new(),
        transcript_offset: 0,
        printed_at_ms: None,
        head_only: false,
    };
    if started.elapsed() >= DRAIN_TIME_LIMIT {
        return Ok(0);
    }
    let Some(batch) = claim_batch(mailbox, &payload.session, host, &template)? else {
        return Ok(0);
    };
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": batch.text,
        }
    });
    let written = serde_json::to_writer(io::stdout().lock(), &output)
        .map_err(io::Error::from)
        .and_then(|()| io::stdout().flush());
    for (inflight, claim) in &batch.claims {
        if written.is_ok() {
            let to = if claim.head_only {
                State::Notified
            } else {
                State::Delivered
            };
            mailbox.commit(inflight, to, via, mailbox::now_ms())?;
        } else {
            mailbox.release(inflight)?;
        }
    }
    written.map(|()| 0)
}

/// A waiter's delivery is confirmed once its header (message id plus the
/// claim's stored nonce) shows up in a rewake record in the transcript.
fn confirm_waiter_deliveries(
    mailbox: &Mailbox,
    current_transcript: Option<&Path>,
) -> io::Result<()> {
    for inflight in mailbox.inflight() {
        let Some(claim) = mailbox.read_claim(&inflight) else {
            continue;
        };
        if claim.printed_at_ms.is_none() || claim.transcript_path.is_empty() {
            continue;
        }
        let marker = format!("[id={} n={}", inflight.id, claim.nonce);
        let claim_transcript = PathBuf::from(&claim.transcript_path);
        let mut found = transcript_has_rewake(&claim_transcript, claim.transcript_offset, &marker)
            == Some(true);
        if !found
            && let Some(current) = current_transcript
            && current != claim_transcript
        {
            found = transcript_has_rewake(current, 0, &marker) == Some(true);
        }
        if found {
            let to = if claim.head_only {
                State::Notified
            } else {
                State::Delivered
            };
            mailbox.commit(&inflight, to, DeliveredVia::ClaudeWaiter, mailbox::now_ms())?;
        }
    }
    Ok(())
}

/// `None` means unknown (unreadable, or too much to scan), which must never be
/// read as "not delivered".
fn transcript_has_rewake(path: &Path, offset: u64, marker: &str) -> Option<bool> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = offset.min(len);
    if len - start > MAX_TRANSCRIPT_SCAN_BYTES {
        return None;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        if !line.contains(marker) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if rewake_text(&record)
            .is_some_and(|text| text.starts_with("<task-notification>") && text.contains(marker))
        {
            return Some(true);
        }
    }
    Some(false)
}

/// The model-visible text of a rewake: a user record when the session was
/// idle, a `queued_command` attachment when it was folded into a running turn.
fn rewake_text(record: &Value) -> Option<&str> {
    match record.get("type").and_then(Value::as_str)? {
        "user" => record.pointer("/message/content").and_then(Value::as_str),
        "attachment" => {
            let attachment = record.get("attachment")?;
            (attachment.get("type").and_then(Value::as_str) == Some("queued_command")
                && attachment.get("commandMode").and_then(Value::as_str)
                    == Some("task-notification"))
            .then(|| attachment.get("prompt").and_then(Value::as_str))
            .flatten()
        }
        _ => None,
    }
}

/// Claude SIGTERMs every waiter on `/exit`; one landing between claiming mail
/// and printing it would strand the claim. Blocked for the waiter's whole
/// life, before any thread exists, so no thread can take the signal; the
/// waiter still exits within a rescan once its host is gone.
fn block_sigterm() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}
