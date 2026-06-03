use std::path::PathBuf;
use std::time::SystemTime;

use crate::approval::PendingApproval;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "cc",
            Provider::Codex => "cx",
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Pane {
    pub target: String,
    /// `#{session_name}` portion of `target`. Split out here so consumers
    /// (UI, notify, jump) don't all re-parse the same colon-separated string.
    pub tmux_session: String,
    /// `#{window_name}` for the containing window. Used as a label fallback
    /// when a Claude session was launched without `--name`/`/rename` — the
    /// user-set window name is the closest signal of intent. Beware
    /// `automatic-rename`: when on, tmux sets the window name to the
    /// foreground command. Consumers should skip names that equal
    /// `current_command` or look like terminal-emitted tab IDs (`2.1.139`).
    pub window_name: String,
    /// Tmux's permanent unique ID for this pane (e.g. `%42`). Unlike
    /// `target`, this is immutable for the pane's lifetime — survives
    /// `renumber-windows`, `move-window`, etc. Used as the stable handle
    /// for `.alive` so an opened-then-renumbered pane is still findable.
    pub pane_id: String,
    pub pid: u32,
    pub tty: String,
    pub current_command: String,
    pub cwd: PathBuf,
    pub active: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Session {
    pub provider: Provider,
    pub pid: u32,
    pub session_id: String,
    /// Optional provider-specific display-alias identity. Codex child threads
    /// spawned from one parent should share a display alias, so Codex fills
    /// this with the root thread id. Providers without such a family concept
    /// leave it unset and use `session_id` directly.
    pub alias_session_id: Option<String>,
    pub cwd: PathBuf,
    pub name: Option<String>,
    pub status: String,
    /// `waitingFor` from sessions JSON — populated when `status == "waiting"`
    /// (e.g. `"approve Bash"`). This is Claude Code's own canonical signal that
    /// the session is paused on a permission prompt.
    pub waiting_for: Option<String>,
    /// Provider CLI version recorded by the session source. Claude writes this
    /// into `~/.claude/sessions/*.json`; tmux often exposes the same value as
    /// `pane_current_command`, which gives us a safe fallback join signal when
    /// the process-parent walk is unavailable.
    pub cli_version: Option<String>,
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
    /// Total input tokens for the latest assistant API call. Approximates
    /// current context-window occupancy. Pair with `latest_model` (and the
    /// model's documented context size) to get a percentage.
    pub latest_context_tokens: u64,
    /// Provider-reported context window when available. Claude sessions infer
    /// this from model/settings; Codex token_count events report it directly.
    pub context_window: Option<u64>,
    /// Peak `latest_context_tokens` observed across the session. >200k is
    /// solid evidence the user is on a 1M-context variant.
    pub peak_context_tokens: u64,
    pub latest_model: Option<String>,
    /// Most recent assistant text response. For Blocked sessions this is
    /// usually Claude's *explanation* of the pending tool call.
    pub latest_assistant_text: Option<String>,

    pub state: AttentionState,
    /// Provider-specific prompt hint from the transcript/state layer. For
    /// Codex this means the latest unfinished tool call explicitly requested
    /// approval. This is not enough to classify as Blocked by itself because
    /// a user-approved long-running command is also unfinished until output
    /// returns.
    pub approval_prompt_pending: bool,
    /// Pane content shows a provider permission UI in the last few lines.
    /// For Claude this is the native `1. Yes` / `Esc to cancel` prompt; for
    /// Codex this is the `Would you like to ...?` approval surface. Set in
    /// the refresh pass after the cheap provider signals identify candidates.
    pub pane_blocked: bool,
    /// True when the user has muted this session. Muted sessions still update
    /// in the background but render dimmed and sort to the bottom of the list.
    pub muted: bool,
    /// True when the user has armed a watch on this session via `w` (T-81).
    /// Watched sessions fire a "finished" notification on each transition
    /// into `JustFinished` until toggled off. In-memory only — not persisted.
    pub watched: bool,
    /// True when the user has pinned this session via `*`. Pinned sessions
    /// sort above everything else regardless of attention state, so the rows
    /// you care about stay at the top. Sticky (only `*` clears it) and
    /// persisted across restarts.
    pub pinned: bool,
    /// Pending tool-use approval requests captured by the hook. Newest last.
    /// These enrich the headline/detail and hook-mode `a`/`d` flow, but they
    /// are only actionable when Claude itself reports `status == "waiting"`.
    pub pending_approvals: Vec<PendingApproval>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Provider,
        pid: u32,
        session_id: String,
        cwd: PathBuf,
        name: Option<String>,
        status: String,
        started_at_ms: u64,
        updated_at_ms: u64,
        waiting_for: Option<String>,
    ) -> Self {
        Self {
            provider,
            pid,
            session_id,
            alias_session_id: None,
            cwd,
            name,
            status,
            waiting_for,
            cli_version: None,
            started_at_ms,
            updated_at_ms,
            pane: None,
            transcript_path: None,
            headline: None,
            last_prompt: None,
            last_prompt_at: None,
            last_turn_duration_ms: None,
            last_turn_msg_count: None,
            last_event_at: None,
            last_stop_at: None,
            user_prompt_count: 0,
            last_stop_had_errors: false,
            last_tool_use: None,
            total_cost_usd: 0.0,
            total_tokens_in: 0,
            total_tokens_out: 0,
            total_tokens_cache_write: 0,
            total_tokens_cache_read: 0,
            latest_context_tokens: 0,
            context_window: None,
            peak_context_tokens: 0,
            latest_model: None,
            latest_assistant_text: None,
            state: AttentionState::Unknown,
            approval_prompt_pending: false,
            pane_blocked: false,
            muted: false,
            watched: false,
            pinned: false,
            pending_approvals: Vec::new(),
        }
    }
}

