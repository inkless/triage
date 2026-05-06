use std::time::{Duration, SystemTime};

use crate::models::{AttentionState, Session};

const JUST_FINISHED_WINDOW: Duration = Duration::from_secs(3 * 60);
const IDLE_LONG_THRESHOLD: Duration = Duration::from_secs(30 * 60);
/// A session whose latest event is older than this is considered abandoned —
/// the user has likely moved on and the row should sink to the bottom even if
/// sessions JSON still says `status=busy` (which lags badly for stale sessions).
const STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);
/// While `status=busy`, an event gap exceeding this is most likely Claude
/// pausing on a permission prompt rather than running a long tool. False
/// positive risk: very long-running shell commands. Acceptable for v1; the
/// proper signal needs hooks (T-19).
const BLOCKED_THRESHOLD: Duration = Duration::from_secs(90);

pub fn classify(session: &Session, now: SystemTime) -> AttentionState {
    if session.last_stop_had_errors {
        return AttentionState::Error;
    }

    // A pending tool-use approval is the strongest possible "needs your input"
    // signal — Claude is literally hung waiting on the user. Classify Blocked
    // even if the busy-and-quiet heuristic wouldn't have caught it.
    if !session.pending_approvals.is_empty() {
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
        // Distinguish active work from "stuck waiting on user." When Claude
        // is generating or running a tool, events fire frequently; a 90s gap
        // mid-turn is most likely a permission prompt the user hasn't answered.
        if let Some(age) = event_age
            && age >= BLOCKED_THRESHOLD
        {
            return AttentionState::Blocked;
        }
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

    // No stop yet observed; fall back to last_event_at.
    if let Some(last) = session.last_event_at
        && let Ok(age) = now.duration_since(last)
    {
        if age >= IDLE_LONG_THRESHOLD {
            return AttentionState::IdleLong;
        }
        if age >= JUST_FINISHED_WINDOW {
            return AttentionState::IdleShort;
        }
    }

    AttentionState::Unknown
}

pub fn idle_age(session: &Session, now: SystemTime) -> Option<Duration> {
    let anchor = session.last_stop_at.or(session.last_event_at)?;
    now.duration_since(anchor).ok()
}
