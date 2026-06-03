use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::Pane;

/// Returns a map of pane_pid → Pane.
pub fn list_panes() -> HashMap<u32, Pane> {
    list_panes_checked().unwrap_or_default()
}

pub fn list_panes_checked() -> Result<HashMap<u32, Pane>, String> {
    let mut map = HashMap::new();
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{window_index}.#{pane_index}|#{pane_pid}|#{pane_tty}|#{pane_current_command}|#{pane_current_path}|#{?pane_active,1,0}|#{pane_id}|#{window_name}",
        ])
        .output()
        .map_err(|e| format!("tmux list-panes failed: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            return Err(format!("tmux list-panes exited {}", out.status));
        }
        return Err(format!("tmux list-panes exited {}: {detail}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // splitn(9, …) keeps any embedded `|` in the window_name itself in
        // the final slot — pane_id (slot 7) never contains `|`.
        let parts: Vec<&str> = line.splitn(9, '|').collect();
        if parts.len() < 9 {
            continue;
        }
        let Ok(pid) = parts[2].parse::<u32>() else {
            continue;
        };
        // pane_active=1 marks the most-recently-focused pane within each
        // tmux session, even when that session isn't currently attached.
        // That makes it usable as a "which pane was the user last in for
        // this cwd?" signal regardless of where they're typing now.
        let active = parts[6] == "1";
        map.insert(
            pid,
            Pane {
                target: format!("{}:{}", parts[0], parts[1]),
                tmux_session: parts[0].to_string(),
                window_name: parts[8].to_string(),
                pane_id: parts[7].to_string(),
                pid,
                tty: parts[3].to_string(),
                current_command: parts[4].to_string(),
                cwd: PathBuf::from(parts[5]),
                active,
            },
        );
    }
    Ok(map)
}

/// One-shot snapshot of pid → ppid for the whole system. Consumers walk the
/// map in-process instead of calling `ps` per hop. With up to 8 hops × N
/// sessions per refresh the per-call cost was the dominant lag source.
pub fn build_ppid_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        map.insert(pid, ppid);
    }
    map
}

/// Walk parent PIDs upward (max `max_hops`) until we find one in `pane_pids`.
pub fn find_owning_pane(
    pid: u32,
    pane_pids: &HashMap<u32, Pane>,
    ppid_map: &HashMap<u32, u32>,
    max_hops: usize,
) -> Option<Pane> {
    let mut cur = pid;
    for _ in 0..=max_hops {
        if let Some(pane) = pane_pids.get(&cur) {
            return Some(pane.clone());
        }
        let ppid = *ppid_map.get(&cur)?;
        if ppid <= 1 {
            return None;
        }
        cur = ppid;
    }
    None
}

/// Find an existing pane whose foreground command is `triage` and focus it,
/// or spawn one in a new window of the current tmux session if none exist.
/// Designed to be wired to a tmux key binding (e.g. `M-t`); deliberately
/// skips discovery / transcript-parsing / watcher init so the focus switch
/// stays under ~30ms cold.
///
/// Detection is **PID-based, not command-name-based**: triage writes its pid
/// to `~/.claude/triage/.alive` on startup (`AliveGuard`), and we walk the
/// process tree to find the pane that contains that pid. The earlier
/// `pane_current_command == "triage"` exact match was brittle — the user
/// hit a regression where every M-t press spawned a new window because the
/// running triage's pane_current_command didn't match (likely because the
/// pane's wrapper shell hadn't yet ceded foreground, or tmux reported a
/// path-prefixed name). PID matching is robust against all of those.
///
/// `zoom` is the mobile flow: after focusing the triage pane, `resize-pane
/// -Z` it so triage fills the screen. The `window_zoomed_flag` pre-check
/// avoids un-zooming an already-zoomed pane (since `-Z` toggles). When a
/// triage pane doesn't exist and we have to spawn one, the spawned process
/// is launched with `--zoom-on-jump` so its in-process Enter behavior
/// matches the binding's intent (target pane gets zoomed too).
/// Silent-attach probe used by plain `triage` invocations: if a live
/// triage instance is recorded, switch the user's tmux client to its
/// pane (with optional zoom) and return Ok(true). Returns Ok(false) when
/// nothing live was found — caller falls through to running the TUI in
/// the current pane.
///
/// This is the "single-instance with attach-on-second-start" behavior:
/// typing `triage` from a shell when one's already running auto-jumps
/// to it instead of starting a duplicate. PaneStale (process dead, pane
/// still around) is intentionally NOT attached here — that case means
/// the previous triage exited, so falling through to fresh-launch is
/// what the user wants.
pub fn attach_if_alive(zoom: bool) -> std::io::Result<bool> {
    let panes = list_panes();
    if let Some(LocatedTriage::Live(pane)) = locate_triage(&panes) {
        focus_and_maybe_zoom(&pane, zoom)?;
        return Ok(true);
    }
    Ok(false)
}

