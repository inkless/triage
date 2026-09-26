use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::models::Provider;

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
pub fn attach_to_sessions(
    approvals: Vec<PendingApproval>,
    sessions: &mut [crate::models::Session],
) {
    for a in approvals {
        // 1. session_id exact match.
        if let Some(idx) = sessions.iter().position(|s| {
            s.provider == crate::models::Provider::Claude && s.session_id == a.session_id
        }) {
            sessions[idx].pending_approvals.push(a);
            continue;
        }
        // 2. cwd match, preferring an active pane.
        let cwd_matches: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                (s.provider == crate::models::Provider::Claude && s.cwd == a.cwd).then_some(i)
            })
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
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/triage/hooks/triage-preuse.sh"))
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
        println!(
            "DRY RUN — would write {} ({} bytes)",
            path.display(),
            HOOK_SCRIPT.len()
        );
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
fn is_executable(_path: &Path) -> bool {
    true
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Print the `~/.claude/settings.json` snippet the user needs to add.
/// Kept for backward compatibility — `--install-hooks` is the preferred path
/// because it merges idempotently into an existing settings file.
pub fn print_install_hint() {
    let path = hook_install_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/triage/hooks/triage-preuse.sh".to_string());
    println!("Run `triage --install-hooks` to install. Or, to merge by hand,");
    println!(
        "first write the bash hook to {} and then add the following to ~/.claude/settings.json:",
        path
    );
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

/// `triage --install-hooks`: the approval hook only.
pub fn install_hooks(dry_run: bool) -> io::Result<()> {
    run_hooks_action(
        Action::Install,
        Selection {
            approval: true,
            ..Selection::default()
        },
        dry_run,
    )
}

/// `triage --uninstall-hooks`: the approval hook only.
pub fn uninstall_hooks(dry_run: bool) -> io::Result<()> {
    run_hooks_action(
        Action::Uninstall,
        Selection {
            approval: true,
            ..Selection::default()
        },
        dry_run,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Uninstall,
    Status,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Selection {
    claude: bool,
    approval: bool,
    midturn: bool,
}

const HOOKS_USAGE: &str = "usage: triage hooks install|uninstall|status [--claude] [--approval] [--midturn] [--dry-run]\n\
  With no hook flags: the Claude peer-messaging hooks.\n\
  --approval   the PreToolUse approval hook (same as --install-hooks)\n\
  --midturn    also deliver mail between tool calls (PostToolUse drain)";

/// `triage hooks install|uninstall|status …`.
pub fn cli_hooks(args: &[String]) -> i32 {
    let mut action = None;
    let mut selection = Selection::default();
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "install" if action.is_none() => action = Some(Action::Install),
            "uninstall" if action.is_none() => action = Some(Action::Uninstall),
            "status" if action.is_none() => action = Some(Action::Status),
            "--claude" => selection.claude = true,
            "--approval" => selection.approval = true,
            "--midturn" => selection.midturn = true,
            "--dry-run" => dry_run = true,
            "--help" | "-h" => {
                println!("{HOOKS_USAGE}");
                return 0;
            }
            other => {
                eprintln!("unknown arg {other:?}\n{HOOKS_USAGE}");
                return 2;
            }
        }
    }
    let Some(action) = action else {
        eprintln!("{HOOKS_USAGE}");
        return 2;
    };
    if !selection.claude && !selection.approval {
        selection.claude = true;
    }
    if selection.midturn && (action != Action::Install || !selection.claude) {
        eprintln!("--midturn only applies to installing the Claude messaging hooks\n{HOOKS_USAGE}");
        return 2;
    }
    match run_hooks_action(action, selection, dry_run) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn run_hooks_action(action: Action, selection: Selection, dry_run: bool) -> io::Result<()> {
    let settings_path = settings_json_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate settings.json"))?;
    let script = hook_install_path()
        .ok_or_else(|| io::Error::other("HOME is unset; cannot locate install path"))?;
    let script_str = script.display().to_string();
    let triage = if selection.claude && action != Action::Uninstall {
        let (path, warning) = triage_command_path();
        if let Some(warning) = warning {
            eprintln!("warning: {warning}");
        }
        path
    } else {
        String::new()
    };

    let mut specs = Vec::new();
    if selection.approval {
        specs.extend(approval_specs(action, &script_str));
    }
    if selection.claude {
        specs.extend(claude_messaging_specs(action, &triage, selection.midturn));
    }

    let original = read_settings_json(&settings_path)?;
    if action == Action::Status {
        print_status(&original, &specs);
        if selection.claude {
            print_waiters();
        }
        return Ok(());
    }

    let (edited, changes) = edit_settings(&original, &specs)?;
    let script_changed = match (selection.approval, action) {
        (true, Action::Install) => write_hook_script(&script, dry_run)?,
        _ => false,
    };
    if changes.is_empty() {
        if script_changed {
            println!("Refreshed hook script at {script_str} (settings.json unchanged)");
        } else {
            println!(
                "{} already up to date (no changes)",
                settings_path.display()
            );
        }
    } else if dry_run {
        println!("DRY RUN — would update {}:", settings_path.display());
        for change in &changes {
            println!("  {change}");
        }
        println!();
        println!("{}", serde_json::to_string_pretty(&edited)?);
    } else {
        let backup = write_settings_json(&settings_path, &edited)?;
        println!("Updated {}:", settings_path.display());
        for change in &changes {
            println!("  {change}");
        }
        if let Some(backup) = backup {
            println!("  backup: {}", backup.display());
        }
        if selection.claude && action == Action::Install {
            println!(
                "Claude reads hooks at startup: sessions started before now keep legacy paste until restarted."
            );
        }
    }

    if action == Action::Uninstall {
        if selection.approval && script.exists() {
            if dry_run {
                println!("DRY RUN — would remove hook script {}", script.display());
            } else {
                fs::remove_file(&script)?;
                println!("Removed hook script {}", script.display());
            }
        }
        if selection.claude && !dry_run {
            clear_host_records(Provider::Claude)?;
        }
    }
    Ok(())
}

/// A hook entry triage owns, found in settings by `is_ours` on its command.
struct HookSpec {
    label: &'static str,
    event: &'static str,
    is_ours: fn(&str) -> bool,
    /// `Some((matcher, handler))` to install; `None` to remove.
    desired: Option<(Option<&'static str>, Value)>,
}

fn approval_specs(action: Action, script: &str) -> Vec<HookSpec> {
    vec![HookSpec {
        label: "PreToolUse approval hook",
        event: "PreToolUse",
        is_ours: is_triage_hook_command,
        desired: (action != Action::Uninstall).then(|| {
            (
                Some(".*"),
                serde_json::json!({ "type": "command", "command": script }),
            )
        }),
    }]
}

pub const MESSAGING_HOOK_MARKER: &str = "--triage-hook=v1";

fn claude_messaging_specs(action: Action, triage: &str, midturn: bool) -> Vec<HookSpec> {
    let waiter = |event: &str| {
        serde_json::json!({
            "type": "command",
            "command": format!("{triage} inbox --hook claude {event} --wait {MESSAGING_HOOK_MARKER}"),
            "asyncRewake": true,
            "timeout": crate::peer_hooks::WAITER_TIMEOUT_SECS,
        })
    };
    let install = action != Action::Uninstall;
    vec![
        HookSpec {
            label: "Claude SessionStart mail waiter",
            event: "SessionStart",
            is_ours: |c| is_messaging_command(c, "claude session-start"),
            desired: install.then(|| (None, waiter("session-start"))),
        },
        HookSpec {
            label: "Claude Stop mail waiter",
            event: "Stop",
            is_ours: |c| is_messaging_command(c, "claude stop"),
            desired: install.then(|| (None, waiter("stop"))),
        },
        HookSpec {
            label: "Claude PostToolUse mail drain (--midturn)",
            event: "PostToolUse",
            is_ours: |c| is_messaging_command(c, "claude post-tool-use"),
            desired: (install && midturn).then(|| {
                (
                    Some(".*"),
                    serde_json::json!({
                        "type": "command",
                        "command": format!("{triage} inbox --hook claude post-tool-use {MESSAGING_HOOK_MARKER}"),
                        "timeout": 5,
                    }),
                )
            }),
        },
    ]
}

/// Any version of the marker counts as ours, so an upgrade replaces old
/// entries instead of stacking new ones beside them.
fn is_messaging_command(command: &str, hook: &str) -> bool {
    command.contains("--triage-hook=") && command.contains(&format!(" inbox --hook {hook} "))
}

fn shape_error(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("settings.json has an unexpected shape ({what}); not modifying it"),
    )
}

/// Applies every spec to a copy of `input`. Other tools' entries are never
/// touched, and an unexpected shape is an error rather than a rewrite.
fn edit_settings(input: &Value, specs: &[HookSpec]) -> io::Result<(Value, Vec<String>)> {
    let mut root = match input {
        Value::Object(_) => input.clone(),
        Value::Null => Value::Object(serde_json::Map::new()),
        _ => return Err(shape_error("top level is not an object")),
    };
    let mut changes = Vec::new();
    for spec in specs {
        let root_obj = root.as_object_mut().expect("checked above");
        if spec.desired.is_none() && !root_obj.contains_key("hooks") {
            continue;
        }
        let hooks = root_obj
            .entry("hooks")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| shape_error("\"hooks\" is not an object"))?;
        if spec.desired.is_none() && !hooks.contains_key(spec.event) {
            continue;
        }
        let groups = hooks
            .entry(spec.event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| shape_error(&format!("\"hooks.{}\" is not an array", spec.event)))?;
        for group in groups.iter() {
            if !group.get("hooks").is_some_and(Value::is_array) {
                return Err(shape_error(&format!(
                    "a \"hooks.{}\" group has no \"hooks\" array",
                    spec.event
                )));
            }
        }
        let ours: Vec<(usize, usize)> = groups
            .iter()
            .enumerate()
            .flat_map(|(gi, group)| {
                group["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, h)| {
                        h.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(spec.is_ours)
                    })
                    .map(move |(hi, _)| (gi, hi))
            })
            .collect();
        if let Some((matcher, handler)) = &spec.desired
            && let [(gi, hi)] = ours.as_slice()
            && handlers_equivalent(&groups[*gi]["hooks"][*hi], handler)
            && groups[*gi].get("matcher").and_then(Value::as_str) == *matcher
        {
            continue;
        }
        if spec.desired.is_none() && ours.is_empty() {
            continue;
        }
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = handlers.len();
                handlers.retain(|h| {
                    !h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(spec.is_ours)
                });
                if handlers.len() != before && handlers.is_empty() {
                    group["hooks"] = Value::Null;
                }
            }
        }
        groups.retain(|group| !group["hooks"].is_null());
        match &spec.desired {
            Some((matcher, handler)) => {
                let mut group = serde_json::Map::new();
                if let Some(matcher) = matcher {
                    group.insert("matcher".into(), Value::String((*matcher).to_string()));
                }
                group.insert("hooks".into(), Value::Array(vec![handler.clone()]));
                groups.push(Value::Object(group));
                let verb = if ours.is_empty() { "add" } else { "update" };
                changes.push(format!("{verb} {}", spec.label));
            }
            None => changes.push(format!("remove {}", spec.label)),
        }
        if groups.is_empty() {
            hooks.remove(spec.event);
        }
        if hooks.is_empty() {
            root_obj_remove_hooks(&mut root);
        }
    }
    Ok((root, changes))
}

