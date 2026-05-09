use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::ApprovalMode;

/// Stable identity for a Claude session that survives a triage restart.
/// We can't use pid (recycled by the OS) or sessionId (rewritten by `/clear`),
/// but `(cwd, started_at_ms)` doesn't change for a process's lifetime, and a
/// new process — even if reusing an old pid — has a different start time.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct MuteKey {
    pub cwd: PathBuf,
    pub started_at_ms: u64,
}

/// On-disk shape: list of mutes with a Unix-seconds timestamp.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedMute {
    cwd: PathBuf,
    started_at_ms: u64,
    muted_at_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedState {
    #[serde(default)]
    mutes: Vec<PersistedMute>,
    /// Optional for backward-compat with state.json files written before the
    /// approval-mode toggle existed. Missing → use `ApprovalMode::default()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_mode: Option<ApprovalMode>,
    /// Autonomous-mode toggle (T-56). Off by default; explicit opt-in only.
    #[serde(default)]
    autonomous: bool,
    /// Tmux pane id (`%N`) of the last-running triage instance. Tombstone
    /// — kept across triage exits so `--jump-to-self` and plain `triage`
    /// can locate the previous pane and `respawn-pane` there instead of
    /// creating a duplicate window. Only overwritten when a fresh triage
    /// records its own pane on startup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_pane_id: Option<String>,
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/triage/state.json")
}

pub struct LoadedState {
    pub mutes: Vec<(MuteKey, SystemTime)>,
    pub approval_mode: ApprovalMode,
    pub autonomous: bool,
}

pub fn load_state() -> LoadedState {
    let path = state_path();
    let Ok(bytes) = fs::read(&path) else {
        return LoadedState {
            mutes: Vec::new(),
            approval_mode: ApprovalMode::default(),
            autonomous: false,
        };
    };
    let Ok(state) = serde_json::from_slice::<PersistedState>(&bytes) else {
        return LoadedState {
            mutes: Vec::new(),
            approval_mode: ApprovalMode::default(),
            autonomous: false,
        };
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
    LoadedState {
        mutes,
        approval_mode: state.approval_mode.unwrap_or_default(),
        autonomous: state.autonomous,
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
pub fn save_state<'a, I>(entries: I, approval_mode: ApprovalMode, autonomous: bool)
where
    I: IntoIterator<Item = (&'a MuteKey, &'a SystemTime)>,
{
    let mutes: Vec<PersistedMute> = entries
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
    // Read existing pane_id and preserve. The full save path is for mutes /
    // approval-mode / autonomous; pane_id is owned by AliveGuard and we
    // shouldn't accidentally overwrite it from this code path.
    let existing_pane_id = fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<PersistedState>(&b).ok())
        .and_then(|s| s.last_pane_id);
    let state = PersistedState {
        mutes,
        approval_mode: Some(approval_mode),
        autonomous,
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