/// Width of the *calling tmux client* in columns. Different from the
/// triage pane's `area.width` (ratatui) — that one reflects the pane
/// subset, which a split-screen layout can shrink even on a desktop
/// terminal. Client width is the actual terminal/device dimension we want
/// for "is the user on a narrow device?" auto-zoom decisions: laptop
/// fullscreen tmux is 200+ regardless of pane layout, iPad portrait is
/// ~120, iPhone is ~30–80.
///
/// When triage is invoked from a tmux client (the normal case), tmux's
/// `display-message` resolves the calling client via `TMUX_PANE` →
/// containing window → most-recently-active client. Returns None outside
/// tmux or if the query fails.
pub fn current_client_width() -> Option<u16> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#{client_width}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub fn jump_to_self(zoom: bool) -> std::io::Result<()> {
    let panes = list_panes();
    let cmd = if zoom {
        "triage --zoom-on-jump"
    } else {
        "triage"
    };

    match locate_triage(&panes) {
        Some(LocatedTriage::Live(pane)) => {
            focus_and_maybe_zoom(&pane, zoom)?;
            Ok(())
        }
        // Triage's pane is still alive but the process inside isn't —
        // user force-killed triage with SIGKILL, or the pane is stuck on a
        // shell after a panic. Respawn in place rather than spawning a
        // duplicate window. Prevents the runaway-window bug seen when M-t
        // keeps creating new triage tabs.
        Some(LocatedTriage::PaneStale(pane)) => {
            Command::new("tmux")
                .args(["respawn-pane", "-k", "-t", &pane.target, cmd])
                .status()?;
            focus_and_maybe_zoom(&pane, zoom)?;
            Ok(())
        }
        None => {
            Command::new("tmux")
                .args(["new-window", "-n", "triage", cmd])
                .status()?;
            Ok(())
        }
    }
}

fn focus_and_maybe_zoom(pane: &Pane, zoom: bool) -> std::io::Result<()> {
    // Same three-step pin as jump_to: session via switch-client, window
    // via select-window, pane via select-pane. select-pane alone updates
    // the session's active pane but doesn't always make that window the
    // calling client's current window — symptom seen as "M-t did nothing
    // until I manually navigated to triage's window once."
    let window = pane
        .target
        .rsplit_once('.')
        .map(|(w, _)| w)
        .unwrap_or(&pane.target);
    Command::new("tmux")
        .args(["switch-client", "-t", &pane.tmux_session])
        .status()?;
    Command::new("tmux")
        .args(["select-window", "-t", window])
        .status()?;
    Command::new("tmux")
        .args(["select-pane", "-t", &pane.target])
        .status()?;
    if zoom {
        let already_zoomed = Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                &pane.target,
                "#{window_zoomed_flag}",
            ])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        if !already_zoomed {
            Command::new("tmux")
                .args(["resize-pane", "-Z", "-t", &pane.target])
                .status()?;
        }
    }
    Ok(())
}

enum LocatedTriage {
    /// Triage process is alive AND owns the recorded pane.
    Live(Pane),
    /// Triage's recorded pane still exists in tmux, but the process inside
    /// is dead. Reuse via respawn-pane.
    PaneStale(Pane),
}

fn locate_triage(panes: &HashMap<u32, Pane>) -> Option<LocatedTriage> {
    let record = crate::approval::read_alive_record()?;
    let alive = crate::discovery::pid_alive(record.pid);
    // The ppid walk is only consulted when triage is alive; skip the (cheap but
    // not free) system-wide `ps` snapshot when we already know it's dead.
    let ppid_map = if alive {
        build_ppid_map()
    } else {
        HashMap::new()
    };
    locate_triage_from_record(&record, alive, panes, &ppid_map)
}

/// Resolve which tmux pane (if any) hosts the recorded triage instance, using
/// only the authoritative signals from `.alive`: the recorded pid and pane id.
///
/// We deliberately do **not** match panes by `pane_current_command == "triage"`.
/// That heuristic briefly matches the very pane a fresh `triage` is launched
/// from (the new process is already foreground there before `.alive` is
/// rewritten), so the silent-attach probe resolved to its own pane and exited
/// before the TUI started — the TRI-123 regression. Pid + pane_id can only ever
/// point at an *already-running* instance, never the launching pane, so the
/// self-attach failure mode is impossible by construction here.
///
/// When `.alive` is missing entirely (clean exit, or a crash that skipped
/// `AliveGuard`'s cleanup), we return `None` and the caller launches fresh —
/// we don't try to rediscover an orphaned instance.
fn locate_triage_from_record(
    record: &crate::approval::AliveRecord,
    alive: bool,
    panes: &HashMap<u32, Pane>,
    ppid_map: &HashMap<u32, u32>,
) -> Option<LocatedTriage> {
    // Primary: walk the process tree from triage's pid to its pane. Robust
    // against tmux window renumbering and a stale/absent recorded pane_id.
    if alive && let Some(pane) = find_owning_pane(record.pid, panes, ppid_map, 8) {
        return Some(LocatedTriage::Live(pane));
    }

    // Fallback: the recorded pane_id. If triage is alive but the pid walk
    // missed it (e.g. a ppid chain deeper than the hop budget), reuse the
    // recorded pane; if the process is gone but its pane survives, report it
    // stale so the caller respawns in place rather than spawning a duplicate.
    if let Some(pane) = record
        .pane_id
        .as_deref()
        .and_then(|pane_id| pane_by_id(panes, pane_id))
    {
        return Some(if alive {
            LocatedTriage::Live(pane)
        } else {
            LocatedTriage::PaneStale(pane)
        });
    }

    None
}

