//! Settles claims whose outcome wasn't recorded at delivery time, and returns
//! mail for agents whose session is gone. Cheap enough to run on every triage
//! command; hooks run it for their own agent only, and it never spawns a
//! process.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::mailbox::{
    self, Claim, DeliveredVia, HostId, Inflight, Liveness, Mailbox, Message, State, Store,
};

const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 2 * 1024 * 1024;
const STALE_TMP_MS: u64 = 60_000;
const UNSIGNED_CLAIM_MS: u64 = 60_000;
const IDLE_WAITERLESS_CLAIM_MS: u64 = 30_000;
const DEAD_HOST_BOUNCE_MS: u64 = 30_000;

/// What the transcript after a claim says about it. Built only when the scan
/// completed; an incomplete scan must never be read as "not delivered".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptFacts {
    pub rewake_seen: bool,
    /// A turn began after the mail was printed and has since ended, so the
    /// wake would have been folded in by now.
    pub later_turn_ended: bool,
    pub turn_running: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Commit,
    /// Back to pending as a labelled re-delivery: it may have been shown.
    Revert,
    /// Back to pending unlabelled: it was never shown.
    Release,
    Keep,
}

pub struct ClaimView<'a> {
    pub claim: Option<&'a Claim>,
    pub inflight_age_ms: u64,
    pub now_ms: u64,
    pub host: Liveness,
    pub claimer: Liveness,
    pub facts: Option<TranscriptFacts>,
}

pub fn verdict(view: &ClaimView) -> Verdict {
    let Some(claim) = view.claim else {
        return if view.inflight_age_ms > UNSIGNED_CLAIM_MS {
            Verdict::Release
        } else {
            Verdict::Keep
        };
    };
    let waiter_claim = !claim.transcript_path.is_empty();
    if claim.printed_at_ms.is_none() {
        let gone = view.claimer == Liveness::Dead || view.host == Liveness::Dead;
        return match (gone, waiter_claim) {
            (false, _) => Verdict::Keep,
            (true, true) => Verdict::Release,
            (true, false) => Verdict::Revert,
        };
    }
    let Some(facts) = view.facts else {
        return Verdict::Keep;
    };
    if facts.rewake_seen {
        return Verdict::Commit;
    }
    let old = view.now_ms.saturating_sub(claim.claimed_at_ms) > IDLE_WAITERLESS_CLAIM_MS;
    if view.host == Liveness::Dead
        || facts.later_turn_ended
        || (view.claimer == Liveness::Dead && !facts.turn_running && old)
    {
        Verdict::Revert
    } else {
        Verdict::Keep
    }
}

/// `None` when the transcript can't be read in full within the scan budget.
pub fn scan(path: &Path, offset: u64, marker: &str, printed_at_ms: u64) -> Option<TranscriptFacts> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = offset.min(len);
    if len - start > MAX_TRANSCRIPT_SCAN_BYTES {
        return None;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut facts = TranscriptFacts::default();
    let mut turn_started_after_print = false;
    let mut last_was_turn_end = false;
    for line in BufReader::new(file).lines() {
        let line = line.ok()?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        if rewake_text(&record)
            .is_some_and(|t| t.starts_with("<task-notification>") && t.contains(marker))
        {
            facts.rewake_seen = true;
        }
        let at_ms = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::transcript::parse_timestamp)
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        let turn_end = kind == "system"
            && record.get("subtype").and_then(Value::as_str) == Some("turn_duration");
        if kind == "user" && at_ms.is_some_and(|t| t > printed_at_ms) {
            turn_started_after_print = true;
        }
        if turn_end && turn_started_after_print {
            facts.later_turn_ended = true;
        }
        if kind == "user" || kind == "assistant" || turn_end {
            last_was_turn_end = turn_end;
        }
    }
    facts.turn_running = !last_was_turn_end;
    Some(facts)
}

