use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Pending files older than this are stale. The hook itself only waits a few
/// seconds before falling back to Claude's native permission flow, so anything
/// that survives longer is from a hook process that died without running its
/// cleanup trap (cancelled tool call, SIGKILL, crash). We auto-delete on read
/// so orphaned files don't keep showing a fake pending approval.
const PENDING_TTL: Duration = Duration::from_secs(30);

/// Single tool-use approval request the hook is waiting on.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PendingApproval {
    pub uuid: String,
    pub session_id: String,
    pub cwd: PathBuf,
    pub tool_name: String,
    /// One-line summary used by the UI (truncated to 200–400 chars depending
    /// on tool type). NOT suitable for feeding the autonomous-mode auditor —
    /// truncating the body of a `gh pr create` or a multi-paragraph Edit makes
    /// it impossible to decide on safety. Use `tool_input_full` for that.
    pub tool_input_brief: String,
    /// Full `tool_input` JSON serialization, untruncated. The autonomous-mode
    /// auditor needs the whole command (especially for Bash heredocs and Edit
    /// new_string content) to make a confident decision. Empty string if the
    /// hook payload had no `tool_input` field.
    pub tool_input_full: String,
    pub created_at: SystemTime,
    pub pending_path: PathBuf,
}

impl PendingApproval {
    /// Multi-line, human-friendly rendering of `tool_input` for the detail
    /// pane. Re-parses `tool_input_full` and extracts the salient field per
    /// tool: Bash → command (real newlines), Edit/Write → file_path + the
    /// old_string/content, etc. Falls back to the raw JSON string when the
    /// tool isn't one we know how to summarize. Caller is responsible for
    /// truncating to the available render area.
    pub fn tool_input_detail(&self) -> String {
        let Ok(v) = serde_json::from_str::<Value>(&self.tool_input_full) else {
            return self.tool_input_full.clone();
        };
        if let Some(cmd) = v.get("command").and_then(|s| s.as_str()) {
            let desc = v
                .get("description")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty());
            return match desc {
                Some(d) => format!("{cmd}\n# {d}"),
                None => cmd.to_string(),
            };
        }
        if let Some(path) = v.get("file_path").and_then(|s| s.as_str()) {
            let detail = v
                .get("old_string")
                .or_else(|| v.get("content"))
                .or_else(|| v.get("new_string"))
                .and_then(|s| s.as_str());
            return match detail {
                Some(d) => format!("{path}\n---\n{d}"),
                None => path.to_string(),
            };
        }
        if let Some(url) = v.get("url").and_then(|s| s.as_str()) {
            return url.to_string();
        }
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
        self.tool_input_full.clone()
    }
}

pub fn triage_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/triage")
}

pub fn pending_dir() -> PathBuf {
    triage_dir().join("pending")
}

pub fn decisions_dir() -> PathBuf {
    triage_dir().join("decisions")
}

/// Auto-mode handshake dir. When the autonomous-mode auditor starts processing
/// a hook-captured request, triage writes `claims/<uuid>.json` here. The hook
/// extends its short timeout when it sees its uuid claimed, so the auditor
/// has time to reach a verdict (~10–25s on Sonnet) instead of the hook
/// defaulting to Claude's native permission flow at 3s.
pub fn claims_dir() -> PathBuf {
    triage_dir().join("claims")
}

pub fn alive_file() -> PathBuf {
    triage_dir().join(".alive")
}

/// Read the user's default model setting from `~/.claude/settings.json`. This
/// is the only deterministic source of the variant tag (e.g. `opus[1m]`) —
/// the per-message `model` field in transcripts strips the `[1m]` suffix
/// before logging. Returns None if the file is missing/unreadable/malformed
/// or if no `model` key is set.
pub fn read_default_model() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".claude/settings.json");
    let bytes = fs::read(&path).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.get("model")?.as_str().map(|s| s.to_string())
}

