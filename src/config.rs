//! Hand-editable user config at `~/.config/triage/config.toml`.
//!
//! Single source of truth — no env-var overrides on top. All sections +
//! fields are optional; an empty file (or no file at all) is valid and
//! means "use built-in defaults." Loaded once at startup; no hot reload —
//! restart triage to pick up changes.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

/// Default mobile-width threshold for auto-zoom-on-jump. iPhone ~30–80,
/// iPad portrait ~120, iPad landscape ~200, desktop ~200+. 140 catches iPad
/// portrait without false-positive on a narrow desktop split-screen.
pub const DEFAULT_MOBILE_WIDTH: u16 = 140;
pub const DEFAULT_REFRESH_SECONDS: u64 = 2;

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub ntfy: Option<NtfyConfig>,
    pub thresholds: Thresholds,
    pub notifications: NotificationsConfig,
    pub model: ModelConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NtfyConfig {
    pub url: String,
    pub user: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Thresholds {
    pub mobile_width: u16,
    pub refresh_seconds: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            mobile_width: DEFAULT_MOBILE_WIDTH,
            refresh_seconds: DEFAULT_REFRESH_SECONDS,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NotificationsConfig {
    pub terminal_bundle: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelConfig {
    /// Override per-session context-window detection. None → fall back to the
    /// auto-detect chain (see `transcript.rs`).
    pub context_window: Option<u64>,
}

/// On-disk shape (TOML deserialization target). Each section optional; merged
/// into `Config` via `From`.
#[derive(Debug, Default, Deserialize)]
struct DiskConfig {
    #[serde(default)]
    ntfy: Option<NtfyConfig>,
    #[serde(default)]
    thresholds: Option<DiskThresholds>,
    #[serde(default)]
    notifications: Option<DiskNotifications>,
    #[serde(default)]
    model: Option<DiskModel>,
}

#[derive(Debug, Default, Deserialize)]
struct DiskThresholds {
    #[serde(default)]
    mobile_width: Option<u16>,
    #[serde(default)]
    refresh_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct DiskNotifications {
    #[serde(default)]
    terminal_bundle: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DiskModel {
    #[serde(default)]
    context_window: Option<u64>,
}

impl Config {
    /// Read `~/.config/triage/config.toml` if present. Missing file is fine
    /// — returns built-in defaults. Parse errors print a warning to stderr
    /// and fall back to defaults; we don't crash on a bad config because
    /// triage is a TUI that wants to keep running.
    pub fn load() -> Self {
        match read_disk() {
            Ok(disk) => disk.into(),
            Err(e) => {
                if !is_missing(&e) {
                    eprintln!("[warn] failed to read config: {e}; using defaults");
                }
                Config::default()
            }
        }
    }
}

fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/triage/config.toml"))
}

fn read_disk() -> std::io::Result<DiskConfig> {
    let path = config_path()
        .ok_or_else(|| std::io::Error::other("HOME not set; cannot resolve config path"))?;
    let bytes = fs::read(&path)?;
    // World/group-readable files are a footgun for `[ntfy].token`. Refuse to
    // load a config with permissive perms — better to noisily revert to
    // defaults than silently leak credentials. (Unix-only check; Windows
    // skips.)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "[warn] {} has permissive perms ({:o}); refusing to load. \
                 Run `chmod 600 {}` to fix.",
                path.display(),
                mode & 0o777,
                path.display(),
            );
            return Ok(DiskConfig::default());
        }
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| std::io::Error::other(format!("config not utf-8: {e}")))?;
    toml::from_str::<DiskConfig>(text)
        .map_err(|e| std::io::Error::other(format!("config parse error: {e}")))
}

fn is_missing(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
        || e.kind() == std::io::ErrorKind::PermissionDenied
            && e.to_string().contains("No such file")
}

impl From<DiskConfig> for Config {
    fn from(d: DiskConfig) -> Self {
        let mut cfg = Config {
            ntfy: d.ntfy,
            ..Default::default()
        };
        if let Some(t) = d.thresholds {
            if let Some(v) = t.mobile_width {
                cfg.thresholds.mobile_width = v;
            }
            if let Some(v) = t.refresh_seconds {
                cfg.thresholds.refresh_seconds = v;
            }
        }
        if let Some(n) = d.notifications {
            cfg.notifications.terminal_bundle = n.terminal_bundle;
        }
        if let Some(m) = d.model {
            cfg.model.context_window = m.context_window;
        }
        cfg
    }
}