fn pane_by_id(panes: &HashMap<u32, Pane>, pane_id: &str) -> Option<Pane> {
    panes.values().find(|pane| pane.pane_id == pane_id).cloned()
}

/// Switch tmux focus to the given pane target (`session:window.pane`). When
/// `zoom` is true, also `resize-pane -Z` so the target fills the screen —
/// designed for the popup-launch flow on mobile, where the user is jumping
/// onto a tiny phone screen and probably wants the destination pane maximized.
pub fn jump_to(target: &str, zoom: bool) -> std::io::Result<()> {
    // Three-step pin: session → window → pane. Empirically, `select-pane`
    // alone doesn't reliably switch the *window* when the target window
    // differs from the calling client's current window — symptom seen
    // when triage was spawned via M-p (new window in same session as the
    // jump target): status said "jumped" but the client stayed on
    // triage's window. Explicit select-window resolves it.
    let session = target.split_once(':').map(|(s, _)| s).unwrap_or("");
    let window = target.rsplit_once('.').map(|(w, _)| w).unwrap_or(target);
    if !session.is_empty() {
        Command::new("tmux")
            .args(["switch-client", "-t", session])
            .status()?;
    }
    Command::new("tmux")
        .args(["select-window", "-t", window])
        .status()?;
    Command::new("tmux")
        .args(["select-pane", "-t", target])
        .status()?;
    if zoom {
        // `-Z` toggles, so check `window_zoomed_flag` first — without this,
        // jumping to an already-zoomed pane would un-zoom it.
        let already_zoomed = Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                target,
                "#{window_zoomed_flag}",
            ])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false);
        if !already_zoomed {
            Command::new("tmux")
                .args(["resize-pane", "-Z", "-t", target])
                .status()?;
        }
    }
    Ok(())
}

/// Send tmux key sequences to a pane. Each entry is either a literal string
/// (e.g. `"1"`) or a tmux key name (e.g. `"Enter"`, `"Escape"`). Used to
/// answer Claude Code's native permission prompt remotely when our own hook
/// is bypassed (e.g. by a managed-settings `allowManagedHooksOnly` policy).
pub fn send_keys(target: &str, keys: &[&str]) -> std::io::Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args(["send-keys", "-t", target]);
    for k in keys {
        cmd.arg(k);
    }
    let status = cmd.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "tmux send-keys exited {status}"
        )))
    }
}

pub fn new_window(
    name: &str,
    cwd: &Path,
    command: &str,
    detached: bool,
) -> std::io::Result<String> {
    let command = command_in_cwd(cwd, command);
    let mut tmux = Command::new("tmux");
    tmux.args(["new-window", "-P", "-F", "#{pane_id}"]);
    if detached {
        tmux.arg("-d");
    }
    let output = tmux
        .arg("-c")
        .arg(cwd)
        .args(["-n", name])
        .arg(command)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(std::io::Error::other(if stderr.is_empty() {
            format!("tmux new-window exited {}", output.status)
        } else {
            stderr
        }));
    }
    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane_id.is_empty() {
        return Err(std::io::Error::other("tmux new-window returned no pane id"));
    }
    Ok(pane_id)
}

fn command_in_cwd(cwd: &Path, command: &str) -> String {
    format!(
        "cd {} && {}",
        shell_quote(&cwd.display().to_string()),
        command
    )
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn send_after_boot(target: &str, text: &str, settle: Duration) -> std::io::Result<()> {
    thread::sleep(settle);
    send_keys(target, &["Enter"])?;
    thread::sleep(Duration::from_millis(300));
    paste_text_and_enter(target, text)
}

/// Paste literal text into a pane through a tmux buffer, then submit it with
/// Enter. Used for both one-line and multiline agent messages so callers do
/// not need to know transport details.
pub fn paste_text_and_enter(target: &str, text: &str) -> std::io::Result<()> {
    let nonce = buffer_nonce();
    let buffer_name = format!("triage-msg-{nonce}");
    let temp_dir = triage_temp_dir();
    fs::create_dir_all(&temp_dir)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&temp_dir, fs::Permissions::from_mode(0o700));
    }
    let temp_file = temp_dir.join(format!("{buffer_name}.txt"));

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    {
        let mut file = options.open(&temp_file)?;
        file.write_all(text.as_bytes())?;
    }

    let load_result = Command::new("tmux")
        .args(["load-buffer", "-b", &buffer_name])
        .arg(&temp_file)
        .status();
    let _ = fs::remove_file(&temp_file);
    let status = load_result?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "tmux load-buffer exited {status}"
        )));
    }

    let paste_status = Command::new("tmux")
        .args(["paste-buffer", "-d", "-p", "-b", &buffer_name, "-t", target])
        .status()?;
    if !paste_status.success() {
        let _ = Command::new("tmux")
            .args(["delete-buffer", "-b", &buffer_name])
            .status();
        return Err(std::io::Error::other(format!(
            "tmux paste-buffer exited {paste_status}"
        )));
    }

    send_keys(target, &["Enter"])
}