/// The model-visible text of a Claude rewake: a user record when the session
/// was idle, a `queued_command` attachment when it was folded into a running
/// turn.
pub fn rewake_text(record: &Value) -> Option<&str> {
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

fn claimer_liveness(claim: &Claim) -> Liveness {
    if claim.waiter_start != 0 {
        return mailbox::liveness(HostId {
            pid: claim.waiter_pid,
            start: claim.waiter_start,
        });
    }
    let alive = unsafe { libc::kill(claim.waiter_pid as libc::pid_t, 0) } == 0;
    if alive || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
        Liveness::Alive
    } else {
        Liveness::Dead
    }
}

/// After `/clear` the wake may land in the new session's transcript, which
/// sits beside the old one.
fn current_transcript(store: &Store, inflight: &Inflight, claim: &Claim) -> Option<PathBuf> {
    let current = store.read_host(inflight.host)?.current_session;
    let path = Path::new(&claim.transcript_path)
        .parent()?
        .join(format!("{current}.jsonl"));
    (path.as_os_str() != claim.transcript_path.as_str()).then_some(path)
}

fn facts_for(store: &Store, inflight: &Inflight, claim: &Claim) -> Option<TranscriptFacts> {
    let printed = claim.printed_at_ms?;
    if claim.transcript_path.is_empty() {
        return None;
    }
    let marker = format!("[id={} n={}", inflight.id, claim.nonce);
    let mut facts = scan(
        Path::new(&claim.transcript_path),
        claim.transcript_offset,
        &marker,
        printed,
    )?;
    if !facts.rewake_seen
        && let Some(current) = current_transcript(store, inflight, claim)
        && let Some(later) = scan(&current, 0, &marker, printed)
    {
        facts.rewake_seen = later.rewake_seen;
        facts.later_turn_ended |= later.later_turn_ended;
        facts.turn_running = later.turn_running;
    }
    Some(facts)
}

