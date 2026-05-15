use std::time::{Duration, SystemTime};

use crate::models::{AttentionState, Session};

const JUST_FINISHED_WINDOW: Duration = Duration::from_secs(3 * 60);
const IDLE_LONG_THRESHOLD: Duration = Duration::from_secs(30 * 60);
/// A session whose latest event is older than this is considered abandoned —
/// the user has likely moved on and the row should sink to the bottom even if
/// sessions JSON still says `status=busy` (which lags badly for stale sessions).
const STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);
/// `status=busy` with no transcript activity for this long AND no pending
/// tool_use → classify as Blocked. Catches "silently stuck" sessions where
/// the hook decision file piled up under auto mode, or Claude Code missed
/// a `status=waiting` transition. The `last_tool_use.is_none()` gate is
/// what makes this safe vs the old pre-`ec5ff7c` 90s rule that false-fired
/// on long Bash / cargo builds — a mid-execution tool keeps last_tool_use
/// set to its un-completed entry.
const STUCK_BUSY_THRESHOLD: Duration = Duration::from_secs(5 * 60);

pub fn classify(session: &Session, now: SystemTime) -> AttentionState {
    if session.last_stop_had_errors {
        return AttentionState::Error;
    }

    // Sessions JSON `status=waiting` is the canonical "user attention needed"
    // signal (Claude Code 2.1.13x+ sets this when it pauses on a permission
    // prompt). We deliberately do NOT use `pending_approvals` alone: our
    // PreToolUse hook fires for every tool call (including auto-approved
    // Reads/Edits/etc.), so a pending file just means the hook is blocking
    // — not that Claude is actually waiting on a user decision. Pending files
    // still feed into the headline + `a`/`d` actions, but only when status
    // also says waiting.
    if session.status == "waiting" {
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
        // Stuck-busy detection: Claude says busy, but the transcript has
        // been silent for STUCK_BUSY_THRESHOLD AND there's no pending
        // tool_use to explain the silence (i.e. no long Bash/build is
        // mid-execution). Most likely cause: auto-mode hook decision-file
        // pile-up, or Claude Code missed a `status=waiting` transition.
        // Surfacing the row as Blocked gives the user a signal to look.
        if session.last_tool_use.is_none()
            && let Some(age) = event_age
            && age >= STUCK_BUSY_THRESHOLD
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