fn root_obj_remove_hooks(root: &mut Value) {
    if let Some(obj) = root.as_object_mut() {
        obj.remove("hooks");
    }
}

fn handlers_equivalent(existing: &Value, desired: &Value) -> bool {
    let (Some(a), Some(b)) = (existing.as_object(), desired.as_object()) else {
        return false;
    };
    a.len() == b.len()
        && b.iter()
            .all(|(key, want)| match (key.as_str(), a.get(key)) {
                ("command", Some(Value::String(have))) => want
                    .as_str()
                    .is_some_and(|want| commands_equivalent(have, want)),
                (_, Some(have)) => have == want,
                _ => false,
            })
}

/// Compares the executable path semantically (tilde vs absolute, symlinks)
/// and the arguments literally.
fn commands_equivalent(a: &str, b: &str) -> bool {
    let (a_path, a_rest) = split_command(a);
    let (b_path, b_rest) = split_command(b);
    a_rest == b_rest && paths_equivalent(&a_path, &b_path)
}

fn split_command(command: &str) -> (String, &str) {
    let command = command.trim_start();
    if let Some(quoted) = command.strip_prefix('\'')
        && let Some(end) = quoted.find('\'')
    {
        return (quoted[..end].to_string(), quoted[end + 1..].trim_start());
    }
    match command.split_once(char::is_whitespace) {
        Some((path, rest)) => (path.to_string(), rest.trim_start()),
        None => (command.to_string(), ""),
    }
}