/// Best-effort claim write. The hook reads `claims/<uuid>.json` and extends
/// its deadline when present; absence means triage isn't going to decide, so
/// the hook can bail to Claude's native flow.
pub fn write_claim(uuid: &str) {
    let dir = claims_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(format!("{uuid}.json")), b"{}");
}

/// Best-effort claim removal. Called after the auditor sends its verdict
/// (so the hook reacts to claim absence as "WAIT — let Claude handle it",
/// or to decision-file presence as "auditor decided, use this").
pub fn remove_claim(uuid: &str) {
    let _ = fs::remove_file(claims_dir().join(format!("{uuid}.json")));
}

/// In-memory record of "triage is currently running here." Composed from
/// two on-disk sources kept separate for orthogonal reasons:
/// - `pid` from `~/.claude/triage/.alive` — bare integer for back-compat
///   with the bash PreToolUse hook (`triage-preuse.sh`), which `kill -0`s
///   the file's contents directly. Removed by `AliveGuard` on clean exit.
/// - `pane_id` from `~/.config/triage/state.json` `last_pane_id` —
///   tombstoned. Survives clean exits, kills, panics. Lets `--jump-to-self`
///   relocate the pane and `respawn-pane` in place even when the previous
///   triage's process is gone, which is the key to never spawning duplicate
///   windows.
#[derive(Debug)]
pub struct AliveRecord {
    pub pid: u32,
    pub pane_id: Option<String>,
}

/// Drop guard: writes pid to `.alive` and pane id to state.json on
/// construction, removes `.alive` on drop. The pane id stays in
/// state.json (tombstone) — see `AliveRecord` doc.
pub struct AliveGuard;

impl AliveGuard {
    pub fn install() -> Self {
        let dir = triage_dir();
        let _ = fs::create_dir_all(&dir);
        let _ = fs::write(alive_file(), std::process::id().to_string());
        if let Some(pane) = current_pane_id() {
            crate::persist::save_last_pane_id(&pane);
        }
        AliveGuard
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        // Only `.alive` is removed on clean exit — the hook treats
        // file-absence as "triage isn't intercepting." pane_id stays in
        // state.json so the next `--jump-to-self` can `respawn-pane`
        // in the previous location.
        let _ = fs::remove_file(alive_file());
    }
}

/// Read pid from `.alive` and pane_id from state.json. Returns None when
/// `.alive` is absent or unparseable (treated as "no triage running").
pub fn read_alive_record() -> Option<AliveRecord> {
    let content = fs::read_to_string(alive_file()).ok()?;
    let pid = content.trim().parse().ok()?;
    Some(AliveRecord {
        pid,
        pane_id: crate::persist::read_last_pane_id(),
    })
}

fn current_pane_id() -> Option<String> {
    // TMUX_PANE is the most reliable handle: tmux sets it for every
    // process inside a pane, and `display-message -t %N` accepts it
    // directly. Falling back to a no-target display-message would resolve
    // "current client" which is ambiguous when triage is launched from a
    // detached run-shell context (e.g. via the M-t binding's spawn path).
    std::env::var("TMUX_PANE").ok()
}