pub fn reconcile_agent(store: &Store, agent: &str) -> io::Result<()> {
    let mailbox = store.mailbox(agent)?;
    mailbox.sweep_tmp(STALE_TMP_MS);
    let now = mailbox::now_ms();
    for inflight in mailbox.inflight() {
        let claim = mailbox.read_claim(&inflight);
        let view = ClaimView {
            claim: claim.as_ref(),
            inflight_age_ms: mailbox.inflight_age_ms(&inflight).unwrap_or(0),
            now_ms: now,
            host: mailbox::liveness(inflight.host),
            claimer: claim.as_ref().map_or(Liveness::Unknown, claimer_liveness),
            facts: claim.as_ref().and_then(|c| facts_for(store, &inflight, c)),
        };
        let settled = match verdict(&view) {
            Verdict::Keep => Ok(()),
            Verdict::Commit => {
                let to = if claim.as_ref().is_some_and(|c| c.head_only) {
                    State::Notified
                } else {
                    State::Delivered
                };
                mailbox.commit(&inflight, to, DeliveredVia::ClaudeWaiter, now)
            }
            Verdict::Revert => mailbox.revert(&inflight),
            Verdict::Release => mailbox.release(&inflight),
        };
        match settled {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Every agent's claims, then mail for hosts that have stayed dead.
pub fn reconcile_all(store: &Store) -> io::Result<()> {
    for agent in store.agents() {
        reconcile_agent(store, &agent)?;
    }
    bounce_for_dead_hosts(store)
}

fn dead_marker(store: &Store, host: HostId) -> PathBuf {
    store
        .root()
        .join("hosts")
        .join(format!("{}-{}.dead", host.pid, host.start))
}

fn bounce_for_dead_hosts(store: &Store) -> io::Result<()> {
    let now = mailbox::now_ms();
    let hosts = store.hosts();
    let agent_of = |current: &str| {
        store
            .lineage_root(current)
            .unwrap_or_else(|| current.to_string())
    };
    for (host, record) in &hosts {
        let marker = dead_marker(store, *host);
        match mailbox::liveness(*host) {
            Liveness::Alive => {
                let _ = fs::remove_file(&marker);
                continue;
            }
            Liveness::Unknown => continue,
            Liveness::Dead => {}
        }
        let dead_since = fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let Some(dead_since) = dead_since else {
            fs::write(&marker, now.to_string())?;
            continue;
        };
        if now.saturating_sub(dead_since) < DEAD_HOST_BOUNCE_MS {
            continue;
        }
        let agent = agent_of(&record.current_session);
        let live_elsewhere = hosts.iter().any(|(other, r)| {
            other != host
                && agent_of(&r.current_session) == agent
                && mailbox::liveness(*other) != Liveness::Dead
        });
        if !live_elsewhere {
            bounce_pending(store, &store.mailbox(&agent)?, &agent, record.provider)?;
        }
        store.remove_host(*host)?;
        let _ = fs::remove_file(&marker);
    }
    Ok(())
}

fn bounce_pending(
    store: &Store,
    mailbox: &Mailbox,
    agent: &str,
    provider: crate::models::Provider,
) -> io::Result<()> {
    for id in mailbox.ids(State::Pending) {
        let Ok(msg) = mailbox.read(State::Pending, &id) else {
            continue;
        };
        if !mailbox.move_state(&id, State::Pending, State::Undeliverable)? {
            continue;
        }
        if msg.bounce_of.is_none() {
            store
                .mailbox(&msg.from.agent)?
                .enqueue(&bounce(&msg, agent, provider))?;
        }
    }
    Ok(())
}

/// Never echoes the original body, and is never bounced itself.
fn bounce(original: &Message, agent: &str, provider: crate::models::Provider) -> Message {
    Message {
        v: 1,
        id: mailbox::new_message_id(),
        created_at_ms: mailbox::now_ms(),
        from: mailbox::Sender {
            agent: agent.to_string(),
            session: original.to.session_at_send.clone(),
            provider,
            label: "triage".to_string(),
        },
        to: mailbox::Recipient {
            agent: original.from.agent.clone(),
            session_at_send: original.from.session.clone(),
        },
        body: format!(
            "Undeliverable: your message {} to agent {} was not delivered because that session ended first. It will not be re-delivered; send it again if it still matters.",
            original.id,
            mailbox::short_id(agent)
        ),
        attempt: 0,
        bounce_of: Some(original.id.clone()),
        delivered_at_ms: None,
        delivered_via: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_100_000;

    fn claim(transcript: &str, printed: Option<u64>) -> Claim {
        Claim {
            nonce: "0123456789abcdef".to_string(),
            claimed_at_ms: NOW - 40_000,
            waiter_pid: 1,
            waiter_start: 1,
            transcript_path: transcript.to_string(),
            transcript_offset: 0,
            printed_at_ms: printed,
            head_only: false,
        }
    }

    fn view(claim: Option<&Claim>) -> ClaimView<'_> {
        ClaimView {
            claim,
            inflight_age_ms: 1_000,
            now_ms: NOW,
            host: Liveness::Alive,
            claimer: Liveness::Alive,
            facts: Some(TranscriptFacts {
                rewake_seen: false,
                later_turn_ended: false,
                turn_running: false,
            }),
        }
    }

    #[test]
    fn a_claim_without_its_sidecar_is_released_once_old() {
        assert_eq!(verdict(&view(None)), Verdict::Keep);
        let old = ClaimView {
            inflight_age_ms: 61_000,
            ..view(None)
        };
        assert_eq!(verdict(&old), Verdict::Release);
    }

    #[test]
    fn an_unprinted_claim_is_released_for_a_waiter_but_reverted_for_a_drain() {
        let waiter = claim("/t.jsonl", None);
        let drain = claim("", None);
        assert_eq!(verdict(&view(Some(&waiter))), Verdict::Keep);
        let dead = |c| ClaimView {
            claimer: Liveness::Dead,
            ..view(Some(c))
        };
        assert_eq!(verdict(&dead(&waiter)), Verdict::Release);
        assert_eq!(verdict(&dead(&drain)), Verdict::Revert);
    }

    #[test]
    fn a_printed_claim_follows_the_transcript() {
        let printed = claim("/t.jsonl", Some(NOW - 35_000));
        let with = |facts: Option<TranscriptFacts>, host, claimer| ClaimView {
            facts,
            host,
            claimer,
            ..view(Some(&printed))
        };
        let quiet = TranscriptFacts::default();
        let seen = TranscriptFacts {
            rewake_seen: true,
            ..quiet
        };
        assert_eq!(
            verdict(&with(None, Liveness::Dead, Liveness::Dead)),
            Verdict::Keep
        );
        assert_eq!(
            verdict(&with(Some(seen), Liveness::Dead, Liveness::Dead)),
            Verdict::Commit
        );
        assert_eq!(
            verdict(&with(Some(quiet), Liveness::Dead, Liveness::Alive)),
            Verdict::Revert
        );
        let ended = TranscriptFacts {
            later_turn_ended: true,
            ..quiet
        };
        assert_eq!(
            verdict(&with(Some(ended), Liveness::Alive, Liveness::Alive)),
            Verdict::Revert
        );
        assert_eq!(
            verdict(&with(Some(quiet), Liveness::Alive, Liveness::Dead)),
            Verdict::Revert
        );
        let running = TranscriptFacts {
            turn_running: true,
            ..quiet
        };
        assert_eq!(
            verdict(&with(Some(running), Liveness::Alive, Liveness::Dead)),
            Verdict::Keep
        );
        assert_eq!(
            verdict(&with(Some(quiet), Liveness::Alive, Liveness::Alive)),
            Verdict::Keep
        );
    }

    #[test]
    fn a_waiterless_claim_is_not_reverted_before_30s() {
        let mut recent = claim("/t.jsonl", Some(NOW - 5_000));
        recent.claimed_at_ms = NOW - 5_000;
        let v = ClaimView {
            claimer: Liveness::Dead,
            ..view(Some(&recent))
        };
        assert_eq!(verdict(&v), Verdict::Keep);
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tri149")
            .join(name)
    }

    /// The captured idle rewake predates the current header, so match the
    /// part of the marker it shares.
    const FIXTURE_MARKER: &str = "id=01900000-0000-7000-8000-000000000001 n=0123456789abcdef";

    #[test]
    fn scan_sees_idle_and_mid_turn_rewakes() {
        let printed = 1_790_396_270_000;
        let idle = scan(&fixture("claude-rewake.jsonl"), 0, FIXTURE_MARKER, printed).unwrap();
        assert!(idle.rewake_seen);
        assert!(idle.later_turn_ended);
        assert!(!idle.turn_running);
        let wrong_nonce = scan(
            &fixture("claude-rewake.jsonl"),
            0,
            "id=01900000-0000-7000-8000-000000000001 n=ffffffffffffffff",
            printed,
        )
        .unwrap();
        assert!(!wrong_nonce.rewake_seen);
        let midturn = fs::read_to_string(fixture("claude-rewake-midturn.jsonl")).unwrap();
        let marker = midturn
            .split("[id=")
            .nth(1)
            .map(|rest| format!("[id={}", &rest[..36 + 20]))
            .unwrap();
        let seen = scan(&fixture("claude-rewake-midturn.jsonl"), 0, &marker, 0).unwrap();
        assert!(seen.rewake_seen, "{marker}");
    }

    #[test]
    fn an_oversized_scan_is_unknown_not_undelivered() {
        let path = std::env::temp_dir().join(format!("triage-scan-big-{}", std::process::id()));
        fs::write(&path, vec![b'\n'; (MAX_TRANSCRIPT_SCAN_BYTES + 1) as usize]).unwrap();
        assert_eq!(scan(&path, 0, FIXTURE_MARKER, 0), None);
        let _ = fs::remove_file(path);
    }
}