/// User-facing label for a live session. Keep this shared between the TUI and
/// `triage agents` so humans and agents see the same target names.
pub fn session_display_label(s: &Session) -> String {
    s.name
        .clone()
        .or_else(|| s.pane.as_ref().and_then(useful_window_name))
        .or_else(|| {
            s.pane
                .as_ref()
                .map(|p| p.tmux_session.as_str())
                .filter(|n| !n.is_empty() && !n.chars().all(|c| c.is_ascii_digit()))
                .map(|n| n.to_string())
        })
        .or_else(|| s.cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| s.cwd.display().to_string())
}

/// Return `Pane.window_name` only when it carries user intent. Skip when:
/// - empty
/// - tmux's automatic-rename has set it to the foreground command
/// - it's a terminal-emitted tab ID like `2.1.139`
/// - it's a bracketed tmux marker like `[tmux]`
/// - it's a known shell/editor/tool residue from automatic rename
pub fn useful_window_name(pane: &Pane) -> Option<String> {
    let name = pane.window_name.trim();
    if name.is_empty() {
        return None;
    }
    if name == pane.current_command {
        return None;
    }
    if name.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    if name.starts_with('[') && name.ends_with(']') {
        return None;
    }
    if is_auto_rename_residue(name) {
        return None;
    }
    Some(name.to_string())
}

fn is_auto_rename_residue(name: &str) -> bool {
    matches!(
        name,
        // shells
        "fish" | "bash" | "zsh" | "sh" | "dash" | "ksh" | "tcsh"
        // multiplexers / nested terminal
        | "tmux" | "screen"
        // editors
        | "nvim" | "vim" | "vi" | "nano" | "emacs" | "helix" | "hx"
        // common interpreters / CLIs
        | "claude" | "node" | "python" | "python3" | "ruby" | "go"
        | "irb" | "pry" | "ipython" | "psql" | "redis-cli" | "mysql"
        // network / file tools
        | "ssh" | "scp" | "rsync" | "git" | "lazygit" | "gh"
        // monitors / pagers
        | "top" | "htop" | "btop" | "less" | "more" | "cat" | "tail"
        // build tools
        | "make" | "cargo" | "pnpm" | "npm" | "yarn" | "bun"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: Option<&str>, window_name: &str, tmux_session: &str) -> Session {
        let mut s = Session::new(
            Provider::Claude,
            123,
            "sid".to_string(),
            PathBuf::from("/tmp/project"),
            name.map(str::to_string),
            "idle".to_string(),
            0,
            0,
            None,
        );
        s.pane = Some(Pane {
            target: "main:1.0".to_string(),
            tmux_session: tmux_session.to_string(),
            window_name: window_name.to_string(),
            pane_id: "%42".to_string(),
            pid: 123,
            tty: "/dev/ttys001".to_string(),
            current_command: "claude".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            active: false,
        });
        s
    }

    #[test]
    fn display_label_prefers_provider_name() {
        let s = session(Some("agent-name"), "tmux-name", "tmux-session");

        assert_eq!(session_display_label(&s), "agent-name");
    }

    #[test]
    fn display_label_uses_tmux_window_name() {
        let s = session(None, "agent-ACDC-21", "acdc-hub");

        assert_eq!(session_display_label(&s), "agent-ACDC-21");
    }

    #[test]
    fn display_label_skips_auto_window_name_and_uses_tmux_session() {
        let s = session(None, "2.1.158", "acdc-hub");

        assert_eq!(session_display_label(&s), "acdc-hub");
    }

    #[test]
    fn display_label_skips_numeric_tmux_session() {
        let s = session(None, "fish", "1");

        assert_eq!(session_display_label(&s), "project");
    }
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
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApprovalMode {
    // Hook gives a richer approve/deny path (full tool_input, deny-with-reason).
    // Falls back to Tmux when the hook is bypassed by managed policy.
    #[default]
    Hook,
    Tmux,
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
