use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Pane {
    pub target: String,
    pub pid: u32,
    pub tty: String,
    pub current_command: String,
    pub cwd: PathBuf,
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
    pub last_turn_duration_ms: Option<u64>,
    pub last_turn_msg_count: Option<u64>,
    pub last_event_at: Option<SystemTime>,
    pub last_stop_at: Option<SystemTime>,
    pub user_prompt_count: u64,
    pub last_stop_had_errors: bool,

    pub state: AttentionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionState {
    Error,
    JustFinished,
    Working,
    Fresh,
    IdleShort,
    IdleLong,
    Unknown,
}

impl AttentionState {
    pub fn priority(self) -> u8 {
        match self {
            AttentionState::Error => 0,
            AttentionState::JustFinished => 1,
            AttentionState::Working => 2,
            AttentionState::Fresh => 3,
            AttentionState::IdleShort => 4,
            AttentionState::IdleLong => 5,
            AttentionState::Unknown => 6,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AttentionState::Error => "error",
            AttentionState::JustFinished => "done",
            AttentionState::Working => "work",
            AttentionState::Fresh => "fresh",
            AttentionState::IdleShort => "idle",
            AttentionState::IdleLong => "long",
            AttentionState::Unknown => "?",
        }
    }
}