/// The first `triage` on PATH that is this binary, kept in its PATH form
/// (e.g. `/opt/homebrew/bin/triage`) so upgrades that swap the target keep
/// working; otherwise this binary's own path.
fn triage_command_path() -> (String, Option<String>) {
    let exe = std::env::current_exe().ok();
    let exe_canonical = exe.as_ref().and_then(|e| e.canonicalize().ok());
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("triage"))
            .find(|candidate| {
                is_executable(candidate)
                    && candidate.canonicalize().ok().as_ref() == exe_canonical.as_ref()
            })
    });
    match (on_path, exe) {
        (Some(path), _) => (shell_word(&path.display().to_string()), None),
        (None, Some(exe)) => (
            shell_word(&exe.display().to_string()),
            Some(format!(
                "this triage binary is not the `triage` on PATH; hooks will run {}",
                exe.display()
            )),
        ),
        (None, None) => (
            "triage".to_string(),
            Some("cannot locate the triage binary".into()),
        ),
    }
}

fn shell_word(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn print_status(settings: &Value, specs: &[HookSpec]) {
    for spec in specs {
        let handlers: Vec<&Value> = settings
            .pointer(&format!("/hooks/{}", spec.event))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|g| g.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(spec.is_ours)
            })
            .collect();
        let state = match (&spec.desired, handlers.as_slice()) {
            (None, []) => continue,
            (Some(_), []) => "missing".to_string(),
            (None, _) => "installed (not requested)".to_string(),
            (Some((_, want)), [have]) => {
                let command = have.get("command").and_then(Value::as_str).unwrap_or("");
                let (path, _) = split_command(command);
                if !is_executable(Path::new(&expand_tilde(&path))) {
                    format!("stale ({path} is missing or not executable)")
                } else if !handlers_equivalent(have, want) {
                    "stale (differs from this triage's entry)".to_string()
                } else {
                    "installed".to_string()
                }
            }
            (Some(_), many) => format!("duplicated ({} entries)", many.len()),
        };
        println!("{:<44} {state}", spec.label);
    }
}