fn buffer_nonce() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid}-{nanos}")
}

fn triage_temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config/triage/tmp"))
                .unwrap_or_else(|| PathBuf::from("/tmp"))
        })
        .join("triage")
}

/// Capture the visible pane content plus 200 lines of scrollback. Used as a
/// fallback source for "what is Claude asking permission for" — the transcript
/// JSONL doesn't include a pending tool_use until after the user approves and
/// the round-trip completes.
pub fn capture_pane(target: &str) -> Option<String> {
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-S", "-200", "-t", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Bounded variant: capture only the last `lines` lines of the pane. Used
/// when we only need to spot the permission-UI anchor near the bottom (the
/// box never spans more than ~30 lines, so 40 leaves headroom). Cheaper than
/// `capture_pane`'s 200-line default.
pub fn capture_pane_tail(target: &str, lines: u32) -> Option<String> {
    let start = format!("-{lines}");
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-S", &start, "-t", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like `capture_pane_tail` but preserves ANSI styling (`-e`). Needed to tell
/// Claude's faint ghost/placeholder composer text from real input — see
/// `has_draft_input`. Run `strip_ansi` over it for plain-text line matching.
pub fn capture_pane_tail_ansi(target: &str, lines: u32) -> Option<String> {
    let start = format!("-{lines}");
    let out = Command::new("tmux")
        .args(["capture-pane", "-e", "-p", "-S", &start, "-t", target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True iff the captured pane shows a Claude permission prompt UI in its
/// most recent block. We require two distinct UI lines to BOTH appear as
/// trimmed exact-line matches:
///
///   `❯ 1. Yes`                         (the live cursor on option 1)
///   `Esc to cancel · Tab to amend`     (the prompt footer)
///
/// Anchoring on whole lines (not substring) is what keeps this from
/// false-firing on code edits / diffs / prose that *quote* these strings
/// inside source — Claude renders them on their own lines (with at most
/// leading whitespace), while a quote in code has surrounding chars.
pub fn has_pending_permission_prompt(pane: &str) -> bool {
    let mut found_cursor = false;
    let mut found_footer = false;
    for line in pane.lines() {
        let trimmed = line.trim();
        if trimmed == "❯ 1. Yes" {
            found_cursor = true;
        }
        if trimmed == "Esc to cancel · Tab to amend" {
            found_footer = true;
        }
        if found_cursor && found_footer {
            return true;
        }
    }
    false
}

/// True iff the captured pane shows Codex's native approval UI.
///
/// Codex has no sessions-json equivalent of Claude's `status=waiting`, so
/// this is intentionally paired with the rollout signal that the unfinished
/// tool call requested approval. We require both the approval question and
/// positive/negative choices in the visible pane block to avoid treating
/// stale scrollback or quoted source text as a live prompt.
pub fn has_codex_permission_prompt(pane: &str) -> bool {
    let mut found_question = false;
    let mut found_yes = false;
    let mut found_no = false;

    for line in pane.lines() {
        let trimmed = trim_codex_prompt_line(line);
        if is_codex_prompt_question(trimmed) {
            found_question = true;
        }
        if is_codex_yes_choice(trimmed) {
            found_yes = true;
        }
        if is_codex_no_choice(trimmed) {
            found_no = true;
        }
        if found_question && found_yes && found_no {
            return true;
        }
    }

    false
}

/// Best-effort: does the target's Claude composer hold real, unsent text the
/// user is mid-typing? Sending into it would paste onto their draft and submit
/// the mangled result. Claude Code renders the input line as `❯ <text>` (the
/// marker is U+276F) at the bottom of the pane. We scan from the bottom for the
/// first such line — that's the composer — and report whether it holds real
/// input.
///
/// Crucially, Claude draws a **faint** (ANSI SGR 2) ghost/placeholder hint in
/// the empty composer (e.g. a suggested next prompt). That ghost text is NOT
/// the user's input — pasting over it is fine. So `pane` must be captured WITH
/// escapes (`capture_pane_tail_ansi`), and only **non-faint** glyphs after the
/// marker count as a real draft. Without the styling these are indistinguishable
/// and the ghost text false-triggers the gate (TRI-131 regression).
///
/// Heuristic and Claude-specific: Codex's composer differs and isn't detected
/// (returns false); we err toward false (allow the send) on anything we don't
/// recognize rather than block on uncertainty.
pub fn has_draft_input(pane: &str) -> bool {
    for line in pane.lines().rev() {
        if let Some(pos) = line.find('❯') {
            return composer_has_real_text(&line[pos + '❯'.len_utf8()..]);
        }
    }
    false
}

/// True if the composer content after the `❯` marker contains visible text at
/// normal intensity. Two styles mark text as *not* real input, so they don't
/// count:
///   - **faint** (SGR 2) — Claude's ghost/placeholder hint is drawn faint.
///   - **reverse** (SGR 7) — the terminal cursor block. On a placeholder the
///     cursor sits at position 0, highlighting the first ghost char in
///     reverse video; counting it would re-introduce the false positive even
///     though the rest of the hint is faint. Real input keeps the cursor
///     *after* the typed text, so the typed glyphs stay normal-intensity.
///
/// `s` may carry ANSI SGR escapes from a `-e` capture.
fn composer_has_real_text(s: &str) -> bool {
    let mut faint = false;
    let mut reverse = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Consume a CSI escape: ESC '[' params <final-letter>. Only `m`
            // (SGR) carries the intensity/reverse state we care about.
            if chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte = '\0';
                for p in chars.by_ref() {
                    if p.is_ascii_alphabetic() {
                        final_byte = p;
                        break;
                    }
                    params.push(p);
                }
                if final_byte == 'm' {
                    for code in params.split(';') {
                        match code {
                            "2" => faint = true,
                            "7" => reverse = true,
                            "22" => faint = false,
                            "27" => reverse = false,
                            "" | "0" => {
                                faint = false;
                                reverse = false;
                            }
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }
        if !faint && !reverse && c != '\u{a0}' && !c.is_whitespace() {
            return true;
        }
    }
    false
}

/// Remove ANSI CSI escape sequences from `s`, yielding the visible glyphs only.
/// Used so the plain-text line matchers (permission-prompt detection) can run
/// on a `-e` capture.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for p in chars.by_ref() {
                    if p.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexPromptChoice {
    Yes,
    No,
    Other,
}

/// Return the currently highlighted Codex approval-menu choice, if the
/// captured pane includes Codex's cursor marker.
pub fn codex_selected_permission_choice(pane: &str) -> Option<CodexPromptChoice> {
    let mut choice = None;
    for line in pane.lines() {
        let Some(trimmed) = selected_codex_prompt_line(line) else {
            continue;
        };
        choice = Some(if is_codex_yes_choice(trimmed) {
            CodexPromptChoice::Yes
        } else if is_codex_no_choice(trimmed) {
            CodexPromptChoice::No
        } else {
            CodexPromptChoice::Other
        });
    }
    choice
}

/// Pull the full visible Codex approval body from the latest approval prompt
/// in a captured pane tail. This intentionally excludes the yes/no choices,
/// leaving the auditor with the reason and command/edit description.
pub fn parse_codex_pending_full(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let (question_idx, question) = lines.iter().enumerate().rev().find_map(|(idx, line)| {
        let trimmed = trim_codex_prompt_line(line);
        is_codex_prompt_question(trimmed).then_some((idx, trimmed))
    })?;

    let mut collected = Vec::new();
    for line in &lines[question_idx + 1..] {
        let trimmed = trim_codex_prompt_line(line);
        if trimmed.is_empty() {
            continue;
        }
        if is_codex_choice(trimmed) || trimmed.starts_with("Press enter to confirm") {
            break;
        }
        collected.push(trimmed.to_string());
    }

    if collected.is_empty() && question.starts_with("Allow Codex to ") {
        collected.push(question.to_string());
    }

    (!collected.is_empty()).then(|| collected.join("\n"))
}

fn trim_codex_prompt_line(line: &str) -> &str {
    let trimmed = line
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '┆' | '┊'))
        .trim()
        .trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, '›' | '❯' | '>' | '-' | '•' | '●' | '○')
        })
        .trim();
    strip_numbered_choice_prefix(trimmed)
}

fn selected_codex_prompt_line(line: &str) -> Option<&str> {
    let trimmed = line
        .trim()
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '║' | '┆' | '┊'))
        .trim();
    let selected = trimmed
        .strip_prefix('›')
        .or_else(|| trimmed.strip_prefix('❯'))?;
    Some(strip_numbered_choice_prefix(selected.trim()))
}

