//! `triage messages`: review peer mail across every agent's mailbox, and
//! `triage messages purge` for retention.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::mailbox::{self, Message, State, Store};

const UNLINKED_FLAG_AFTER_MS: u64 = 24 * 60 * 60 * 1000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

const USAGE: &str = "usage: triage messages [--with AGENT] [--thread AGENT AGENT] [--pending] [--follow] [--json]\n       triage messages purge [--older-than DAYS]\n  AGENT: an agent id or its last 8 characters";

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub state: &'static str,
    #[serde(flatten)]
    pub msg: Message,
    /// Addressed to a session that never linked to an agent, and old.
    pub unlinked: bool,
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Pending => "pending",
        State::Inflight => "inflight",
        State::Delivered => "delivered",
        State::Notified => "notified",
        State::Read => "read",
        State::Undeliverable => "undeliverable",
        State::Pasted => "pasted",
    }
}

const LISTED_STATES: [State; 6] = [
    State::Pending,
    State::Delivered,
    State::Notified,
    State::Read,
    State::Undeliverable,
    State::Pasted,
];

/// Every message in every mailbox, oldest first.
pub fn collect(store: &Store) -> Vec<Entry> {
    let now = mailbox::now_ms();
    let linked: std::collections::HashSet<String> = store
        .hosts()
        .into_iter()
        .map(|(_, r)| {
            store
                .lineage_root(&r.current_session)
                .unwrap_or(r.current_session)
        })
        .collect();
    let mut entries = Vec::new();
    for agent in store.agents() {
        let Ok(mailbox) = store.mailbox(&agent) else {
            continue;
        };
        let unlinked_agent = store.lineage_root(&agent).is_none() && !linked.contains(&agent);
        let mut push = |state: State, msg: Message| {
            let unlinked = unlinked_agent
                && matches!(state, State::Pending)
                && now.saturating_sub(msg.created_at_ms) > UNLINKED_FLAG_AFTER_MS;
            entries.push(Entry {
                state: state_name(state),
                msg,
                unlinked,
            });
        };
        for state in LISTED_STATES {
            for id in mailbox.ids(state) {
                if let Ok(msg) = mailbox.read(state, &id) {
                    push(state, msg);
                }
            }
        }
        for inflight in mailbox.inflight() {
            if let Ok(msg) = mailbox.read_inflight(&inflight) {
                push(State::Inflight, msg);
            }
        }
    }
    entries.sort_by(|a, b| a.msg.id.cmp(&b.msg.id));
    entries
}

fn matches_agent(agent: &str, selector: &str) -> bool {
    agent == selector || mailbox::short_id(agent) == selector
}

#[derive(Default)]
struct Filter {
    with: Option<String>,
    thread: Option<(String, String)>,
    pending: bool,
}

impl Filter {
    fn keep(&self, e: &Entry) -> bool {
        let (from, to) = (&e.msg.from.agent, &e.msg.to.agent);
        if let Some(a) = &self.with
            && !matches_agent(from, a)
            && !matches_agent(to, a)
        {
            return false;
        }
        if let Some((a, b)) = &self.thread {
            let forward = matches_agent(from, a) && matches_agent(to, b);
            let back = matches_agent(from, b) && matches_agent(to, a);
            if !forward && !back {
                return false;
            }
        }
        !self.pending || matches!(e.state, "pending" | "inflight" | "notified")
    }
}

/// Labels agents by what they called themselves when they last sent mail.
fn labels(entries: &[Entry]) -> HashMap<String, String> {
    entries
        .iter()
        .map(|e| {
            (
                e.msg.from.agent.clone(),
                mailbox::display_label(&e.msg.from.label).to_string(),
            )
        })
        .collect()
}

fn format_entry(e: &Entry, labels: &HashMap<String, String>) -> String {
    let name = |agent: &str| {
        let short = mailbox::short_id(agent);
        match labels.get(agent) {
            Some(label) => format!("{label} ({short})"),
            None => short.to_string(),
        }
    };
    let state = match (e.state, e.msg.delivered_via, e.msg.delivered_at_ms) {
        ("delivered" | "read" | "pasted", Some(via), Some(at)) => format!(
            "{} via {} · {}s",
            e.state,
            serde_json::to_value(via)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            at.saturating_sub(e.msg.created_at_ms) / 1000
        ),
        _ if e.unlinked => format!("{} · unlinked 24h+", e.state),
        (state, _, _) => state.to_string(),
    };
    let first_line = e.msg.body.lines().next().unwrap_or("");
    let body = if first_line.chars().count() > 100 || e.msg.body.contains('\n') {
        format!("{}…", first_line.chars().take(100).collect::<String>())
    } else {
        first_line.to_string()
    };
    format!(
        "{}  {} → {}  [{state}]  {body}",
        format_time(e.msg.created_at_ms),
        name(&e.msg.from.agent),
        name(&e.msg.to.agent),
    )
}

/// Local wall-clock `YYYY-MM-DD HH:MM`.
fn format_time(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return ms.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

pub fn cli(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err((code, message)) => {
            eprintln!("{message}");
            code
        }
    }
}