fn print_waiters() {
    let store = crate::mailbox::Store::open_default();
    let hosts: Vec<_> = store
        .hosts()
        .into_iter()
        .filter(|(host, _)| crate::mailbox::liveness(*host) == crate::mailbox::Liveness::Alive)
        .collect();
    if hosts.is_empty() {
        println!("No live hook-capable sessions.");
        return;
    }
    for (host, record) in hosts {
        let agent = store
            .lineage_root(&record.current_session)
            .unwrap_or_else(|| record.current_session.clone());
        let waiter = match (
            record.provider,
            store.try_lock(crate::mailbox::LockKind::Waiter, &agent),
        ) {
            (Provider::Claude, Ok(None)) => "waiter live",
            (Provider::Claude, Ok(Some(_))) => "no waiter",
            (Provider::Claude, Err(_)) => "waiter unknown",
            (Provider::Codex, _) => "sync hooks",
        };
        println!(
            "pid {:<7} {:<6} agent {} session {}  {waiter}",
            host.pid,
            record.provider.label(),
            crate::mailbox::short_id(&agent),
            crate::mailbox::short_id(&record.current_session),
        );
    }
}

/// A host record is what makes a target hook-capable; without the hooks it
/// must fall back to legacy paste.
fn clear_host_records(provider: Provider) -> io::Result<()> {
    let store = crate::mailbox::Store::open_default();
    for (host, record) in store.hosts() {
        if record.provider == provider {
            store.remove_host(host)?;
        }
    }
    Ok(())
}

