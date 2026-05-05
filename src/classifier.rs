use std::time::{Duration, SystemTime};

use crate::models::{AttentionState, Session};

const JUST_FINISHED_WINDOW: Duration = Duration::from_secs(3 * 60);
const IDLE_LONG_THRESHOLD: Duration = Duration::from_secs(30 * 60);

pub fn classify(session: &Session, now: SystemTime) -> AttentionState {
    if session.last_stop_had_errors {
        return AttentionState::Error;
    }

    if session.status == "busy" {
        return AttentionState::Working;
    }

    if session.user_prompt_count == 0 && session.headline.is_none() {
        return AttentionState::Fresh;
    }

    if let Some(stop) = session.last_stop_at
        && let Ok(age) = now.duration_since(stop) {
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
        && let Ok(age) = now.duration_since(last) {
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
