use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use crate::models::{AttentionState, Session};

/// Fire a macOS desktop notification for a session that just became actionable
/// (Blocked or Error). Best-effort — failures are silently ignored, since
/// running inside a TUI we can't surface them anywhere useful.
///
/// Prefers `terminal-notifier` (Homebrew) when installed because it lets the
/// notification carry a click action that jumps tmux focus to the blocked
/// pane, and it isn't attributed to the Script Editor app the way `osascript
/// display notification` is. Falls back to `osascript` otherwise — the
/// notification still fires, but clicking it goes nowhere useful.
pub fn alert(session: &Session) {
    let title = match session.state {
        AttentionState::Blocked => "needs your input",
        AttentionState::Error => "error",
        _ => return,
    };
    let label = session
        .name
        .clone()
        .or_else(|| {
            session
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "session".to_string());
    let preview = session
        .headline
        .as_deref()
        .or(session.last_prompt.as_deref())
        .map(|s| s.replace('\n', " "))
        .map(|s| s.chars().take(140).collect::<String>())
        .unwrap_or_default();

    if let Some(notifier) = terminal_notifier_path() {
        send_via_terminal_notifier(notifier, title, &label, &preview, session.pane.as_ref());
        return;
    }
    send_via_osascript(title, &label, &preview);
}

fn send_via_terminal_notifier(
    notifier: &str,
    title: &str,
    label: &str,
    preview: &str,
    pane: Option<&crate::models::Pane>,
) {
    let mut cmd = Command::new(notifier);
    cmd.args(["-title", "triage"]);
    cmd.args(["-subtitle", &format!("{label} — {title}")]);
    cmd.args(["-message", preview]);
    cmd.args(["-group", "triage"]); // collapse repeats
    // Sender attribution is OPT-IN only: macOS silently drops (or hangs)
    // notifications when the sender app doesn't have notification permission,
    // and kitty/Ghostty/etc. usually don't have it set up. Default behavior
    // is to let the notification appear under terminal-notifier's own icon.
    // Set `TRIAGE_TERMINAL_BUNDLE=net.kovidgoyal.kitty` (or similar) after
    // granting that app notification permission to opt in.
    if let Some(bundle) = forced_terminal_bundle_id() {
        cmd.args(["-sender", bundle]);
    }

    // Click action: bring the user's terminal app to the foreground, then
    // switch the tmux client to the blocked pane. Requires the Pane (for the
    // target) and tmux on PATH (for the command). Activating the terminal
    // first matters when the user is in a different macOS app — without it,
    // tmux's internal focus changes but the terminal window stays hidden.
    if let (Some(pane), Some(tmux)) = (pane, tmux_path()) {
        let session_name = pane.tmux_session.as_str();
        let activate_cmd = detected_terminal_bundle()
            .map(|bundle| {
                format!(
                    "/usr/bin/osascript -e 'tell application id {0} to activate' && ",
                    shell_quote(bundle)
                )
            })
            .unwrap_or_default();
        let exec = format!(
            "{activate}{tmux} switch-client -t {session} && {tmux} select-pane -t {target}",
            activate = activate_cmd,
            tmux = shell_quote(tmux),
            session = shell_quote(session_name),
            target = shell_quote(&pane.target),
        );
        cmd.args(["-execute", &exec]);
    }
    spawn_detached(cmd);
}

fn send_via_osascript(title: &str, label: &str, preview: &str) {
    let body = if preview.is_empty() {
        label.to_string()
    } else {
        format!("{label} — {preview}")
    };
    let script = format!(
        "display notification {body} with title {title}",
        body = applescript_string(&body),
        title = applescript_string(&format!("triage — {title}")),
    );
    let mut cmd = Command::new("osascript");
    cmd.args(["-e", &script]);
    spawn_detached(cmd);
}

/// Fire-and-forget subprocess. We don't care about the output and we MUST NOT
/// block the UI thread waiting for it — that's what froze triage when several
/// notifications had to be sent on the same refresh tick. The child is
/// reaped by a short-lived helper thread so it doesn't linger as a zombie.
fn spawn_detached(mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    thread::spawn(move || {
        if let Ok(mut child) = cmd.spawn() {
            let _ = child.wait();
        }
    });
}

/// User-forced terminal bundle ID via `TRIAGE_TERMINAL_BUNDLE`. Used as the
/// `-sender` for terminal-notifier. Auto-detection isn't safe here: macOS
/// silently drops notifications when the sender app lacks permission AND
/// (newer macOS) terminal-notifier's sender-spoofing is unreliable in
/// general. Force it by env var only.
fn forced_terminal_bundle_id() -> Option<&'static str> {
    static CACHED: OnceLock<Option<&'static str>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let forced = std::env::var("TRIAGE_TERMINAL_BUNDLE").ok()?;
        let trimmed = forced.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Leak the override so we can hand out &'static str. Once-per-process,
        // negligible memory cost.
        Some(Box::leak(trimmed.to_string().into_boxed_str()) as &'static str)
    })
}

