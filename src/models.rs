use std::path::PathBuf;
use std::time::SystemTime;

use crate::approval::PendingApproval;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Pane {
    pub target: String,
    /// `#{session_name}` portion of `target`. Split out here so consumers
    /// (UI, notify, jump) don't all re-parse the same colon-separated string.
    pub tmux_session: String,
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
    /// `waitingFor` from sessions JSON — populated when `status == "waiting"`
    /// (e.g. `"approve Bash"`). This is Claude Code's own canonical signal that
    /// the session is paused on a permission prompt.
    pub waiting_for: Option<String>,
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
    /// (tool_name, one-line input brief) of the latest assistant `tool_use`
    /// event in the transcript. When `status == "waiting"`, this is the
    /// specific tool call Claude is asking permission for.
    pub last_tool_use: Option<(String, String)>,

    /// Approximate cumulative session cost in USD, summed from per-message
    /// `usage` × per-model rates over the transcript. See
    /// `TranscriptDigest::total_cost_usd` for caveats.
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tokens_cache_write: u64,
    pub total_tokens_cache_read: u64,

    pub state: AttentionState,
    /// True when the user has muted this session. Muted sessions still update
    /// in the background but render dimmed and sort to the bottom of the list.
    pub muted: bool,
    /// Pending tool-use approval requests captured by the hook. Newest last.
    /// These enrich the headline/detail and hook-mode `a`/`d` flow, but they
    /// are only actionable when Claude itself reports `status == "waiting"`.
    pub pending_approvals: Vec<PendingApproval>,
}

/// How `a`/`d` deliver an approve/deny when a session is at a permission
/// prompt. Two distinct mechanisms exist and we don't auto-fall-back between
/// them — the user picks explicitly so behavior is predictable.
///
/// - `Hook`: write a decision file the PreToolUse hook is polling for. Carries
///   a deny reason. Requires the hook to actually run, which is blocked on
///   machines with `allowManagedHooksOnly: true` in managed-settings.json.
/// - `Tmux`: `tmux send-keys` against the pane to dismiss Claude's native
///   permission prompt. Works regardless of managed policy. Deny is just
///   Escape — no reason payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalMode {
    Hook,
    Tmux,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        // Hook gives a richer approve/deny path (full tool_input, deny-with-
        // reason). Falls back to Tmux when the hook is bypassed by managed
        // policy — toggle with `h`.
        Self::Hook
    }
}

impl ApprovalMode {
    pub fn label(self) -> &'static str {
        match self {
            ApprovalMode::Hook => "hook",
            ApprovalMode::Tmux => "tmux",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            ApprovalMode::Hook => ApprovalMode::Tmux,
            ApprovalMode::Tmux => ApprovalMode::Hook,
        }
    }
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
