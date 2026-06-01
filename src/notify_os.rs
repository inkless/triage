use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;

use crate::config::{Config, NtfyConfig};
use crate::models::{AttentionState, Provider, Session};

/// Fire a macOS desktop notification for a session that just became actionable
/// (Blocked or Error). Best-effort — failures are silently ignored, since
/// running inside a TUI we can't surface them anywhere useful.
///
/// Prefers our own `triage-notify` Swift helper (under
/// `scripts/triage-notify/triage-notify.app/`) when available — it uses the
/// modern UNUserNotificationCenter API so click actions actually fire on
/// macOS 14+. Falls back to `osascript display notification` (display-only,
/// no click action). `terminal-notifier` was tried earlier but its
/// `-execute` callback was silently broken on recent macOS.
pub fn alert(session: &Session, cfg: &Config, phone_push: bool) {
    let title = match session.state {
        AttentionState::Blocked => "needs your input",
        AttentionState::Error => "error",
        _ => return,
    };
    let label = session_label(session);
    let preview = session_preview(session);

    // Phone push (ntfy). Body deliberately minimal — `<label> · <state>` —
    // so the publish target (whoever can read the topic) doesn't see prompt
    // contents. See specs/notify-self-host.md. Suppressed when `phone_push`
    // is false — caller (refresh) sets this to defer Blocked transitions
    // through the auditor when auto-mode is on; phone fires later only on
    // a `WAIT` verdict via `push_to_phone`.
    if phone_push && let Some(ntfy) = cfg.ntfy.as_ref() {
        ntfy_push(ntfy, &label, title);
    }

    if let Some(notifier) = triage_notify_path() {
        send_via_triage_notify(
            notifier,
            title,
            &label,
            &preview,
            session.pane.as_ref(),
            cfg,
        );
        return;
    }
    send_via_osascript(title, &label, &preview);
}

/// User-armed "session finished" alert (T-81). Fired when a watched session
/// transitions into `JustFinished`. Mirrors `alert` shape: Mac local banner
/// via the triage-notify.app helper (with click-to-jump to the session's
/// pane) + optional ntfy push gated on the caller-supplied `phone_push`.
///
/// Title is `finished` rather than `needs your input` / `error` so the
/// banner is visually distinct from the auto-fire actionable-state alerts.
pub fn notify_session_done(session: &Session, cfg: &Config, phone_push: bool) {
    let title = "finished";
    let label = session_label(session);
    let preview = session_preview(session);

    if phone_push && let Some(ntfy) = cfg.ntfy.as_ref() {
        ntfy_push(ntfy, &label, title);
    }

    if let Some(notifier) = triage_notify_path() {
        send_via_triage_notify(
            notifier,
            title,
            &label,
            &preview,
            session.pane.as_ref(),
            cfg,
        );
        return;
    }
    send_via_osascript(title, &label, &preview);
}

/// Phone-only push. Used by the auto-mode WAIT path: triage deferred the
/// phone push when the session went Blocked under auto-mode (the auditor
/// might've handled it silently); now that the verdict is WAIT, surface the
/// session to the phone. Desktop notification has already fired from the
/// original `alert()` call.
pub fn push_to_phone(session: &Session, cfg: &Config) {
    let title = match session.state {
        AttentionState::Blocked => "needs your input",
        AttentionState::Error => "error",
        _ => return,
    };
    let label = session_label(session);
    if let Some(ntfy) = cfg.ntfy.as_ref() {
        ntfy_push(ntfy, &label, title);
    }
}

fn session_label(session: &Session) -> String {
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
    match session.provider {
        Provider::Claude => label,
        Provider::Codex => format!("cx {label}"),
    }
}

fn session_preview(session: &Session) -> String {
    session
        .headline
        .as_deref()
        .or(session.last_prompt.as_deref())
        .map(|s| s.replace('\n', " "))
        .map(|s| s.chars().take(140).collect::<String>())
        .unwrap_or_default()
}

/// One-shot CLI entry: `triage notify [--title T] [--tags T] <message...>`.
///
/// Unlike the in-TUI `ntfy_push` which fire-and-forgets, this blocks on curl
/// so the calling process (agent, hook, script) gets a real exit status. The
/// 5-second timeout still bounds it.
///
/// Errors are reported to stderr; the function returns a non-zero `io::Error`
/// on any failure (missing config, empty message, curl non-zero, curl missing).
const NOTIFY_USAGE: &str = "usage: triage notify [--title T] [--tags T] <message...>";