fn strip_numbered_choice_prefix(line: &str) -> &str {
    let Some((prefix, rest)) = line.split_once(". ") else {
        return line;
    };
    if prefix.chars().all(|c| c.is_ascii_digit()) {
        rest.trim()
    } else {
        line
    }
}

fn is_codex_prompt_question(line: &str) -> bool {
    matches!(
        line,
        "Would you like to run the following command?"
            | "Would you like to grant these permissions?"
            | "Would you like to make the following edits?"
    ) || line.starts_with("Allow Codex to run `")
        || line.ends_with(" needs your approval.")
}

fn is_codex_yes_choice(line: &str) -> bool {
    line.starts_with("Yes, proceed")
        || line == "Yes, just this once"
        || line.starts_with("Yes, and ")
        || line.starts_with("Allow this request")
        || line.starts_with("Run the tool")
}

fn is_codex_no_choice(line: &str) -> bool {
    line.starts_with("No, ")
        || line == "Cancel this request"
        || line == "Decline this request and continue"
}

fn is_codex_choice(line: &str) -> bool {
    is_codex_yes_choice(line) || is_codex_no_choice(line)
}

/// Pull the human-readable preview of what Claude is asking from a captured
/// pane. Anchors on `1. Yes` (the option list, reliable across all prompt
/// variants) and walks upward, collecting every content line until we hit
/// the outer box separator (a long run of `─`). Inner separators (`╌`, used
/// inside Edit/Write diffs) are skipped, not used as boundaries. The chip
/// header line (e.g. "Bash command", "Edit file") is dropped since the tool
/// name is already in the row prefix.
///
/// We don't anchor on "Do you want to" — it varies by tool and version.
pub fn parse_pending_brief(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let opt_idx = lines.iter().rposition(|l| l.contains("1. Yes"))?;
    let mut collected: Vec<&str> = Vec::new();
    for line in lines[..opt_idx].iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Do you want") {
            continue;
        }
        if is_outer_separator(trimmed) {
            break;
        }
        if is_inner_separator(trimmed) {
            continue;
        }
        collected.push(trimmed);
        if collected.len() >= 20 {
            break;
        }
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    // Drop the chip header ("Bash command", "Edit file", "Web search", …) so
    // we don't duplicate the tool name. Single-word tools like `pwd` survive
    // this filter because they don't match the `<Tool> <category>` shape.
    if collected.first().is_some_and(|l| is_chip_header(l)) {
        collected.remove(0);
    }
    Some(collected.join(" "))
}

