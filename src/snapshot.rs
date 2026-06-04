use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

use crate::models::{Pane, Provider, Session, useful_window_name};
use crate::persist::AliasKey;
use crate::{approval, classifier, codex, discovery, tmux, transcript, ui};

pub fn discover_sessions(
    now: SystemTime,
    digest_cache: &mut transcript::DigestCache,
    codex_cache: &mut codex::CodexDigestCache,
    aliases: &HashMap<AliasKey, String>,
) -> Vec<Session> {
    let panes = tmux::list_panes();
    discover_sessions_with_panes(now, digest_cache, codex_cache, aliases, panes)
}

pub fn discover_sessions_with_panes(
    now: SystemTime,
    digest_cache: &mut transcript::DigestCache,
    codex_cache: &mut codex::CodexDigestCache,
    aliases: &HashMap<AliasKey, String>,
    panes: HashMap<u32, Pane>,
) -> Vec<Session> {
    let mut sessions = discovery::discover_live_sessions();
    let ppid_map = tmux::build_ppid_map();

    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, &ppid_map, 8);
    }
    attach_tmux_fallbacks(&mut sessions, &panes);
    transcript::assign_transcripts(&mut sessions, digest_cache);
    for s in &mut sessions {
        transcript::enrich(s, now, digest_cache);
    }

    let mut codex_sessions = codex::discover_live_sessions(&panes, &ppid_map, codex_cache);
    sessions.append(&mut codex_sessions);

    enrich_permission_prompts(&mut sessions);
    let pending = approval::read_pending();
    approval::attach_to_sessions(pending, &mut sessions);
    scan_blocked_panes(&mut sessions);

    for s in &mut sessions {
        s.state = classifier::classify(s, now);
    }

    digest_cache.evict_missing();
    codex_cache.evict_missing();
    ui::apply_aliases_to_sessions(&mut sessions, aliases);

    dedup_sessions_by_pane(&mut sessions);

    sessions
}

/// Collapse sessions that resolve to the same tmux pane down to one. A pane
/// runs a single interactive program, but newer Claude (2.1.16x) keeps a
/// launcher process + the live session + claimed `--bg-spare` descendants, and
/// stale previous-session JSONs linger after `/clear` — several of which can
/// pid-walk to the same pane and surface as duplicate rows (the "two rows,
/// one pane" bug). Keep the most-recently-updated session (the live one),
/// breaking ties on higher pid; drop the rest. Sessions with no pane are never
/// collapsed (nothing to collide on).
fn dedup_sessions_by_pane(sessions: &mut Vec<Session>) {
    // Best (most recent, then highest-pid) session index per pane id.
    let mut best: HashMap<String, usize> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        let Some(pane) = &s.pane else { continue };
        let rank = (s.updated_at_ms, s.pid);
        match best.get(&pane.pane_id) {
            Some(&b) if (sessions[b].updated_at_ms, sessions[b].pid) >= rank => {}
            _ => {
                best.insert(pane.pane_id.clone(), i);
            }
        }
    }
    let keepers: HashSet<usize> = best.into_values().collect();

    let mut idx = 0;
    sessions.retain(|s| {
        // Pane-less sessions never collide; paned ones survive only if they're
        // the chosen keeper for their pane.
        let keep = s.pane.is_none() || keepers.contains(&idx);
        idx += 1;
        keep
    });
}

pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        // Pinned rows float to the very top regardless of state or mute —
        // `*` is an explicit "keep this visible" override. `b.cmp(a)` so
        // pinned (true) sorts before unpinned (false).
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| a.muted.cmp(&b.muted))
            .then_with(|| a.state.priority().cmp(&b.state.priority()))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });
}

fn attach_tmux_fallbacks(sessions: &mut [Session], panes: &HashMap<u32, Pane>) {
    let mut used = sessions
        .iter()
        .filter_map(|s| s.pane.as_ref().map(|p| p.pane_id.clone()))
        .collect::<HashSet<_>>();

    for session in sessions.iter_mut().filter(|s| s.pane.is_none()) {
        if let Some(pane) = fallback_pane_by_metadata(session, panes, &used) {
            used.insert(pane.pane_id.clone());
            session.pane = Some(pane);
        }
    }
}

fn fallback_pane_by_metadata(
    session: &Session,
    panes: &HashMap<u32, Pane>,
    used: &HashSet<String>,
) -> Option<Pane> {
    if session.provider != Provider::Claude {
        return None;
    }

    if let Some(name) = session.name.as_deref().filter(|n| !n.trim().is_empty()) {
        let candidates = panes
            .values()
            .filter(|pane| !used.contains(&pane.pane_id))
            .filter(|pane| pane.cwd.as_path() == session.cwd.as_path())
            .filter(|pane| useful_window_name(pane).as_deref() == Some(name))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.len() == 1 {
            return candidates.into_iter().next();
        }
    }

    let version = session
        .cli_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())?;
    let candidates = panes
        .values()
        .filter(|pane| !used.contains(&pane.pane_id))
        .filter(|pane| pane.cwd.as_path() == session.cwd.as_path())
        .filter(|pane| pane.current_command == version)
        .cloned()
        .collect::<Vec<_>>();

    (candidates.len() == 1)
        .then(|| candidates.into_iter().next())
        .flatten()
}