fn usage_error(msg: impl std::fmt::Display) -> (i32, String) {
    (2, format!("{msg}\n{USAGE}"))
}

fn run(args: &[String]) -> Result<(), (i32, String)> {
    let store = Store::open_default();
    if args.first().map(String::as_str) == Some("purge") {
        return run_purge(&store, &args[1..]);
    }
    let mut filter = Filter::default();
    let mut json = false;
    let mut follow = false;
    let mut i = 0;
    let valid =
        |s: &str| mailbox::is_uuid(s) || (s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--follow" => follow = true,
            "--pending" => filter.pending = true,
            "--with" => {
                let a = args
                    .get(i + 1)
                    .filter(|a| valid(a))
                    .ok_or_else(|| usage_error("--with needs an agent id"))?;
                filter.with = Some(a.clone());
                i += 1;
            }
            "--thread" => {
                let (a, b) = (args.get(i + 1), args.get(i + 2));
                match (a, b) {
                    (Some(a), Some(b)) if valid(a) && valid(b) => {
                        filter.thread = Some((a.clone(), b.clone()));
                    }
                    _ => return Err(usage_error("--thread needs two agent ids")),
                }
                i += 2;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(usage_error(format!("unknown arg {other:?}"))),
        }
        i += 1;
    }
    if json && follow {
        return Err(usage_error(
            "--follow prints lines; it can't be combined with --json",
        ));
    }
    if let Err(e) = crate::reconcile::reconcile_all(&store) {
        store.log_hook_error("reconcile", &e);
    }
    let all = collect(&store);
    let names = labels(&all);
    let entries: Vec<Entry> = all.into_iter().filter(|e| filter.keep(e)).collect();
    if json {
        let text = serde_json::to_string_pretty(&entries).map_err(|e| (1, e.to_string()))?;
        println!("{text}");
        return Ok(());
    }
    let mut out = io::stdout().lock();
    for e in &entries {
        writeln!(out, "{}", format_entry(e, &names)).map_err(|e| (1, e.to_string()))?;
    }
    if entries.is_empty() && !follow {
        writeln!(out, "No messages.").map_err(|e| (1, e.to_string()))?;
    }
    out.flush().map_err(|e| (1, e.to_string()))?;
    if !follow {
        return Ok(());
    }
    let mut seen: HashMap<String, &'static str> = entries
        .iter()
        .map(|e| (e.msg.id.clone(), e.state))
        .collect();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let all = collect(&store);
        let names = labels(&all);
        for e in all.iter().filter(|e| filter.keep(e)) {
            if seen.get(&e.msg.id) != Some(&e.state) {
                seen.insert(e.msg.id.clone(), e.state);
                writeln!(out, "{}", format_entry(e, &names)).map_err(|e| (1, e.to_string()))?;
            }
        }
        out.flush().map_err(|e| (1, e.to_string()))?;
    }
}

fn run_purge(store: &Store, args: &[String]) -> Result<(), (i32, String)> {
    let days = match args {
        [] => crate::config::Config::load().send.retention_days,
        [flag, days] if flag == "--older-than" => days
            .parse()
            .map_err(|_| usage_error(format!("invalid day count {days:?}")))?,
        _ => return Err(usage_error("purge takes only --older-than DAYS")),
    };
    let (mail, legacy) = purge(store, days).map_err(|e| (1, e.to_string()))?;
    println!("Removed {mail} message(s) and {legacy} legacy log line(s) older than {days} day(s).");
    Ok(())
}

/// Removes settled mail (never pending, inflight or notified) and legacy
/// `agent-messages.jsonl` lines older than `days`. Returns both counts.
pub fn purge(store: &Store, days: u64) -> io::Result<(usize, usize)> {
    let cutoff = mailbox::now_ms().saturating_sub(days * DAY_MS);
    let mut removed = 0;
    for agent in store.agents() {
        let mailbox = store.mailbox(&agent)?;
        for state in [
            State::Delivered,
            State::Read,
            State::Pasted,
            State::Undeliverable,
        ] {
            for id in mailbox.ids(state) {
                let Ok(msg) = mailbox.read(state, &id) else {
                    continue;
                };
                if msg.delivered_at_ms.unwrap_or(msg.created_at_ms) < cutoff {
                    mailbox.remove(state, &id)?;
                    removed += 1;
                }
            }
        }
    }
    Ok((removed, purge_legacy_log(cutoff / 1000)?))
}

fn legacy_log_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/triage/agent-messages.jsonl"))
}

fn purge_legacy_log(cutoff_secs: u64) -> io::Result<usize> {
    let Some(path) = legacy_log_path() else {
        return Ok(0);
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let old = |line: &str| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("ts").and_then(serde_json::Value::as_u64))
            .is_some_and(|ts| ts < cutoff_secs)
    };
    let kept: Vec<&str> = text.lines().filter(|l| !old(l)).collect();
    let removed = text.lines().count() - kept.len();
    if removed > 0 {
        let staged = path.with_extension("jsonl.tmp");
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        fs::write(&staged, body)?;
        fs::rename(&staged, &path)?;
    }
    Ok(removed)
}
