use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
    mutes: Vec<PersistedMute>,
}

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/triage/state.json")
}

pub fn load_mutes() -> Vec<(MuteKey, SystemTime)> {
    let path = state_path();
    let Ok(bytes) = fs::read(&path) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_slice::<PersistedState>(&bytes) else {
        return Vec::new();
    };
    state
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
        .collect()
}

/// Best-effort save. Failures are ignored — losing a mute set is annoying
/// but not catastrophic, and we don't want IO errors to surface in the TUI.
pub fn save_mutes<'a, I>(entries: I)
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
    let state = PersistedState { mutes };
    let Ok(json) = serde_json::to_vec_pretty(&state) else {
        return;
    };
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, json);
}