/// Detect which terminal app triage is running under, for the click-action
/// activate path only. Unlike `-sender`, activating an app on click works
/// reliably on modern macOS — it just brings that bundle to the foreground.
/// Checks `TRIAGE_TERMINAL_BUNDLE` first, then env vars, then walks the
/// process tree (handles tmux-inside-terminal, since tmux strips most env).
fn detected_terminal_bundle() -> Option<&'static str> {
    static CACHED: OnceLock<Option<&'static str>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Some(forced) = forced_terminal_bundle_id() {
            return Some(forced);
        }
        if let Some(b) = bundle_from_env() {
            return Some(b);
        }
        bundle_from_proc_tree()
    })
}

fn bundle_from_env() -> Option<&'static str> {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return Some("net.kovidgoyal.kitty");
    }
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return Some("com.mitchellh.ghostty");
    }
    if std::env::var_os("WEZTERM_PANE").is_some() {
        return Some("com.github.wez.wezterm");
    }
    if std::env::var_os("ALACRITTY_LOG").is_some() {
        return Some("org.alacritty");
    }
    match std::env::var("TERM_PROGRAM").ok().as_deref() {
        Some("iTerm.app") => Some("com.googlecode.iterm2"),
        Some("Apple_Terminal") => Some("com.apple.Terminal"),
        Some("WezTerm") => Some("com.github.wez.wezterm"),
        Some("ghostty") => Some("com.mitchellh.ghostty"),
        _ => None,
    }
}

fn bundle_from_proc_tree() -> Option<&'static str> {
    let mut pid = std::process::id();
    for _ in 0..16 {
        let ppid = parent_pid(pid)?;
        if ppid <= 1 {
            break;
        }
        let cmd = command_of(ppid).unwrap_or_default();
        let lower = cmd.to_lowercase();
        if lower.contains("kitty") {
            return Some("net.kovidgoyal.kitty");
        }
        if lower.contains("ghostty") {
            return Some("com.mitchellh.ghostty");
        }
        if lower.contains("wezterm") {
            return Some("com.github.wez.wezterm");
        }
        if lower.contains("alacritty") {
            return Some("org.alacritty");
        }
        if lower.contains("iterm") {
            return Some("com.googlecode.iterm2");
        }
        if lower.ends_with("/terminal") || lower.contains("/terminal.app/") {
            return Some("com.apple.Terminal");
        }
        pid = ppid;
    }
    None
}

fn parent_pid(pid: u32) -> Option<u32> {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8(out.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn command_of(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn terminal_notifier_path() -> Option<&'static str> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| which("terminal-notifier"))
        .as_deref()
}

fn tmux_path() -> Option<&'static str> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED.get_or_init(|| which("tmux")).as_deref()
}

fn which(cmd: &str) -> Option<String> {
    let out = Command::new("which").arg(cmd).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!path.is_empty()).then_some(path)
}

/// Single-quote-wrap a value for `sh -c` style execution. Replaces any inner
/// single quote with the standard `'\''` dance.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote a string for embedding inside an AppleScript snippet. AppleScript
/// strings use double quotes, with `\\` and `\"` as the only escapes we care
/// about; tabs/newlines are uncommon in our content but handled defensively.
fn applescript_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