/// Read every pending file. Returns one PendingApproval per file. Files we
/// can't parse are skipped silently — the hook owns lifecycle, so a malformed
/// file means triage just won't surface it (the hook will time out and Claude
/// will fall back to its own prompt).
///
/// Side effect: deletes pending files older than `PENDING_TTL`. The hook
/// itself falls back after a few seconds, so anything that survives longer is
/// from a process that died without running its cleanup trap (cancelled tool
/// call, SIGKILL, crash).
pub fn read_pending() -> Vec<PendingApproval> {
    let dir = pending_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(uuid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let created_at = entry
            .metadata()
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .unwrap_or(now);
        if now
            .duration_since(created_at)
            .is_ok_and(|age| age > PENDING_TTL)
        {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(decisions_dir().join(format!("{uuid}.json")));
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let session_id = v
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let cwd = v
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(PathBuf::from)
            .unwrap_or_default();
        let tool_name = v
            .get("tool_name")
            .and_then(|s| s.as_str())
            .unwrap_or("?")
            .to_string();
        let tool_input_brief = brief_tool_input(v.get("tool_input"));
        let tool_input_full = v
            .get("tool_input")
            .map(|val| {
                if let Some(s) = val.as_str() {
                    s.to_string()
                } else {
                    val.to_string()
                }
            })
            .unwrap_or_default();
        out.push(PendingApproval {
            uuid: uuid.to_string(),
            session_id,
            cwd,
            tool_name,
            tool_input_brief,
            tool_input_full,
            created_at,
            pending_path: path,
        });
    }
    out.sort_by_key(|p| p.created_at);
    out
}

/// Write the decision JSON the hook is polling for. The hook reads this and
/// emits it as its own stdout, which Claude consumes.
pub fn approve(uuid: &str) {
    write_decision(uuid, r#"{"decision":"approve"}"#);
}

pub fn deny(uuid: &str, reason: &str) {
    let payload = serde_json::json!({
        "decision": "block",
        "reason": reason,
    });
    write_decision(uuid, &payload.to_string());
}

fn write_decision(uuid: &str, body: &str) {
    let dir = decisions_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{uuid}.json"));
    let _ = fs::write(path, body);
}

/// Render a preview of the tool input. The hook payload has the full tool
/// argument JSON, so we can show meaningfully more than what we'd parse from
/// the pane: command + description for Bash, file path + edit summary for
/// Edit/Write, etc. Headline wraps to 4 lines so we lift the truncation cap.
pub fn brief_tool_input(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    if let Some(cmd) = input.get("command").and_then(|s| s.as_str()) {
        // Bash: show command + description on the same line so the row's
        // wrap_text can split them across visual lines naturally.
        let desc = input
            .get("description")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty());
        return match desc {
            Some(d) => truncate(&format!("{cmd}  ·  {d}"), 400),
            None => truncate(cmd, 400),
        };
    }
    if let Some(path) = input.get("file_path").and_then(|s| s.as_str()) {
        // Edit/Write: path + a short hint of what's changing. Edit has
        // `old_string`; Write has `content`. Truncate hard since long diffs
        // would dominate the row.
        let detail = input
            .get("old_string")
            .or_else(|| input.get("content"))
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| truncate(s, 120));
        return match detail {
            Some(d) => truncate(&format!("{path}  ·  {d}"), 400),
            None => truncate(path, 200),
        };
    }
    if let Some(url) = input.get("url").and_then(|s| s.as_str()) {
        return truncate(url, 200);
    }
    if let Some(s) = input.as_str() {
        return truncate(s, 200);
    }
    truncate(&input.to_string(), 200)
}

pub fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

/// Match each pending approval to its session.
///
/// 1. Prefer `session_id` match — exact identity when sessions JSON is fresh.
/// 2. Fall back to cwd match. After `/clear` the sessions JSON keeps the
///    stale sessionId pointing at the pre-clear file, while the hook payload
///    carries the live sessionId — so a direct sessionId lookup misses.
///    When multiple sessions share a cwd (e.g. two lakehouse panes), prefer
///    one whose tmux pane is currently active over arbitrary first-match.
/// 3. If still ambiguous, attach to the first cwd-matching session — better
///    than dropping the approval entirely.
pub fn attach_to_sessions(approvals: Vec<PendingApproval>, sessions: &mut [crate::models::Session]) {
    for a in approvals {
        // 1. session_id exact match.
        if let Some(idx) = sessions.iter().position(|s| s.session_id == a.session_id) {
            sessions[idx].pending_approvals.push(a);
            continue;
        }
        // 2. cwd match, preferring an active pane.
        let cwd_matches: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (s.cwd == a.cwd).then_some(i))
            .collect();
        let chosen = cwd_matches
            .iter()
            .copied()
            .find(|&i| sessions[i].pane.as_ref().is_some_and(|p| p.active))
            .or_else(|| cwd_matches.first().copied());
        if let Some(idx) = chosen {
            sessions[idx].pending_approvals.push(a);
        }
    }
}