pub fn cli_notify(args: &[String]) -> io::Result<()> {
    let mut title: Option<String> = None;
    let mut tags: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!("{NOTIFY_USAGE}");
                return Ok(());
            }
            "--title" => {
                title = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| io::Error::other("--title needs a value"))?,
                );
                i += 2;
            }
            "--tags" => {
                tags = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or_else(|| io::Error::other("--tags needs a value"))?,
                );
                i += 2;
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }

    // Stdin convention: a single positional `-` reads the message from stdin.
    // Multiline output is preserved (we only trim trailing newline so
    // `echo` / heredocs feel natural).
    let message = if positional.len() == 1 && positional[0] == "-" {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        buf.trim_end_matches('\n').to_string()
    } else {
        positional.join(" ")
    };

    if message.is_empty() {
        return Err(io::Error::other(NOTIFY_USAGE));
    }

    let cfg = Config::load();
    let ntfy = cfg.ntfy.as_ref().ok_or_else(|| {
        io::Error::other(
            "ntfy not configured. Add an [ntfy] block with `url=` (and optional \
             `user=`/`token=`) to ~/.config/triage/config.toml.",
        )
    })?;

    let title = title.as_deref().unwrap_or("triage agent");
    let tags = tags.as_deref().unwrap_or("information");

    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "-m", "5", "-X", "POST"]);
    if let (Some(user), Some(token)) = (ntfy.user.as_deref(), ntfy.token.as_deref()) {
        cmd.arg("-u").arg(format!("{user}:{token}"));
    }
    cmd.arg("-H").arg(format!("Title: {title}"));
    cmd.arg("-H").arg(format!("Tags: {tags}"));
    cmd.args(["-d", &message]);
    cmd.arg(&ntfy.url);

    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| io::Error::other(format!("failed to invoke curl: {e}")))?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "curl exited {} posting to {}",
            status.code().unwrap_or(-1),
            ntfy.url,
        )));
    }
    Ok(())
}

fn ntfy_push(ntfy: &NtfyConfig, label: &str, state: &str) {
    let body = format!("{label} · {state}");
    let mut cmd = Command::new("curl");
    // -fsSL: fail on HTTP errors silently (no stderr unless -v), follow
    //   redirects, no progress bar.
    // -m 5: total time budget. We do not block on this — spawn_detached
    //   reaps the child — but bounding the request keeps zombie-thread
    //   accumulation minimal if ntfy's edge ever stalls.
    cmd.args(["-fsSL", "-m", "5", "-X", "POST"]);
    if let (Some(user), Some(token)) = (ntfy.user.as_deref(), ntfy.token.as_deref()) {
        cmd.arg("-u").arg(format!("{user}:{token}"));
    }
    cmd.args(["-H", "Title: triage"]);
    cmd.args(["-H", "Tags: warning"]);
    cmd.args(["-d", &body]);
    cmd.arg(&ntfy.url);
    spawn_detached(cmd);
}

