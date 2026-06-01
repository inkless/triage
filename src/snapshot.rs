use std::collections::HashMap;
use std::time::SystemTime;

use crate::models::{Provider, Session};
use crate::persist::AliasKey;
use crate::{approval, classifier, codex, discovery, tmux, transcript, ui};

pub fn discover_sessions(
    now: SystemTime,
    digest_cache: &mut transcript::DigestCache,
    codex_cache: &mut codex::CodexDigestCache,
    aliases: &HashMap<AliasKey, String>,
) -> Vec<Session> {
    let mut sessions = discovery::discover_live_sessions();
    let panes = tmux::list_panes();
    let ppid_map = tmux::build_ppid_map();

    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, &ppid_map, 8);
    }
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

    sessions
}

pub fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        a.muted
            .cmp(&b.muted)
            .then_with(|| a.state.priority().cmp(&b.state.priority()))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });
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