/// Path to `~/.claude/settings.json`. None when HOME is unset.
fn settings_json_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/settings.json"))
}

/// Bash hook content, embedded at compile time. `--install-hooks` writes this
/// to `hook_install_path()` so the hook is decoupled from the source-repo
/// location — `cargo install triage` users no longer need a checkout.
const HOOK_SCRIPT: &str = include_str!("../scripts/hooks/triage-preuse.sh");

/// Canonical install location for the bash hook. Stable across triage
/// upgrades; settings.json points here, not at the source repo.
fn hook_install_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/triage/hooks/triage-preuse.sh"))
}

/// True when `cmd` looks like a triage PreToolUse hook entry — basename
/// match. Lets us detect (and migrate) entries that point at older locations
/// like `~/workspace/triage/scripts/hooks/triage-preuse.sh` before we
/// started installing into `~/.config/triage/hooks/`.
fn is_triage_hook_command(cmd: &str) -> bool {
    Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == "triage-preuse.sh")
        .unwrap_or(false)
}

/// Write the embedded bash hook to `hook_install_path()` with mode 0755.
/// Idempotent — returns Ok(false) when on-disk content already matches and
/// the file has the executable bit set; returns Ok(true) when something
/// was written. Honors `dry_run` (prints intent, doesn't modify).
fn write_hook_script(path: &Path, dry_run: bool) -> io::Result<bool> {
    let need_write = match fs::read_to_string(path) {
        Ok(existing) => existing != HOOK_SCRIPT,
        Err(_) => true,
    };
    if !need_write && is_executable(path) {
        return Ok(false);
    }
    if dry_run {
        println!("DRY RUN — would write {} ({} bytes)", path.display(), HOOK_SCRIPT.len());
        return Ok(true);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, HOOK_SCRIPT)?;
    set_executable(path)?;
    Ok(true)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool { true }
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> { Ok(()) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyInstalled,
    Removed,
    NotFound,
}

/// Print the `~/.claude/settings.json` snippet the user needs to add.
/// Kept for backward compatibility — `--install-hooks` is the preferred path
/// because it merges idempotently into an existing settings file.
pub fn print_install_hint() {
    let path = hook_install_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/triage/hooks/triage-preuse.sh".to_string());
    println!("Run `triage --install-hooks` to install. Or, to merge by hand,");
    println!("first write the bash hook to {} and then add the following to ~/.claude/settings.json:", path);
    println!();
    println!("{{");
    println!("  \"hooks\": {{");
    println!("    \"PreToolUse\": [");
    println!("      {{");
    println!("        \"matcher\": \".*\",");
    println!("        \"hooks\": [");
    println!("          {{ \"type\": \"command\", \"command\": \"{path}\" }}");
    println!("        ]");
    println!("      }}");
    println!("    ]");
    println!("  }}");
    println!("}}");
    println!();
    println!("Or merge automatically: `triage --install-hooks` (add `--dry-run` to preview).");
}

/// Idempotently install the triage PreToolUse hook into ~/.claude/settings.json.
/// Existing entries pointing at our script are left alone (returns
/// `AlreadyInstalled`); otherwise we append our matcher group, leaving any
/// other PreToolUse entries (Navi, etc.) untouched. Always writes a `.bak`
/// next to the original before overwriting.
pub fn install_hooks(dry_run: bool) -> io::Result<InstallOutcome> {
    let script = hook_install_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate install path"))?;
    let script_str = script.display().to_string();
    let path = settings_json_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate settings.json"))?;
    let original = read_settings_json(&path)?;
    let (modified, outcome) = apply_install(&original, &script_str);

    // Always sync the on-disk script to the embedded copy. Done even on
    // AlreadyInstalled so a triage upgrade with hook-script changes refreshes
    // the bash file even when settings.json itself didn't need changes.
    let script_changed = write_hook_script(&script, dry_run)?;

    match outcome {
        InstallOutcome::AlreadyInstalled if !script_changed => {
            println!(
                "triage hook already installed at {} (no changes)",
                script_str
            );
        }
        InstallOutcome::AlreadyInstalled => {
            println!("Refreshed hook script at {} (settings.json unchanged)", script_str);
        }
        InstallOutcome::Installed if dry_run => {
            println!("DRY RUN — would update {}:", path.display());
            println!();
            println!("{}", serde_json::to_string_pretty(&modified)?);
        }
        InstallOutcome::Installed => {
            write_settings_json(&path, &modified)?;
            println!("Installed triage hook in {}", path.display());
            println!("  hook script: {}", script_str);
            println!("  backup:      {}.bak", path.display());
        }
        _ => {}
    }
    Ok(outcome)
}

/// Inverse of `install_hooks`: removes any PreToolUse entries pointing at our
/// script. Empty matcher groups are pruned, and empty `PreToolUse` /  `hooks`
/// keys are removed. Other tools' hook entries are untouched.
pub fn uninstall_hooks(dry_run: bool) -> io::Result<InstallOutcome> {
    let script = hook_install_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate install path"))?;
    let path = settings_json_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate settings.json"))?;
    let original = read_settings_json(&path)?;
    let (modified, outcome) = apply_uninstall(&original);

    let script_present = script.exists();

    match outcome {
        InstallOutcome::NotFound if !script_present => {
            println!("triage hook not present in {} (no changes)", path.display());
        }
        InstallOutcome::NotFound if dry_run => {
            println!("DRY RUN — would remove hook script {}", script.display());
        }
        InstallOutcome::NotFound => {
            fs::remove_file(&script)?;
            println!("Removed orphan hook script {}", script.display());
        }
        InstallOutcome::Removed if dry_run => {
            println!("DRY RUN — would update {}:", path.display());
            println!();
            println!("{}", serde_json::to_string_pretty(&modified)?);
            if script_present {
                println!();
                println!("DRY RUN — would remove hook script {}", script.display());
            }
        }
        InstallOutcome::Removed => {
            write_settings_json(&path, &modified)?;
            println!("Removed triage hook from {}", path.display());
            println!("  backup: {}.bak", path.display());
            if script_present {
                fs::remove_file(&script)?;
                println!("Removed hook script {}", script.display());
            }
        }
        _ => {}
    }
    Ok(outcome)
}

fn read_settings_json(path: &Path) -> io::Result<Value> {
    if !path.exists() {
        // Fresh file — start from an empty object so install can populate.
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let bytes = fs::read(path)?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("failed to parse {}: {e}", path.display())))
}

