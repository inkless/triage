use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::{Provider, Session};

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

/// On-disk shape for a pin. Unlike mutes, a pin carries no timestamp — it's
/// sticky until the user toggles `*` off, with no auto-clear behavior.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedPin {
    cwd: PathBuf,
    started_at_ms: u64,
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
    /// Pinned sessions (float to the top of the table). Keyed on the same
    /// stable (cwd, started_at_ms) identity as mutes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pins: Vec<PersistedPin>,
    /// User-provided triage-only row names. These deliberately do not mutate
    /// Claude/Codex/tmux state; they are display aliases owned by triage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<PersistedAlias>,
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
    pub pins: Vec<MuteKey>,
    pub aliases: Vec<(AliasKey, String)>,
    pub autonomous: bool,
    pub phone_push_enabled: bool,
}

impl LoadedState {
    fn empty() -> Self {
        Self {
            mutes: Vec::new(),
            pins: Vec::new(),
            aliases: Vec::new(),
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
    let aliases = aliases_from_state(&state).into_iter().collect::<Vec<_>>();
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
    let pins = state
        .pins
        .into_iter()
        .map(|p| MuteKey {
            cwd: p.cwd,
            started_at_ms: p.started_at_ms,
        })
        .collect();
    LoadedState {
        mutes,
        pins,
        aliases,
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
pub fn save_state<'a, M, P, A>(
    mutes_iter: M,
    pins_iter: P,
    aliases_iter: A,
    autonomous: bool,
    phone_push_enabled: bool,
) where
    M: IntoIterator<Item = (&'a MuteKey, &'a SystemTime)>,
    P: IntoIterator<Item = &'a MuteKey>,
    A: IntoIterator<Item = (&'a AliasKey, &'a String)>,
{
    save_state_with_alias_mode(
        mutes_iter,
        pins_iter,
        aliases_iter,
        autonomous,
        phone_push_enabled,
        AliasWriteMode::Merge,
    );
}

/// Save state after an explicit rename edit. Unlike the normal save path,
/// this replaces aliases so clearing an alias stays cleared.
pub fn save_state_replace_aliases<'a, M, P, A>(
    mutes_iter: M,
    pins_iter: P,
    aliases_iter: A,
    autonomous: bool,
    phone_push_enabled: bool,
) where
    M: IntoIterator<Item = (&'a MuteKey, &'a SystemTime)>,
    P: IntoIterator<Item = &'a MuteKey>,
    A: IntoIterator<Item = (&'a AliasKey, &'a String)>,
{
    save_state_with_alias_mode(
        mutes_iter,
        pins_iter,
        aliases_iter,
        autonomous,
        phone_push_enabled,
        AliasWriteMode::Replace,
    );
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AliasWriteMode {
    Merge,
    Replace,
}

fn save_state_with_alias_mode<'a, M, P, A>(
    mutes_iter: M,
    pins_iter: P,
    aliases_iter: A,
    autonomous: bool,
    phone_push_enabled: bool,
    alias_write_mode: AliasWriteMode,
) where
    M: IntoIterator<Item = (&'a MuteKey, &'a SystemTime)>,
    P: IntoIterator<Item = &'a MuteKey>,
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
    let pins: Vec<PersistedPin> = pins_iter
        .into_iter()
        .map(|k| PersistedPin {
            cwd: k.cwd.clone(),
            started_at_ms: k.started_at_ms,
        })
        .collect();

    // Read existing state once. Pane id is owned by AliveGuard. Aliases are
    // normally merged from disk so a long-running older TUI snapshot cannot
    // wipe aliases created by another triage instance when it saves an
    // unrelated toggle or mute change.
    let existing_state: PersistedState = fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let existing_pane_id = existing_state.last_pane_id.clone();
    let codex_thread_roots = crate::codex::load_thread_roots_for_aliases();
    let aliases = persisted_aliases_for_save(
        &existing_state,
        aliases_iter,
        alias_write_mode,
        &codex_thread_roots,
    );
    let state = PersistedState {
        mutes,
        pins,
        aliases,
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

fn aliases_from_state(state: &PersistedState) -> HashMap<AliasKey, String> {
    let codex_thread_roots = crate::codex::load_thread_roots_for_aliases();
    aliases_from_state_with_roots(state, &codex_thread_roots)
}

fn aliases_from_state_with_roots(
    state: &PersistedState,
    codex_thread_roots: &HashMap<String, String>,
) -> HashMap<AliasKey, String> {
    state
        .aliases
        .iter()
        .filter(|a| !a.alias.trim().is_empty() && !a.session_id.is_empty())
        .map(|a| {
            let key =
                canonical_alias_key(a.provider.clone(), a.session_id.clone(), codex_thread_roots);
            (key, a.alias.trim().to_string())
        })
        .collect()
}

fn persisted_aliases_for_save<'a, A>(
    existing_state: &PersistedState,
    aliases_iter: A,
    alias_write_mode: AliasWriteMode,
    codex_thread_roots: &HashMap<String, String>,
) -> Vec<PersistedAlias>
where
    A: IntoIterator<Item = (&'a AliasKey, &'a String)>,
{
    let mut alias_map = if alias_write_mode == AliasWriteMode::Merge {
        aliases_from_state_with_roots(existing_state, codex_thread_roots)
    } else {
        HashMap::new()
    };
    alias_map.extend(
        aliases_iter
            .into_iter()
            .filter(|(_, alias)| !alias.trim().is_empty())
            .map(|(key, alias)| {
                (
                    canonical_alias_key(
                        key.provider.clone(),
                        key.session_id.clone(),
                        codex_thread_roots,
                    ),
                    alias.trim().to_string(),
                )
            }),
    );
    alias_map
        .into_iter()
        .map(|(key, alias)| PersistedAlias {
            provider: key.provider,
            session_id: key.session_id,
            alias,
        })
        .collect()
}

fn canonical_alias_key(
    provider: String,
    session_id: String,
    codex_thread_roots: &HashMap<String, String>,
) -> AliasKey {
    let mut key = AliasKey {
        provider,
        session_id,
    };
    if key.provider == Provider::Codex.label()
        && let Some(root) = codex_thread_roots.get(&key.session_id)
    {
        key.session_id = root.clone();
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(provider: &str, session_id: &str, alias: &str) -> PersistedAlias {
        PersistedAlias {
            provider: provider.to_string(),
            session_id: session_id.to_string(),
            alias: alias.to_string(),
        }
    }

    fn alias_key(provider: &str, session_id: &str) -> AliasKey {
        AliasKey {
            provider: provider.to_string(),
            session_id: session_id.to_string(),
        }
    }

    fn alias_map(aliases: Vec<PersistedAlias>) -> HashMap<(String, String), String> {
        aliases
            .into_iter()
            .map(|a| ((a.provider, a.session_id), a.alias))
            .collect()
    }

    #[test]
    fn merge_alias_save_preserves_existing_aliases() {
        let existing_state = PersistedState {
            aliases: vec![alias("cc", "old-session", "old alias")],
            ..Default::default()
        };
        let key = alias_key("cx", "new-session");
        let name = "new alias".to_string();

        let aliases = persisted_aliases_for_save(
            &existing_state,
            [(&key, &name)],
            AliasWriteMode::Merge,
            &HashMap::new(),
        );

        let aliases = alias_map(aliases);
        assert_eq!(
            aliases.get(&("cc".to_string(), "old-session".to_string())),
            Some(&"old alias".to_string())
        );
        assert_eq!(
            aliases.get(&("cx".to_string(), "new-session".to_string())),
            Some(&"new alias".to_string())
        );
    }

    #[test]
    fn replace_alias_save_drops_existing_aliases() {
        let existing_state = PersistedState {
            aliases: vec![alias("cc", "old-session", "old alias")],
            ..Default::default()
        };
        let key = alias_key("cx", "new-session");
        let name = "new alias".to_string();

        let aliases = persisted_aliases_for_save(
            &existing_state,
            [(&key, &name)],
            AliasWriteMode::Replace,
            &HashMap::new(),
        );

        let aliases = alias_map(aliases);
        assert!(!aliases.contains_key(&("cc".to_string(), "old-session".to_string())));
        assert_eq!(
            aliases.get(&("cx".to_string(), "new-session".to_string())),
            Some(&"new alias".to_string())
        );
    }

    #[test]
    fn alias_save_canonicalizes_codex_children_to_roots() {
        let mut roots = HashMap::new();
        roots.insert("child".to_string(), "root".to_string());
        let existing_state = PersistedState {
            aliases: vec![alias("cx", "child", "existing alias")],
            ..Default::default()
        };
        let key = alias_key("cx", "child");
        let name = "new alias".to_string();

        let aliases = persisted_aliases_for_save(
            &existing_state,
            [(&key, &name)],
            AliasWriteMode::Merge,
            &roots,
        );

        let aliases = alias_map(aliases);
        assert_eq!(
            aliases.get(&("cx".to_string(), "root".to_string())),
            Some(&"new alias".to_string())
        );
        assert!(!aliases.contains_key(&("cx".to_string(), "child".to_string())));
    }
}
