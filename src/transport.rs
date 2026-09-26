//! Wake pointers for agents with pending mail, sent by a detached helper that
//! runs as a singleton per agent. It is the only thing that nudges a session:
//! `codex queue` for Codex, and a draft-gated tmux pointer for a Claude
//! session whose waiter is gone.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::mailbox::{self, HostId, HostRecord, Liveness, LockKind, State, Store};
use crate::models::Provider;

const POINTER_TTL_SECS: u64 = 600;

/// Parser skip signature: `agent_comm::WAKE_POINTER_PREFIX`.
pub fn pointer_text(count: usize, senders: &[String]) -> String {
    format!(
        "{}{} peer message(s) from {}. If nothing is attached, run: triage inbox",
        crate::agent_comm::WAKE_POINTER_PREFIX,
        count,
        senders.join(", ")
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PointerMarker {
    pid: u32,
    start: u64,
    thread: String,
    created_at_ms: u64,
    ttl_secs: u64,
}

fn markers_dir(store: &Store) -> PathBuf {
    store.root().join("markers")
}

fn pointer_marker(store: &Store, agent: &str) -> PathBuf {
    markers_dir(store).join(format!("{agent}.pointer"))
}

fn rescan_marker(store: &Store, agent: &str) -> PathBuf {
    markers_dir(store).join(format!("{agent}.rescan"))
}

/// A pointer was consumed (or went to a thread that is no longer current).
pub fn clear_pointer(store: &Store, agent: &str) {
    if mailbox::is_uuid(agent) {
        let _ = fs::remove_file(pointer_marker(store, agent));
    }
}

fn live_pointer(store: &Store, agent: &str) -> bool {
    let Ok(bytes) = fs::read(pointer_marker(store, agent)) else {
        return false;
    };
    let Ok(marker) = serde_json::from_slice::<PointerMarker>(&bytes) else {
        return false;
    };
    mailbox::now_ms().saturating_sub(marker.created_at_ms) < marker.ttl_secs * 1000
}

fn touch_rescan(store: &Store, agent: &str) -> io::Result<()> {
    fs::create_dir_all(markers_dir(store))?;
    File::create(rescan_marker(store, agent))?;
    Ok(())
}

fn rescan_time(store: &Store, agent: &str) -> Option<SystemTime> {
    fs::metadata(rescan_marker(store, agent))
        .and_then(|m| m.modified())
        .ok()
}

/// Asks for a helper pass: a running helper sees the fresh rescan marker and
/// loops; otherwise a new one is started, detached from the caller.
pub fn nudge_helper(store: &Store, agent: &str) -> io::Result<()> {
    if !mailbox::is_uuid(agent) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid agent id",
        ));
    }
    touch_rescan(store, agent)?;
    if store.try_lock(LockKind::Helper, agent)?.is_none() {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args(["inbox", "--helper", agent])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

/// `triage inbox --helper <agent>`: exits 0 in every case, logging errors.
pub fn cli_helper(args: &[String]) -> i32 {
    let store = Store::open_default();
    let Some(agent) = args.first().filter(|a| mailbox::is_uuid(a)) else {
        store.log_hook_error("helper", &"missing or invalid agent id");
        return 0;
    };
    let grace = Duration::from_secs(crate::config::Config::load().send.pointer_grace_secs);
    if let Err(e) = run_helper(&store, agent, grace) {
        store.log_hook_error(&format!("helper {agent}"), &e);
    }
    0
}

fn run_helper(store: &Store, agent: &str, grace: Duration) -> io::Result<()> {
    let Some(_lock) = store.try_lock(LockKind::Helper, agent)? else {
        return Ok(());
    };
    loop {
        let pass_started = SystemTime::now();
        helper_pass(store, agent, grace)?;
        if rescan_time(store, agent).is_none_or(|t| t < pass_started) {
            return Ok(());
        }
    }
}

fn helper_pass(store: &Store, agent: &str, grace: Duration) -> io::Result<()> {
    let mailbox = store.mailbox(agent)?;
    let Some((host, record)) = current_host(store, agent) else {
        return Ok(());
    };
    let read_pending = || -> Vec<_> {
        mailbox
            .ids(State::Pending)
            .into_iter()
            .filter_map(|id| mailbox.read(State::Pending, &id).ok())
            .collect()
    };
    let Some(oldest) = read_pending().iter().map(|m| m.created_at_ms).min() else {
        return Ok(());
    };
    let age = Duration::from_millis(mailbox::now_ms().saturating_sub(oldest));
    if age < grace {
        std::thread::sleep(grace - age);
    }
    let pending = read_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let senders: Vec<String> = pending
        .iter()
        .map(|m| mailbox::short_id(&m.from.agent).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let text = pointer_text(pending.len(), &senders);
    match record.provider {
        Provider::Codex => {
            if live_pointer(store, agent) {
                return Ok(());
            }
            let status = Command::new("codex")
                .args([
                    "queue",
                    "--thread",
                    &record.current_session,
                    "--message",
                    &text,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(io::Error::other(format!("codex queue exited {status}")));
            }
            let marker = PointerMarker {
                pid: std::process::id(),
                start: mailbox::proc_info(std::process::id()).map_or(0, |i| i.start),
                thread: record.current_session.clone(),
                created_at_ms: mailbox::now_ms(),
                ttl_secs: POINTER_TTL_SECS,
            };
            fs::create_dir_all(markers_dir(store))?;
            fs::write(pointer_marker(store, agent), serde_json::to_vec(&marker)?)?;
            Ok(())
        }
        Provider::Claude => {
            if store.try_lock(LockKind::Waiter, agent)?.is_none() {
                return Ok(());
            }
            crate::agent_comm::paste_wake_pointer(host.pid, &text).map_err(io::Error::other)
        }
    }
}

/// The live host whose current session belongs to `agent`, most recently
/// active first when the agent is open in more than one.
fn current_host(store: &Store, agent: &str) -> Option<(HostId, HostRecord)> {
    store
        .hosts()
        .into_iter()
        .filter(|(_, record)| {
            store
                .lineage_root(&record.current_session)
                .unwrap_or_else(|| record.current_session.clone())
                == agent
        })
        .filter(|(host, _)| mailbox::liveness(*host) == Liveness::Alive)
        .max_by_key(|(_, record)| record.updated_at_ms)
}
