use std::time::{Duration, SystemTime};

use crate::models::{AttentionState, Provider, Session};

const JUST_FINISHED_WINDOW: Duration = Duration::from_secs(3 * 60);
const IDLE_LONG_THRESHOLD: Duration = Duration::from_secs(30 * 60);
/// A session whose latest event is older than this is considered abandoned —
/// the user has likely moved on and the row should sink to the bottom even if
/// sessions JSON still says `status=busy` (which lags badly for stale sessions).
const STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

fn has_fresh_busy_evidence(session: &Session, now: SystemTime) -> bool {
    if session.status != "busy" || session.pane.is_none() {
        return false;
    }

    let Some(updated_at) =
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(session.updated_at_ms))
    else {
        return false;
    };
    now.duration_since(updated_at)
        .is_ok_and(|age| age <= IDLE_LONG_THRESHOLD)
}

pub fn classify(session: &Session, now: SystemTime) -> AttentionState {
    if session.provider == Provider::Codex {
        return classify_codex(session, now);
    }

    if session.last_stop_had_errors {
        return AttentionState::Error;
    }

    // Two-signal Blocked detection:
    //   1. Sessions JSON `status=waiting` — Claude Code's own canonical
    //      signal. Cheapest path, accurate when set, but routinely missed:
    //      observed cases where the native permission UI was visibly up for
    //      minutes while status stayed `busy`. We can't fix that from
    //      outside Claude Code, so we layer a second signal underneath.
    //   2. `pane_blocked` — set in the refresh pass when the pane content
    //      shows the `1. Yes`/`2. No` permission UI anchor. Deterministic
    //      ground truth from the pixels Claude actually drew; survives the
    //      hook's 3-s timeout (after which the hook file is gone and only
    //      the pane tells us the user is still being asked).
    if session.status == "waiting" || session.pane_blocked {
        return AttentionState::Blocked;
    }

    // Real activity age, used for the Stale check below. last_stop_at is the
    // strongest signal (turn ended); fall back to last_event_at otherwise.
    let event_age = session
        .last_stop_at
        .or(session.last_event_at)
        .and_then(|t| now.duration_since(t).ok());

    // A live pane plus recently refreshed provider metadata is stronger
    // evidence than an old transcript while Claude reports an in-flight turn.
    // Keep the freshness window bounded because both status and updatedAt can
    // stop changing while an abandoned process remains alive.
    if has_fresh_busy_evidence(session, now) {
        return AttentionState::Working;
    }

    // Stale normally takes precedence over status=busy because sessions JSON
    // can remain busy for days. The fresh-metadata exception above is bounded,
    // so abandoned processes still sink once their provider heartbeat ages.
    if let Some(age) = event_age
        && age >= STALE_THRESHOLD
    {
        return AttentionState::Stale;
    }

    if session.status == "busy" {
        return AttentionState::Working;
    }

    if session.user_prompt_count == 0 && session.headline.is_none() {
        return AttentionState::Fresh;
    }

    if let Some(stop) = session.last_stop_at
        && let Ok(age) = now.duration_since(stop)
    {
        if age <= JUST_FINISHED_WINDOW {
            return AttentionState::JustFinished;
        }
        if age >= IDLE_LONG_THRESHOLD {
            return AttentionState::IdleLong;
        }
        return AttentionState::IdleShort;
    }

    // No stop yet observed; fall back to last_event_at. Newer Claude Code
    // (2.1.13x+) doesn't emit `stop_hook_summary`, so this is the common
    // path. Treat sessions JSON `status=idle` itself as the implicit
    // turn-end signal: any recent event on an idle session means the turn
    // just ended (otherwise the earlier `status=busy` branch would have
    // caught it as Working).
    if let Some(last) = session.last_event_at
        && let Ok(age) = now.duration_since(last)
    {
        if age <= JUST_FINISHED_WINDOW {
            return AttentionState::JustFinished;
        }
        if age >= IDLE_LONG_THRESHOLD {
            return AttentionState::IdleLong;
        }
        return AttentionState::IdleShort;
    }

    AttentionState::Unknown
}

fn classify_codex(session: &Session, now: SystemTime) -> AttentionState {
    if session.pane_blocked {
        return AttentionState::Blocked;
    }

    let event_age = session
        .last_event_at
        .and_then(|t| now.duration_since(t).ok());
    if let Some(age) = event_age
        && age >= STALE_THRESHOLD
    {
        return AttentionState::Stale;
    }

    if session.status == "busy" {
        return AttentionState::Working;
    }

    if session.user_prompt_count == 0 && session.headline.is_none() {
        return AttentionState::Fresh;
    }

    if let Some(age) = event_age {
        if age <= JUST_FINISHED_WINDOW {
            return AttentionState::JustFinished;
        }
        if age >= IDLE_LONG_THRESHOLD {
            return AttentionState::IdleLong;
        }
        return AttentionState::IdleShort;
    }

    AttentionState::Unknown
}

pub fn idle_age(session: &Session, now: SystemTime) -> Option<Duration> {
    let anchor = session.last_stop_at.or(session.last_event_at)?;
    now.duration_since(anchor).ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::models::{Pane, Provider};

    fn busy_claude(now: SystemTime, metadata_age: Duration, with_pane: bool) -> Session {
        let updated_at_ms = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("test time is after the epoch")
            .saturating_sub(metadata_age)
            .as_millis() as u64;
        let mut session = Session::new(
            Provider::Claude,
            42,
            "session".to_string(),
            PathBuf::from("/repo"),
            Some("agent".to_string()),
            "busy".to_string(),
            0,
            updated_at_ms,
            None,
        );
        session.last_event_at = Some(now - STALE_THRESHOLD - Duration::from_secs(1));
        session.user_prompt_count = 1;
        if with_pane {
            session.pane = Some(Pane {
                target: "main:1.0".to_string(),
                tmux_session: "main".to_string(),
                window_name: "agent".to_string(),
                pane_id: "%1".to_string(),
                pid: 42,
                tty: "/dev/ttys001".to_string(),
                current_command: "claude".to_string(),
                cwd: PathBuf::from("/repo"),
                active: true,
            });
        }
        session
    }

    #[test]
    fn fresh_busy_metadata_keeps_paned_claude_working() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let session = busy_claude(now, Duration::from_secs(10 * 60), true);

        assert_eq!(classify(&session, now), AttentionState::Working);
    }

    #[test]
    fn stale_busy_metadata_does_not_hide_abandoned_session() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let session = busy_claude(now, IDLE_LONG_THRESHOLD + Duration::from_millis(1), true);

        assert_eq!(classify(&session, now), AttentionState::Stale);
    }

    #[test]
    fn busy_metadata_at_freshness_boundary_is_working() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let session = busy_claude(now, IDLE_LONG_THRESHOLD, true);

        assert_eq!(classify(&session, now), AttentionState::Working);
    }

    #[test]
    fn fresh_busy_metadata_requires_a_tmux_pane() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let session = busy_claude(now, Duration::from_secs(10 * 60), false);

        assert_eq!(classify(&session, now), AttentionState::Stale);
    }

    #[test]
    fn future_busy_metadata_does_not_defeat_stale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
        let mut session = busy_claude(now, Duration::ZERO, true);
        session.updated_at_ms += 1;

        assert_eq!(classify(&session, now), AttentionState::Stale);
    }
}