fn read_settings_json(path: &Path) -> io::Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let bytes = fs::read(path)?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::other(format!("failed to parse {}: {e}", path.display())))
}

const MAX_BACKUPS: usize = 5;

/// Atomic replace that keeps the file's mode, after a timestamped 0600
/// backup (newest `MAX_BACKUPS` kept). Returns the backup path.
fn write_settings_json(path: &Path, v: &Value) -> io::Result<Option<PathBuf>> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("settings path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("settings path has no file name"))?;
    let backup_prefix = format!("{name}.triage-bak-");
    let mode = fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o777);
    let backup = if path.exists() {
        let backup = dir.join(format!("{backup_prefix}{}", crate::mailbox::now_ms()));
        fs::copy(path, &backup)?;
        fs::set_permissions(&backup, fs::Permissions::from_mode(0o600))?;
        let mut backups: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&backup_prefix))
            })
            .collect();
        backups.sort();
        let excess = backups.len().saturating_sub(MAX_BACKUPS);
        for old in &backups[..excess] {
            let _ = fs::remove_file(old);
        }
        Some(backup)
    } else {
        None
    };
    let mut body = serde_json::to_string_pretty(v)?;
    body.push('\n');
    let staged = dir.join(format!(".{name}.triage-{}", crate::mailbox::new_nonce()));
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&staged)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&staged, path).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;
    Ok(backup)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TRIAGE: &str = "/opt/homebrew/bin/triage";

    fn others() -> Value {
        json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "~/.claude/hooks/block-log-md-edits.sh"}]}
                ],
                "Stop": [
                    {"hooks": [{"type": "command", "command": "navi-stop.sh"}]}
                ]
            }
        })
    }

    fn install(input: &Value, midturn: bool) -> (Value, Vec<String>) {
        let mut specs = approval_specs(
            Action::Install,
            "/home/u/.config/triage/hooks/triage-preuse.sh",
        );
        specs.extend(claude_messaging_specs(Action::Install, TRIAGE, midturn));
        edit_settings(input, &specs).unwrap()
    }

    fn handlers<'a>(v: &'a Value, event: &str) -> Vec<&'a str> {
        v["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn install_appends_ours_and_leaves_other_hooks_untouched() {
        let (out, changes) = install(&others(), false);
        assert_eq!(changes.len(), 3);
        assert_eq!(out["model"], "opus");
        assert_eq!(
            out["hooks"]["PreToolUse"][0],
            others()["hooks"]["PreToolUse"][0]
        );
        assert_eq!(out["hooks"]["Stop"][0], others()["hooks"]["Stop"][0]);
        assert_eq!(
            handlers(&out, "Stop")[1],
            "/opt/homebrew/bin/triage inbox --hook claude stop --wait --triage-hook=v1"
        );
        assert_eq!(out["hooks"]["Stop"][1]["hooks"][0]["asyncRewake"], true);
        assert!(out["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn reinstall_is_a_no_op() {
        let (once, _) = install(&others(), true);
        let (twice, changes) = install(&once, true);
        assert!(changes.is_empty(), "{changes:?}");
        assert_eq!(twice, once);
    }

    #[test]
    fn an_equivalent_path_spelling_is_not_reinstalled() {
        let dir = std::env::temp_dir().join(format!("triage-hooks-eq-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("triage");
        fs::write(&real, "").unwrap();
        let link = dir.join("triage-link");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let installed = json!({"hooks": {"Stop": [{"hooks": [{
            "type": "command",
            "command": format!("{} inbox --hook claude stop --wait --triage-hook=v1", link.display()),
            "asyncRewake": true, "timeout": 86400
        }]}]}});
        let specs = claude_messaging_specs(Action::Install, &real.display().to_string(), false);
        let stop = specs
            .into_iter()
            .filter(|s| s.event == "Stop")
            .collect::<Vec<_>>();
        let (_, changes) = edit_settings(&installed, &stop).unwrap();
        assert!(changes.is_empty(), "{changes:?}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_and_older_version_entries_are_replaced_not_duplicated() {
        let old = json!({"hooks": {
            "PreToolUse": [{"matcher": ".*", "hooks": [{"type": "command", "command": "/old/checkout/scripts/hooks/triage-preuse.sh"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "/old/triage inbox --hook claude stop --wait --triage-hook=v0"}]}]
        }});
        let (out, _) = install(&old, false);
        assert_eq!(
            handlers(&out, "PreToolUse"),
            ["/home/u/.config/triage/hooks/triage-preuse.sh"]
        );
        assert_eq!(
            handlers(&out, "Stop"),
            ["/opt/homebrew/bin/triage inbox --hook claude stop --wait --triage-hook=v1"]
        );
    }

    #[test]
    fn unexpected_shapes_are_refused_rather_than_rewritten() {
        for bad in [
            json!([]),
            json!({"hooks": []}),
            json!({"hooks": {"Stop": {}}}),
            json!({"hooks": {"Stop": [{"command": "x"}]}}),
        ] {
            let specs = claude_messaging_specs(Action::Install, TRIAGE, false);
            assert!(edit_settings(&bad, &specs).is_err(), "{bad}");
        }
    }

    #[test]
    fn uninstall_removes_only_our_handlers_from_shared_groups() {
        let shared = json!({"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "navi-stop.sh"},
            {"type": "command", "command": "/opt/homebrew/bin/triage inbox --hook claude stop --wait --triage-hook=v1"}
        ]}]}});
        let (out, changes) = edit_settings(
            &shared,
            &claude_messaging_specs(Action::Uninstall, "", false),
        )
        .unwrap();
        assert_eq!(changes, ["remove Claude Stop mail waiter"]);
        assert_eq!(handlers(&out, "Stop"), ["navi-stop.sh"]);
        let (clean, _) = edit_settings(
            &install(&json!({}), true).0,
            &claude_messaging_specs(Action::Uninstall, "", false),
        )
        .unwrap();
        assert_eq!(clean["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(clean["hooks"].get("Stop").is_none());
        assert!(clean["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn a_narrowed_matcher_is_restored() {
        let (mut installed, _) = install(&json!({}), true);
        installed["hooks"]["PostToolUse"][0]["matcher"] = json!("Bash");
        let (out, changes) = install(&installed, true);
        assert_eq!(
            changes,
            ["update Claude PostToolUse mail drain (--midturn)"]
        );
        assert_eq!(out["hooks"]["PostToolUse"][0]["matcher"], ".*");
    }

    #[test]
    fn midturn_is_declarative() {
        let (with, _) = install(&json!({}), true);
        assert_eq!(handlers(&with, "PostToolUse").len(), 1);
        let (without, changes) = install(&with, false);
        assert_eq!(
            changes,
            ["remove Claude PostToolUse mail drain (--midturn)"]
        );
        assert!(without["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn settings_writes_keep_the_mode_and_cap_backups() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("triage-hooks-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        for n in 0..7 {
            write_settings_json(&path, &json!({ "n": n })).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o644);
        assert_eq!(read_settings_json(&path).unwrap(), json!({"n": 6}));
        let backups: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".triage-bak-"))
            .collect();
        assert_eq!(backups.len(), MAX_BACKUPS);
        assert!(backups.iter().all(|b| mode(b) == 0o600));
        let _ = fs::remove_dir_all(dir);
    }
}
