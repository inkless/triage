use std::path::PathBuf;
use std::time::SystemTime;

use crate::approval::PendingApproval;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Pane {
    pub target: String,
    pub pid: u32,
    pub tty: String,
    pub current_command: String,
    pub cwd: PathBuf,
    pub active: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Session {
    pub pid: u32,
    pub session_id: String,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub status: String,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,

    pub pane: Option<Pane>,

    pub transcript_path: Option<PathBuf>,
    pub headline: Option<String>,
    pub last_prompt: Option<String>,
    pub last_prompt_at: Option<SystemTime>,
    pub last_turn_duration_ms: Option<u64>,
    pub last_turn_msg_count: Option<u64>,
    pub last_event_at: Option<SystemTime>,
    pub last_stop_at: Option<SystemTime>,
    pub user_prompt_count: u64,
    pub last_stop_had_errors: bool,

    pub state: AttentionState,
    /// True when the user has muted this session. Muted sessions still update
    /// in the background but render dimmed and sort to the bottom of the list.
    pub muted: bool,
    /// Pending tool-use approval requests from the PreToolUse hook. Newest
    /// last. When non-empty, the session is forced to `Blocked`.
    pub pending_approvals: Vec<PendingApproval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionState {
    Error,
    /// Claude is in a turn (status=busy) but no transcript events have fired
    /// for a while — most likely waiting on the user for tool approval.
    Blocked,
    JustFinished,
    Working,
    Fresh,
    IdleShort,
    IdleLong,
    /// Idle long enough that the user has very likely moved on. Distinct from
    /// IdleLong so we can deprioritize without hiding outright.
    Stale,
    Unknown,
}

impl AttentionState {
    pub fn priority(self) -> u8 {
        // Error and Blocked both need user attention immediately. Fresh
        // sessions (newly opened, no activity yet) rank below idle ones — an
        // idle session has context worth resuming; a fresh one is empty.
        // Stale (idle >24h) sinks below everything except unknown.
        match self {
            AttentionState::Error => 0,
            AttentionState::Blocked => 1,
            AttentionState::JustFinished => 2,
            AttentionState::Working => 3,
            AttentionState::IdleShort => 4,
            AttentionState::IdleLong => 5,
            AttentionState::Fresh => 6,
            AttentionState::Stale => 7,
            AttentionState::Unknown => 8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AttentionState::Error => "error",
            AttentionState::Blocked => "block",
            AttentionState::JustFinished => "done",
            AttentionState::Working => "work",
            AttentionState::Fresh => "fresh",
            AttentionState::IdleShort => "idle",
            AttentionState::IdleLong => "long",
            AttentionState::Stale => "stale",
            AttentionState::Unknown => "?",
        }
    }
}
