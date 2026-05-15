use std::time::{Duration, SystemTime};

use crate::models::{AttentionState, Session};

const JUST_FINISHED_WINDOW: Duration = Duration::from_secs(3 * 60);
const IDLE_LONG_THRESHOLD: Duration = Duration::from_secs(30 * 60);
/// A session whose latest event is older than this is considered abandoned —
/// the user has likely moved on and the row should sink to the bottom even if
/// sessions JSON still says `status=busy` (which lags badly for stale sessions).
const STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

pub fn classify(session: &Session, now: SystemTime) -> AttentionState {
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

    // Stale takes precedence over status=busy because sessions JSON lags 30+
    // min and routinely reports busy on Claude processes that haven't seen
    // activity in days. Without this override, the ux pane (idle 5d, status
    // still busy) would classify as Working.
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

pub fn idle_age(session: &Session, now: SystemTime) -> Option<Duration> {
    let anchor = session.last_stop_at.or(session.last_event_at)?;
    now.duration_since(anchor).ok()
}
