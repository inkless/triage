use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::{ApprovalMode, Provider, Session};

/// Stable identity for a Claude session that survives a triage restart.
/// We can't use pid (recycled by the OS) or sessionId (rewritten by `/clear`),
/// but `(cwd, started_at_ms)` doesn't change for a process's lifetime, and a
/// new process — even if reusing an old pid — has a different start time.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct MuteKey {
    pub cwd: PathBuf,
    pub started_at_ms: u64,
}

/// Triage-local display alias identity. Provider + session id is stable for
/// Codex thread ids and avoids collisions if providers use similar ids.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct AliasKey {
    pub provider: String,
    pub session_id: String,
}

impl AliasKey {
    pub fn for_session(session: &Session) -> Self {
        Self {
            provider: session.provider.label().to_string(),
            session_id: session
                .alias_session_id
                .as_deref()
                .unwrap_or(&session.session_id)
                .to_string(),
        }
    }

    pub fn exact_for_session(session: &Session) -> Self {
        Self {
            provider: session.provider.label().to_string(),
            session_id: session.session_id.clone(),
        }
    }

    pub fn candidates_for_session(session: &Session) -> Vec<Self> {
        let exact = Self::exact_for_session(session);
        let alias = Self::for_session(session);
        if exact == alias {
            vec![exact]
        } else {
            vec![exact, alias]
        }
    }
}

/// On-disk shape: list of mutes with a Unix-seconds timestamp.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedMute {
    cwd: PathBuf,
    started_at_ms: u64,
    muted_at_secs: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAlias {
    provider: String,
    session_id: String,
    alias: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedState {
    #[serde(default)]
    mutes: Vec<PersistedMute>,
    /// User-provided triage-only row names. These deliberately do not mutate
    /// Claude/Codex/tmux state; they are display aliases owned by triage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<PersistedAlias>,
    /// Optional for backward-compat with state.json files written before the
    /// approval-mode toggle existed. Missing → use `ApprovalMode::default()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_mode: Option<ApprovalMode>,
    /// Autonomous-mode toggle (T-56). Off by default; explicit opt-in only.
    #[serde(default)]
    autonomous: bool,
    /// ntfy phone push enabled. Defaults true (preserves behavior on first
    /// load + for existing state.json files written before this field
    /// existed). Toggle with `p`. Gates the *app-driven* ntfy POSTs only —
    /// the `triage notify` CLI is unaffected (user-initiated pings always
    /// fire).
    #[serde(default = "default_true")]
    phone_push_enabled: bool,
    /// Tmux pane id (`%N`) of the last-running triage instance. Tombstone
    /// — kept across triage exits so `--jump-to-self` and plain `triage`
    /// can locate the previous pane and `respawn-pane` there instead of
    /// creating a duplicate window. Only overwritten when a fresh triage
    /// records its own pane on startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_pane_id: Option<String>,
}

fn default_true() -> bool {
    true
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/triage/state.json")
}

pub struct LoadedState {
    pub mutes: Vec<(MuteKey, SystemTime)>,
    pub aliases: Vec<(AliasKey, String)>,
    pub approval_mode: ApprovalMode,
    pub autonomous: bool,
    pub phone_push_enabled: bool,
}

impl LoadedState {
    fn empty() -> Self {
        Self {
            mutes: Vec::new(),
            aliases: Vec::new(),
            approval_mode: ApprovalMode::default(),
            autonomous: false,
            phone_push_enabled: true,
        }
    }
}

pub fn load_state() -> LoadedState {
    let path = state_path();
    let Ok(bytes) = fs::read(&path) else {
        return LoadedState::empty();
    };
    let Ok(state) = serde_json::from_slice::<PersistedState>(&bytes) else {
        return LoadedState::empty();
    };
    let mutes = state
        .mutes
        .into_iter()
        .map(|m| {
            let mute_at = UNIX_EPOCH + Duration::from_secs(m.muted_at_secs);
            (
                MuteKey {
                    cwd: m.cwd,
                    started_at_ms: m.started_at_ms,
                },
                mute_at,
            )
        })
        .collect();
    let codex_thread_roots = crate::codex::load_thread_roots_for_aliases();
    let aliases = state
        .aliases
        .into_iter()
        .filter(|a| !a.alias.trim().is_empty() && !a.session_id.is_empty())
        .map(|a| {
            let mut key = AliasKey {
                provider: a.provider,
                session_id: a.session_id,
            };
            if key.provider == Provider::Codex.label()
                && let Some(root) = codex_thread_roots.get(&key.session_id)
            {
                key.session_id = root.clone();
            }
            (key, a.alias)
        })
        .collect();
    LoadedState {
        mutes,
        aliases,
        approval_mode: state.approval_mode.unwrap_or_default(),
        autonomous: state.autonomous,
        phone_push_enabled: state.phone_push_enabled,
    }
}

/// Read just `last_pane_id` without loading the rest of the state. Used
/// by the silent-attach path on a plain `triage` invocation — we want to
/// know where the previous instance was without paying the full
/// `load_state` cost (which constructs MuteKey hashmaps etc.).
pub fn read_last_pane_id() -> Option<String> {
    let bytes = fs::read(state_path()).ok()?;
    let state: PersistedState = serde_json::from_slice(&bytes).ok()?;
    state.last_pane_id
}

/// Write just `last_pane_id`, preserving all other fields of state.json.
/// Called by AliveGuard on triage startup to record the pane it's running
/// in. Tombstone — never cleared by triage itself; only overwritten by a
/// future startup, or manually edited.
pub fn save_last_pane_id(pane_id: &str) {
    let path = state_path();
    let mut state: PersistedState = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    state.last_pane_id = Some(pane_id.to_string());
    let Ok(json) = serde_json::to_vec_pretty(&state) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, json);
}

/// Best-effort save. Failures are ignored — losing prefs is annoying
/// but not catastrophic, and we don't want IO errors to surface in the TUI.
pub fn save_state<'a, M, A>(
    mutes_iter: M,
    aliases_iter: A,
    approval_mode: ApprovalMode,
    autonomous: bool,
    phone_push_enabled: bool,
) where
    M: IntoIterator<Item = (&'a MuteKey, &'a SystemTime)>,
    A: IntoIterator<Item = (&'a AliasKey, &'a String)>,
{
    let mutes: Vec<PersistedMute> = mutes_iter
        .into_iter()
        .filter_map(|(k, t)| {
            let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
            Some(PersistedMute {
                cwd: k.cwd.clone(),
                started_at_ms: k.started_at_ms,
                muted_at_secs: secs,
            })
        })
        .collect();
    let aliases: Vec<PersistedAlias> = aliases_iter
        .into_iter()
        .filter(|(_, alias)| !alias.trim().is_empty())
        .map(|(key, alias)| PersistedAlias {
            provider: key.provider.clone(),
            session_id: key.session_id.clone(),
            alias: alias.trim().to_string(),
        })
        .collect();
    // Read existing pane_id and preserve. The full save path is for mutes /
    // approval-mode / autonomous / phone-push; pane_id is owned by
    // AliveGuard and we shouldn't accidentally overwrite it from this code
    // path.
    let existing_pane_id = fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<PersistedState>(&b).ok())
        .and_then(|s| s.last_pane_id);
    let state = PersistedState {
        mutes,
        aliases,
        approval_mode: Some(approval_mode),
        autonomous,
        phone_push_enabled,
        last_pane_id: existing_pane_id,
    };
    let Ok(json) = serde_json::to_vec_pretty(&state) else {
        return;
    };
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, json);
}