fn send_via_triage_notify(
    bundle_path: &str,
    title: &str,
    label: &str,
    preview: &str,
    pane: Option<&crate::models::Pane>,
    cfg: &Config,
) {
    // Launch via LaunchServices (`open -na <bundle> --args …`) instead of
    // exec'ing the binary directly. Without this, macOS 14+ never registers
    // the bundle with the notification system and `requestAuthorization`
    // fails silently with "Notifications are not allowed for this
    // application." `-n` forces a new instance per notification (so two
    // simultaneous Blocked transitions don't collide); `-a` passes the
    // .app path; `--args` forwards everything after to the helper.
    let mut cmd = Command::new("open");
    cmd.args(["-na", bundle_path]);
    cmd.arg("--args");
    cmd.args(["--title", "triage"]);
    cmd.args(["--subtitle", &format!("{label} — {title}")]);
    cmd.args(["--message", preview]);

    // Click action: activate the user's terminal app, then `tmux
    // switch-client + select-pane` to the blocked pane. Activation matters
    // when the user is in a different macOS app — without it, tmux's
    // internal focus changes but the terminal window stays hidden.
    if let (Some(pane), Some(tmux)) = (pane, tmux_path()) {
        let session_name = pane.tmux_session.as_str();
        let activate_cmd = detected_terminal_bundle(cfg)
            .map(|bundle| {
                // Use LaunchServices (`open -b <bundle>`) rather than
                // `osascript -e 'tell application id ... to activate'`.
                // AppleScript activate is unreliable for terminals without a
                // scripting suite (kitty, ghostty): osascript exits 0 but
                // the app does not come forward, and on some macOS configs
                // Script Editor pops as a fallback handler. `open -b` goes
                // through LaunchServices directly and reliably foregrounds
                // the bundle.
                format!("/usr/bin/open -b {} && ", shell_quote(bundle))
            })
            .unwrap_or_default();
        // `unset TMUX`: macOS launches the response-mode helper outside any
        // tmux client, and the inherited env can carry a broken `TMUX` value
        // (observed: `/opt/homebrew/bin/tmux` — the binary path, not a
        // socket path). When set, tmux honors it as the socket and fails
        // with "Socket operation on non-socket". Clearing it makes tmux
        // fall back to its default `/tmp/tmux-$UID/default` socket, which
        // is what the user's running server listens on.
        //
        // `switch-client` with no `-c` targets "the most recently used
        // client" — which is what we want, since `open -b` brings the
        // user's terminal-and-its-tmux-client to the foreground.
        let action = format!(
            "unset TMUX; {activate}{tmux} switch-client -t {session} && {tmux} select-pane -t {target}",
            activate = activate_cmd,
            tmux = shell_quote(tmux),
            session = shell_quote(session_name),
            target = shell_quote(&pane.target),
        );
        cmd.args(["--action", &action]);
    }
    // Short helper lifetime to limit pile-up. Multiple concurrent helpers
    // can trigger macOS's "<app> is not responding" dialog because the
    // CommandLine .app has no NSApplication / AppleEvent handler. 20s is
    // a tradeoff: clicks beyond that window won't fire (rare in practice
    // — banners get clicked within seconds), but stacked notifications
    // clear quickly. The proper fix is an NSApplication-based daemon
    // (tracked separately).
    cmd.args(["--timeout", "20"]);
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

/// Detect which terminal app triage is running under, for the click-action
/// activate path only. Checks the config-provided override (which itself
/// folds in the legacy `TRIAGE_TERMINAL_BUNDLE` env var), then env-based
/// per-terminal sentinels, then walks the process tree (handles tmux-
/// inside-terminal, since tmux strips most env).
fn detected_terminal_bundle(cfg: &Config) -> Option<&'static str> {
    static CACHED: OnceLock<Option<&'static str>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Some(forced) = forced_terminal_bundle_id(cfg) {
            return Some(forced);
        }
        if let Some(b) = bundle_from_env() {
            return Some(b);
        }
        bundle_from_proc_tree()
    })
}

fn forced_terminal_bundle_id(cfg: &Config) -> Option<&'static str> {
    let forced = cfg.notifications.terminal_bundle.as_deref()?;
    let trimmed = forced.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Leak the override so we can hand out &'static str. Once-per-process,
    // negligible memory cost.
    Some(Box::leak(trimmed.to_string().into_boxed_str()) as &'static str)
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

/// Locate the `triage-notify.app` bundle. Returns the `.app` directory
/// (not the binary inside) — we hand this to `open -na` for proper
/// LaunchServices registration.
///
/// Searches `~/.config/triage/` first (where `build.rs` stages a copy on
/// every `cargo build` / `cargo install` — this is the path the
/// cargo-installed binary at `~/.cargo/bin/triage` relies on), then a few
/// fallbacks relative to the running binary so dev / manual installs
/// continue to work.
fn triage_notify_path() -> Option<&'static str> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            // User XDG-style location populated by build.rs. First priority
            // because it's stable across binary location (cargo-install vs
            // workspace) and tracks the most recently built helper.
            if let Some(home) = std::env::var_os("HOME") {
                candidates
                    .push(std::path::PathBuf::from(home).join(".config/triage/triage-notify.app"));
            }
            let exe = std::env::current_exe().ok()?;
            let exe_dir = exe.parent()?;
            // Workspace layout: target/release/triage → ../../scripts/...
            candidates.push(exe_dir.join("../../scripts/triage-notify/triage-notify.app"));
            // Sibling layout: <prefix>/bin/triage → <prefix>/scripts/...
            candidates.push(exe_dir.join("../scripts/triage-notify/triage-notify.app"));
            // Same-dir layout
            candidates.push(exe_dir.join("triage-notify.app"));
            for c in &candidates {
                if let Ok(p) = c.canonicalize()
                    && p.is_dir()
                {
                    return Some(p.display().to_string());
                }
            }
            None
        })
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