/// Like `parse_pending_brief` but unbounded: no 20-line cap, joins with
/// newlines instead of spaces. The autonomous-mode auditor needs the full
/// command (Bash heredocs span tens of lines, Edit diffs likewise) to make
/// a confident decision. Anchor and separator logic are identical to
/// `parse_pending_brief`.
pub fn parse_pending_full(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let opt_idx = lines.iter().rposition(|l| l.contains("1. Yes"))?;
    let mut collected: Vec<&str> = Vec::new();
    for line in lines[..opt_idx].iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Do you want") {
            continue;
        }
        if is_outer_separator(trimmed) {
            break;
        }
        if is_inner_separator(trimmed) {
            continue;
        }
        collected.push(trimmed);
    }
    if collected.is_empty() {
        return None;
    }
    collected.reverse();
    if collected.first().is_some_and(|l| is_chip_header(l)) {
        collected.remove(0);
    }
    Some(collected.join("\n"))
}

fn is_outer_separator(s: &str) -> bool {
    !s.is_empty() && s.chars().count() >= 20 && s.chars().all(|c| c == '─')
}

fn is_inner_separator(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| matches!(c, '╌' | '╴' | '╶'))
}

fn is_chip_header(s: &str) -> bool {
    let mut iter = s.split_whitespace();
    let Some(first) = iter.next() else {
        return false;
    };
    let Some(second) = iter.next() else {
        return false;
    };
    if iter.next().is_some() {
        return false;
    }
    first.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && matches!(
            second,
            "command" | "file" | "search" | "fetch" | "URL" | "request"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn new_window_command_explicitly_enters_cwd() {
        assert_eq!(
            command_in_cwd(Path::new("/tmp/my project"), "claude"),
            "cd '/tmp/my project' && claude"
        );
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(
            command_in_cwd(Path::new("/tmp/it's ok"), "codex"),
            "cd '/tmp/it'\\''s ok' && codex"
        );
    }

    fn pane(pid: u32, pane_id: &str, current_command: &str) -> Pane {
        Pane {
            target: format!("main:1.{pid}"),
            tmux_session: "main".to_string(),
            window_name: "triage".to_string(),
            pane_id: pane_id.to_string(),
            pid,
            tty: "/dev/ttys001".to_string(),
            current_command: current_command.to_string(),
            cwd: PathBuf::from("/tmp/project"),
            active: false,
        }
    }

    #[test]
    fn find_owning_pane_matches_direct_pane_pid() {
        let pane = Pane {
            target: "main:1.0".to_string(),
            tmux_session: "main".to_string(),
            window_name: "agent-ACDC-21".to_string(),
            pane_id: "%311".to_string(),
            pid: 98644,
            tty: "/dev/ttys001".to_string(),
            current_command: "2.1.158".to_string(),
            cwd: PathBuf::from("/tmp/project"),
            active: false,
        };
        let mut panes = HashMap::new();
        panes.insert(pane.pid, pane);

        let found = find_owning_pane(98644, &panes, &HashMap::new(), 8).unwrap();

        assert_eq!(found.pane_id, "%311");
    }

    fn alive_record(pid: u32, pane_id: Option<&str>) -> crate::approval::AliveRecord {
        crate::approval::AliveRecord {
            pid,
            pane_id: pane_id.map(str::to_string),
        }
    }

    #[test]
    fn locates_live_triage_by_pid_walk() {
        // triage (pid 11) is recorded; the pid walk finds the pane that hosts
        // it directly, independent of its recorded pane_id or current command.
        let mut panes = HashMap::new();
        panes.insert(10, pane(10, "%10", "fish"));
        panes.insert(11, pane(11, "%11", "triage"));
        let record = alive_record(11, Some("%11"));

        let located = locate_triage_from_record(&record, true, &panes, &HashMap::new());

        match located {
            Some(LocatedTriage::Live(p)) => assert_eq!(p.pane_id, "%11"),
            _ => panic!("expected live triage located by pid"),
        }
    }

    #[test]
    fn falls_back_to_recorded_pane_id_when_pid_walk_misses() {
        // pid 999 isn't a pane pid and the ppid map is empty, so the walk
        // misses; the recorded pane_id still resolves the live instance.
        let mut panes = HashMap::new();
        panes.insert(11, pane(11, "%11", "fish")); // command name is irrelevant now
        let record = alive_record(999, Some("%11"));

        let located = locate_triage_from_record(&record, true, &panes, &HashMap::new());

        match located {
            Some(LocatedTriage::Live(p)) => assert_eq!(p.pane_id, "%11"),
            _ => panic!("expected fallback to recorded pane_id"),
        }
    }

    #[test]
    fn dead_pid_with_surviving_pane_is_stale() {
        let mut panes = HashMap::new();
        panes.insert(11, pane(11, "%11", "fish"));
        let record = alive_record(11, Some("%11"));

        let located = locate_triage_from_record(&record, false, &panes, &HashMap::new());

        match located {
            Some(LocatedTriage::PaneStale(p)) => assert_eq!(p.pane_id, "%11"),
            _ => panic!("expected stale recorded pane for respawn"),
        }
    }

    #[test]
    fn dead_pid_with_no_pane_is_none() {
        let panes = HashMap::new();
        let record = alive_record(11, Some("%11"));

        assert!(locate_triage_from_record(&record, false, &panes, &HashMap::new()).is_none());
    }

    // TRI-123: a fresh `triage` launch must never attach to its own launching
    // pane. With pid+pane_id resolution this is structural, not a special case:
    // `.alive` only ever names an *already-running* instance, so the launching
    // pane (a different pid, a different pane than the recorded one) can't match
    // — even though it reports `triage` as its current command.
    #[test]
    fn does_not_attach_to_launching_pane() {
        let mut panes = HashMap::new();
        // %366 is the pane we're launching from: it shows `triage` (this very
        // process) but its pid (366) is not the recorded instance.
        panes.insert(366, pane(366, "%366", "triage"));
        // No recorded instance is alive and no recorded pane survives.
        let record = alive_record(999, Some("%999"));

        assert!(locate_triage_from_record(&record, false, &panes, &HashMap::new()).is_none());
        // Even if we (wrongly) believed it alive, the pid walk over an empty
        // ppid map and the missing pane_id still refuse to match %366.
        assert!(locate_triage_from_record(&record, true, &panes, &HashMap::new()).is_none());
    }

    #[test]
    fn draft_input_detected_when_composer_has_text() {
        // Real input is normal intensity (no faint span after the marker).
        let pane =
            "──────────────\n❯\u{a0}leave it to the coordinator\n──────────────\n  [Opus] ux";
        assert!(has_draft_input(pane));
    }

    #[test]
    fn faint_placeholder_is_not_draft() {
        // TRI-133 regression: Claude's ghost/placeholder hint is drawn faint
        // (SGR 2). Captured with -e it must NOT count as real unsent input.
        // Mirrors a real %38 capture: `\e[39m❯ \e[2m<hint>\e[0m`.
        let pane = "─────\n\u{1b}[39m❯\u{a0}\u{1b}[2mmonitor PR until CI green\u{1b}[0m\n─────";
        assert!(!has_draft_input(pane));
    }

    #[test]
    fn per_word_faint_placeholder_is_not_draft() {
        // %165-style: each word individually wrapped in its own faint span.
        let pane = "❯\u{a0}\u{1b}[2mmark\u{1b}[0m \u{1b}[2mit\u{1b}[0m \u{1b}[2mready\u{1b}[0m";
        assert!(!has_draft_input(pane));
    }

    #[test]
    fn cursor_on_faint_placeholder_is_not_draft() {
        // TRI-133 follow-up: on a placeholder the cursor sits on the first char
        // (reverse video, SGR 7) with the rest faint. Mirrors a real %39
        // capture: `❯ \e[7ms\e[0;2mtand down…\e[0m`. The lone reverse cursor
        // char must not count as real input.
        let pane = "❯\u{a0}\u{1b}[7ms\u{1b}[0;2mtand down until coordinator replies\u{1b}[0m";
        assert!(!has_draft_input(pane));
    }

    #[test]
    fn real_text_with_trailing_ghost_is_draft() {
        // Normal-intensity typed text followed by a faint autocomplete ghost
        // still counts as a real draft.
        let pane = "❯\u{a0}fix the \u{1b}[2mbug in the parser\u{1b}[0m";
        assert!(has_draft_input(pane));
    }

    #[test]
    fn empty_composer_is_not_draft() {
        let pane = "──────────────\n❯\u{a0}\n──────────────\n  [Opus] ux";
        assert!(!has_draft_input(pane));
    }

    #[test]
    fn draft_uses_bottom_most_composer_line() {
        // An earlier `❯`-prefixed line in scrollback must not shadow the live
        // (empty) composer at the bottom.
        let pane = "❯\u{a0}an old prompt from history\nassistant replied\n──────\n❯\u{a0}\n──────";
        assert!(!has_draft_input(pane));
    }

    #[test]
    fn no_composer_marker_is_not_draft() {
        assert!(!has_draft_input("just some output\nno prompt here"));
    }

    #[test]
    fn detects_codex_command_approval_prompt() {
        let pane = r#"
╭────────────────────────────────────────────╮
│ Would you like to run the following command? │
│ cargo install --path .                       │
│ › Yes, proceed                               │
│ No, and tell Codex what to do differently    │
╰────────────────────────────────────────────╯
"#;
        assert!(has_codex_permission_prompt(pane));
    }

    #[test]
    fn detects_real_codex_numbered_command_prompt() {
        let pane = r#"
  Would you like to run the following command?

  Reason: Allow Snowflake CLI to run a small context probe so I can identify why the tracker_v3 query is compiling as object-not-found?

  $ snow sql --format CSV --silent -f ios-envelope-401/sql/current_snowflake_context_probe.sql > ios-envelope-401/data/current_snowflake_context_probe.csv 2> ios-envelope-401/data/current_snowflake_context_probe.err

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for commands that start with `snow sql --format CSV --silent -f ios-envelope-401/sql/current_snowflake_context_probe.sql > ios-envelope-401/data/current_snowflake_context_probe.csv 2> ios-envelope-401/data/current_snowflake_context_probe.err` (p)
  3. No, and tell Codex what to do differently (esc)

  Press enter to confirm or esc to cancel
"#;
        assert!(has_codex_permission_prompt(pane));
        assert_eq!(
            codex_selected_permission_choice(pane),
            Some(CodexPromptChoice::Yes)
        );
        assert_eq!(
            parse_codex_pending_full(pane).as_deref(),
            Some(
                "Reason: Allow Snowflake CLI to run a small context probe so I can identify why the tracker_v3 query is compiling as object-not-found?\n$ snow sql --format CSV --silent -f ios-envelope-401/sql/current_snowflake_context_probe.sql > ios-envelope-401/data/current_snowflake_context_probe.csv 2> ios-envelope-401/data/current_snowflake_context_probe.err"
            )
        );
    }

    #[test]
    fn ignores_codex_prompt_text_without_choices() {
        let pane = r#"
let s = "Would you like to run the following command?";
println!("Yes, proceed");
"#;
        assert!(!has_codex_permission_prompt(pane));
    }

    #[test]
    fn detects_codex_selected_no_choice() {
        let pane = r#"
Would you like to run the following command?
  1. Yes, proceed (y)
› 2. No, and tell Codex what to do differently (esc)
"#;
        assert_eq!(
            codex_selected_permission_choice(pane),
            Some(CodexPromptChoice::No)
        );
    }

    #[test]
    fn codex_selected_choice_prefers_latest_prompt_in_tail() {
        let pane = r#"
Would you like to run the following command?
› 1. Yes, proceed (y)
  2. No, and tell Codex what to do differently (esc)

Would you like to run the following command?
  1. Yes, proceed (y)
› 2. No, and tell Codex what to do differently (esc)
"#;
        assert_eq!(
            codex_selected_permission_choice(pane),
            Some(CodexPromptChoice::No)
        );
    }

    #[test]
    fn parse_codex_pending_full_prefers_latest_prompt() {
        let pane = r#"
Would you like to run the following command?
$ old command
› 1. Yes, proceed (y)
  2. No, and tell Codex what to do differently (esc)

Would you like to run the following command?
Reason: newer request
$ date
› 1. Yes, proceed (y)
  2. No, and tell Codex what to do differently (esc)
"#;
        assert_eq!(
            parse_codex_pending_full(pane).as_deref(),
            Some("Reason: newer request\n$ date")
        );
    }
}
