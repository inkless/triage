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
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/triage/state.json")
}

pub struct LoadedState {
    pub mutes: Vec<(MuteKey, SystemTime)>,
    pub approval_mode: ApprovalMode,
}

pub fn load_state() -> LoadedState {
    let path = state_path();
    let Ok(bytes) = fs::read(&path) else {
        return LoadedState {
            mutes: Vec::new(),
            approval_mode: ApprovalMode::default(),
        };
    };
    let Ok(state) = serde_json::from_slice::<PersistedState>(&bytes) else {
        return LoadedState {
            mutes: Vec::new(),
            approval_mode: ApprovalMode::default(),
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
    }
}

/// Best-effort save. Failures are ignored — losing prefs is annoying
/// but not catastrophic, and we don't want IO errors to surface in the TUI.
pub fn save_state<'a, I>(entries: I, approval_mode: ApprovalMode)
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
    let state = PersistedState {
        mutes,
        approval_mode: Some(approval_mode),
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
