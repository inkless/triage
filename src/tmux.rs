use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::models::Pane;

/// Returns a map of pane_pid → Pane.
pub fn list_panes() -> HashMap<u32, Pane> {
    let mut map = HashMap::new();
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{window_index}.#{pane_index}|#{pane_pid}|#{pane_tty}|#{pane_current_command}|#{pane_current_path}|#{?pane_active,1,0}|#{pane_id}|#{window_name}",
        ])
        .output();
    let Ok(out) = out else { return map };
    if !out.status.success() {
        return map;
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
    map
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
    for _ in 0..max_hops {
        let ppid = *ppid_map.get(&cur)?;
        if ppid <= 1 {
            return None;
        }
        if let Some(pane) = pane_pids.get(&ppid) {
            return Some(pane.clone());
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
    let alive = unsafe { libc::kill(record.pid as i32, 0) } == 0;

    if let Some(pane_id) = &record.pane_id
        && let Some(pane) = panes.values().find(|p| &p.pane_id == pane_id).cloned()
    {
        return Some(if alive {
            LocatedTriage::Live(pane)
        } else {
            LocatedTriage::PaneStale(pane)
        });
    }

    // No recorded pane id (legacy `.alive`, or non-tmux launch), or the
    // pane is gone. If the process is alive, fall back to ppid walk to
    // locate its pane via the process tree.
    if alive {
        let ppid_map = build_ppid_map();
        if let Some(pane) = find_owning_pane(record.pid, panes, &ppid_map, 8) {
            return Some(LocatedTriage::Live(pane));
        }
    }
    None
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