fn write_settings_json(path: &Path, v: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let backup = path.with_extension("json.bak");
        fs::copy(path, &backup)?;
    }
    let mut body = serde_json::to_string_pretty(v)?;
    body.push('\n');
    fs::write(path, body)?;
    Ok(())
}

fn apply_install(input: &Value, canonical_path: &str) -> (Value, InstallOutcome) {
    // Triage hooks identified by basename so a re-install migrates old
    // `~/workspace/triage/scripts/...` entries to the new canonical path.
    let stale_present = stale_triage_entries(input, canonical_path);
    let canonical_present = canonical_hook_present(input, canonical_path);

    if canonical_present && !stale_present {
        return (input.clone(), InstallOutcome::AlreadyInstalled);
    }

    let mut root = if input.is_object() {
        input.clone()
    } else {
        return (make_fresh_settings(canonical_path), InstallOutcome::Installed);
    };

    // Migrate: drop any triage-named entries (any path) before adding the
    // canonical one. Reuses `apply_uninstall` since the basename matcher is
    // what we want here too.
    if stale_present || canonical_present {
        let (purged, _) = apply_uninstall(&root);
        root = purged;
    }

    let root_obj = root.as_object_mut().unwrap();
    let hooks = root_obj
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !hooks.is_object() {
        return (make_fresh_settings(canonical_path), InstallOutcome::Installed);
    }
    let hooks_obj = hooks.as_object_mut().unwrap();
    let pre = hooks_obj
        .entry("PreToolUse".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !pre.is_array() {
        return (make_fresh_settings(canonical_path), InstallOutcome::Installed);
    }
    let pre_arr = pre.as_array_mut().unwrap();
    pre_arr.push(serde_json::json!({
        "matcher": ".*",
        "hooks": [
            { "type": "command", "command": canonical_path }
        ]
    }));
    (root, InstallOutcome::Installed)
}

fn apply_uninstall(input: &Value) -> (Value, InstallOutcome) {
    if !any_triage_hook_present(input) {
        return (input.clone(), InstallOutcome::NotFound);
    }
    let mut root = input.clone();
    let mut removed = false;
    if let Some(root_obj) = root.as_object_mut()
        && let Some(hooks) = root_obj.get_mut("hooks").and_then(|h| h.as_object_mut())
    {
        if let Some(pre) = hooks.get_mut("PreToolUse").and_then(|p| p.as_array_mut()) {
            for group in pre.iter_mut() {
                if let Some(group_hooks) =
                    group.get_mut("hooks").and_then(|h| h.as_array_mut())
                {
                    let before = group_hooks.len();
                    group_hooks.retain(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .map(|c| !is_triage_hook_command(c))
                            .unwrap_or(true)
                    });
                    if group_hooks.len() != before {
                        removed = true;
                    }
                }
            }
            pre.retain(|group| {
                group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(true)
            });
            if pre.is_empty() {
                hooks.remove("PreToolUse");
            }
        }
        if hooks.is_empty() {
            root_obj.remove("hooks");
        }
    }
    if removed {
        (root, InstallOutcome::Removed)
    } else {
        (input.clone(), InstallOutcome::NotFound)
    }
}