fn enrich_permission_prompts(sessions: &mut [Session]) {
    for s in sessions {
        if s.provider == Provider::Claude
            && s.status == "waiting"
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
}

fn scan_blocked_panes(sessions: &mut [Session]) {
    for s in sessions.iter_mut() {
        if s.provider == Provider::Claude
            && s.status == "busy"
            && s.pending_approvals.is_empty()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane_tail(&pane.target, 15)
            && tmux::has_pending_permission_prompt(&content)
        {
            s.pane_blocked = true;
        }
    }

    for s in sessions {
        if s.provider == Provider::Codex
            && s.status == "busy"
            && s.approval_prompt_pending
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane_tail(&pane.target, 40)
            && tmux::has_codex_permission_prompt(&content)
        {
            s.pane_blocked = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AttentionState;
    use std::path::PathBuf;

    fn claude_session(cwd: &str, name: Option<&str>, version: Option<&str>) -> Session {
        let mut session = Session::new(
            Provider::Claude,
            99658,
            "sid".to_string(),
            PathBuf::from(cwd),
            name.map(str::to_string),
            "idle".to_string(),
            0,
            0,
            None,
        );
        session.cli_version = version.map(str::to_string);
        session
    }

    fn pane(pid: u32, pane_id: &str, cwd: &str, command: &str, window_name: &str) -> Pane {
        Pane {
            target: format!("main:{pid}.0"),
            tmux_session: "main".to_string(),
            window_name: window_name.to_string(),
            pane_id: pane_id.to_string(),
            pid,
            tty: "/dev/ttys001".to_string(),
            current_command: command.to_string(),
            cwd: PathBuf::from(cwd),
            active: false,
        }
    }

    fn pane_map(panes: Vec<Pane>) -> HashMap<u32, Pane> {
        panes.into_iter().map(|pane| (pane.pid, pane)).collect()
    }

    fn paned_session(cwd: &str, pid: u32, pane_id: &str, updated_at_ms: u64) -> Session {
        let mut s = Session::new(
            Provider::Claude,
            pid,
            format!("sid-{pid}"),
            PathBuf::from(cwd),
            None,
            "idle".to_string(),
            0,
            updated_at_ms,
            None,
        );
        s.pane = Some(pane(pid, pane_id, cwd, "claude", "win"));
        s
    }

    #[test]
    fn dedup_keeps_most_recent_session_per_pane() {
        // Two sessions pid-walked to the same pane (launcher + claimed spare);
        // the more-recently-updated one wins, the stale one is dropped.
        let mut sessions = vec![
            paned_session("/repo/ux", 30907, "%428", 1_000),
            paned_session("/repo/ux", 31903, "%428", 2_000),
        ];
        dedup_sessions_by_pane(&mut sessions);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pid, 31903);
    }

    #[test]
    fn dedup_leaves_distinct_panes_and_paneless_alone() {
        let a = paned_session("/repo/ux", 1, "%1", 10);
        let b = paned_session("/repo/ux", 2, "%2", 20);
        // A pane-less session must never be collapsed away.
        let mut c = paned_session("/repo/other", 3, "%3", 30);
        c.pane = None;
        let mut sessions = vec![a, b, c];
        dedup_sessions_by_pane(&mut sessions);
        assert_eq!(sessions.len(), 3);
    }

    #[test]
    fn pinned_sessions_sort_to_the_top() {
        // A pinned, low-priority (Stale) row must outrank an unpinned
        // high-priority (Blocked) one, and a pinned+muted row must still beat
        // unpinned rows (pin overrides mute).
        let mut blocked = claude_session("/repo/a", None, None);
        blocked.state = AttentionState::Blocked;

        let mut pinned_stale = claude_session("/repo/b", None, None);
        pinned_stale.state = AttentionState::Stale;
        pinned_stale.pinned = true;

        let mut pinned_muted = claude_session("/repo/c", None, None);
        pinned_muted.state = AttentionState::IdleLong;
        pinned_muted.pinned = true;
        pinned_muted.muted = true;

        let mut sessions = vec![blocked, pinned_stale, pinned_muted];
        sort_sessions(&mut sessions);

        assert!(sessions[0].pinned, "first row should be pinned");
        assert!(sessions[1].pinned, "second row should be pinned");
        assert!(!sessions[2].pinned, "unpinned Blocked row sinks below pins");
        assert_eq!(sessions[2].state, AttentionState::Blocked);
    }

    #[test]
    fn fallback_attaches_by_unique_cwd_and_cli_version() {
        let mut sessions = vec![claude_session("/repo/ux", None, Some("2.1.158"))];
        let panes = pane_map(vec![
            pane(98644, "%311", "/repo/ux", "2.1.158", "agent-ACDC-21"),
            pane(123, "%123", "/repo/other", "2.1.158", "other-agent"),
        ]);

        attach_tmux_fallbacks(&mut sessions, &panes);

        assert_eq!(
            sessions[0].pane.as_ref().map(|pane| pane.pane_id.as_str()),
            Some("%311")
        );
    }

    #[test]
    fn fallback_attaches_by_unique_window_name_for_named_sessions() {
        let mut sessions = vec![claude_session(
            "/repo/ux",
            Some("agent-ACDC-26"),
            Some("2.1.158"),
        )];
        let panes = pane_map(vec![
            pane(76788, "%322", "/repo/ux", "fish", "agent-ACDC-26"),
            pane(98644, "%311", "/repo/ux", "2.1.158", "agent-ACDC-21"),
        ]);

        attach_tmux_fallbacks(&mut sessions, &panes);

        assert_eq!(
            sessions[0].pane.as_ref().map(|pane| pane.pane_id.as_str()),
            Some("%322")
        );
    }

    #[test]
    fn fallback_skips_ambiguous_cwd_and_cli_version() {
        let mut sessions = vec![claude_session("/repo/ux", None, Some("2.1.156"))];
        let panes = pane_map(vec![
            pane(1, "%1", "/repo/ux", "2.1.156", "first"),
            pane(2, "%2", "/repo/ux", "2.1.156", "second"),
        ]);

        attach_tmux_fallbacks(&mut sessions, &panes);

        assert!(sessions[0].pane.is_none());
    }
}