fn any_triage_hook_present(v: &Value) -> bool {
    triage_hook_commands(v).next().is_some()
}

fn canonical_hook_present(v: &Value, canonical: &str) -> bool {
    triage_hook_commands(v).any(|c| paths_equivalent(c, canonical))
}

fn stale_triage_entries(v: &Value, canonical: &str) -> bool {
    triage_hook_commands(v).any(|c| !paths_equivalent(c, canonical))
}

fn triage_hook_commands(v: &Value) -> impl Iterator<Item = &str> {
    v.get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .into_iter()
        .flat_map(|groups| groups.iter())
        .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
        .flat_map(|hs| hs.iter())
        .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
        .filter(|c| is_triage_hook_command(c))
}

/// Compare two path strings semantically: tilde-expand, then try to
/// canonicalize each (resolves symlinks + relative components). Falls back
/// to literal string equality. Necessary because settings.json may store
/// the hook command as `~/workspace/.../triage-preuse.sh` (tilde form) while
/// our generator emits the canonical absolute path — naive eq would
/// double-install.
fn paths_equivalent(a: &str, b: &str) -> bool {
    let a_exp = expand_tilde(a);
    let b_exp = expand_tilde(b);
    if a_exp == b_exp {
        return true;
    }
    if let (Ok(a_can), Ok(b_can)) = (
        Path::new(&a_exp).canonicalize(),
        Path::new(&b_exp).canonicalize(),
    ) {
        return a_can == b_can;
    }
    false
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    p.to_string()
}

fn make_fresh_settings(script: &str) -> Value {
    serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": ".*",
                    "hooks": [
                        { "type": "command", "command": script }
                    ]
                }
            ]
        }
    })
}
