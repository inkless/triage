mod approval;
mod auditor;
mod classifier;
mod config;
mod cost_rollup;
mod discovery;
mod models;
mod notify_os;
mod persist;
mod transcript;
mod tmux;
mod ui;
mod watcher;

use std::io;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::ui::{AppState, draw};
use crate::watcher::FsWatcher;

const TICK_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum gap between watcher-triggered refreshes. Without this, an actively
/// writing jsonl can fire fs events fast enough that refresh runs every loop
/// iteration, blocking key handling. The configured refresh-interval still
/// applies as the upper bound when nothing is changing.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(400);

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Top-level help. Must come before any other dispatch so an agent or
    // human running `triage --help` doesn't accidentally launch the TUI.
    if args
        .iter()
        .skip(1)
        .any(|a| a == "--help" || a == "-h" || a == "help")
    {
        print_top_level_help();
        return Ok(());
    }
    // One-shot subcommand: `triage notify [flags] <message...>` posts to
    // ntfy using the config's [ntfy] block. Detected by positional argv[1]
    // (not a flag) so it doesn't collide with `--notify` style flags
    // elsewhere. Blocking call; exit status reflects curl outcome.
    if args.get(1).map(String::as_str) == Some("notify") {
        return notify_os::cli_notify(&args[2..]);
    }
    // `triage cost [flags]` — daily/weekly Claude spend across all
    // sessions on disk. One-shot scan; no persistence. See cost_rollup.rs.
    if args.get(1).map(String::as_str) == Some("cost") {
        return cost_rollup::cli_cost(&args[2..]);
    }
    if args.iter().any(|a| a == "--probe") {
        return probe();
    }
    if args.iter().any(|a| a == "--install-hooks-hint") {
        approval::print_install_hint();
        return Ok(());
    }
    if args.iter().any(|a| a == "--install-hooks") {
        let dry = args.iter().any(|a| a == "--dry-run");
        approval::install_hooks(dry)?;
        return Ok(());
    }
    if args.iter().any(|a| a == "--uninstall-hooks") {
        let dry = args.iter().any(|a| a == "--dry-run");
        approval::uninstall_hooks(dry)?;
        return Ok(());
    }
    // T-56 spike: feed the selected session's pending tool_use to a
    // separate `claude -p` and print the auditor's verdict. No actual
    // approve/deny — just exercising the prompt + parse path.
    if let Some(idx) = args.iter().position(|a| a == "--audit") {
        let pid = args
            .get(idx + 1)
            .and_then(|s| s.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("usage: triage --audit <pid>"))?;
        return auditor::audit(pid);
    }
    if args.iter().any(|a| a == "--audit-prompt") {
        auditor::print_prompt();
        return Ok(());
    }
    // Tmux-binding entrypoint: focus an existing triage pane or spawn one.
    // Skips all discovery / transcript / watcher init so the focus switch
    // is essentially just tmux subprocess overhead (<30ms cold).
    // `--zoom` additionally `resize-pane -Z`s the triage pane so it fills
    // the screen — designed for the mobile binding (M-/) where pane
    // multitasking on a phone is unworkable.
    if args.iter().any(|a| a == "--jump-to-self") {
        let zoom = args.iter().any(|a| a == "--zoom");
        return tmux::jump_to_self(zoom);
    }

    let exit_on_jump = args.iter().any(|a| a == "--exit-on-jump");
    // Per-launch only — not persisted. Mobile users either launch their
    // long-lived triage with this flag (and accept that desktop also
    // zooms on Enter), or rely on the auto-detect path inside Enter.
    let zoom_on_jump = exit_on_jump || args.iter().any(|a| a == "--zoom-on-jump");
    let force_new = args.iter().any(|a| a == "--force-new");

    // Silent attach: typing `triage` (no special flags) when one's already
    // running just switches the user to it and exits 0. Skipped when:
    //   - --force-new (explicit "I want a second instance, e.g. for debug")
    //   - --exit-on-jump (popup-launch context — caller is already
    //     handling lifecycle)
    //   - we're not inside tmux (no pane to switch to)
    if !force_new
        && !exit_on_jump
        && std::env::var_os("TMUX").is_some()
        && tmux::attach_if_alive(zoom_on_jump).unwrap_or(false)
    {
        return Ok(());
    }

    // Aliveness guard sticks around for the whole interactive session. The
    // hook checks for ~/.claude/triage/.alive; without this it bails out and
    // Claude's normal permission prompt takes over.
    let _alive = approval::AliveGuard::install();

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, exit_on_jump, zoom_on_jump);
    restore_terminal()?;
    result
}

fn print_top_level_help() {
    println!(
        r#"triage — TUI to monitor parallel Claude Code sessions across tmux panes

USAGE:
  triage                    launch the TUI (or silent-attach to a running
                            instance in the current tmux session)
  triage <subcommand>       one-shot subcommand
  triage <flag>             one-shot flag operation

SUBCOMMANDS:
  notify <msg> [...]        post a one-shot ntfy push using ~/.config/triage/config.toml
  cost [--by day|cwd|session|model] [--days N] [--top N] [--json]
                            daily/weekly Claude spend across every transcript

FLAGS:
  --help, -h, help          print this message and exit
  --probe                   print joined session table (non-TUI smoke test)
  --install-hooks           install triage's PreToolUse hook into ~/.claude/settings.json
  --uninstall-hooks         remove triage's PreToolUse hook entries
  --install-hooks-hint      print manual install instructions
  --dry-run                 (with install/uninstall-hooks) show changes without writing
  --audit <pid>             feed the session's pending tool_use to the auditor
  --audit-prompt            print the auditor prompt to stdout
  --jump-to-self [--zoom]   focus an existing triage pane (tmux-binding entrypoint)
  --exit-on-jump            exit cleanly after a successful Enter jump (popup-launch mode)
  --zoom-on-jump            force zoom-on-jump regardless of pane width
  --force-new               spawn a new TUI instance even if one is already alive

IN-TUI KEYBINDINGS:
  ⏎ jump · a/d approve/deny · h toggle approval mode · A toggle auto mode
  p toggle phone push · m mute · w watch · / filter (name + cwd)
  H audit log · $ cost overlay · q quit

DOCS:
  README:        https://github.com/inkless/triage
  config file:   ~/.config/triage/config.toml (optional)
"#
    );
}

fn probe() -> io::Result<()> {
    let now = SystemTime::now();
    let mut sessions = discovery::discover_live_sessions();
    let panes = tmux::list_panes();
    println!("# discovered {} live sessions, {} tmux panes\n", sessions.len(), panes.len());
    // Resolve panes before pairing so assign_transcripts can see which session
    // is in the currently-focused tmux pane.
    let ppid_map = tmux::build_ppid_map();
    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, &ppid_map, 8);
    }
    let mut cache = transcript::DigestCache::new();
    transcript::assign_transcripts(&mut sessions, &mut cache);
    for s in &mut sessions {
        transcript::enrich(s, now, &mut cache);
    }
    for s in &mut sessions {
        if s.status == "waiting"
            && s.last_tool_use.is_none()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane(&pane.target)
            && let Some(brief) = tmux::parse_pending_brief(&content)
        {
            let name = s
                .waiting_for
                .as_deref()
                .and_then(|w| w.strip_prefix("approve "))
                .unwrap_or("?")
                .to_string();
            s.last_tool_use = Some((name, brief));
        }
    }
    // Deterministic Blocked scan: for any busy session whose cheaper
    // signals haven't already flagged it, capture the pane tail and look
    // for the `❯ 1. Yes` + `Esc to cancel` anchor that only the live
    // permission UI prints. Catches the cc-gh-warn case where Claude is on
    // its native permission prompt and sessions JSON never wrote
    // status=waiting. We don't gate on `last_tool_use` or transcript age:
    // a pending tool_use is exactly what a permission prompt looks like at
    // the transcript level, so excluding those would miss the case. The
    // strict anchor keeps the false-positive risk negligible.
    for s in &mut sessions {
        if s.status == "busy"
            && s.pending_approvals.is_empty()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane_tail(&pane.target, 15)
            && tmux::has_pending_permission_prompt(&content)
        {
            s.pane_blocked = true;
        }
    }
    for s in &mut sessions {
        s.state = classifier::classify(s, now);
        let pane = s.pane.as_ref().map(|p| p.target.as_str()).unwrap_or("(none)");
        let headline = s
            .headline
            .as_deref()
            .or(s.last_prompt.as_deref())
            .unwrap_or("(no transcript)")
            .replace('\n', " ");
        let head_short: String = headline.chars().take(80).collect();
        println!(
            "  pid={:<6} state={:<6} status={:<5} pane={:<24} cwd={}",
            s.pid,
            s.state.label(),
            s.status,
            pane,
            s.cwd.display()
        );
        println!("    headline: {head_short}");
        if let Some((n, b)) = &s.last_tool_use {
            let b_short: String = b.chars().take(120).collect();
            println!("    pending:  {n} — {b_short}");
        }
    }
    Ok(())
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    exit_on_jump: bool,
    zoom_on_jump: bool,
) -> io::Result<()> {
    let mut app = AppState::new();
    app.config = config::Config::load();
    app.exit_on_jump = exit_on_jump;
    app.zoom_on_jump = zoom_on_jump;
    let refresh_interval = Duration::from_secs(app.config.thresholds.refresh_seconds.max(1));
    let watcher = FsWatcher::spawn().ok();

    refresh(&mut app);
    let mut last_refresh = Instant::now();

    loop {
        let now = SystemTime::now();
        terminal.draw(|f| draw(f, &mut app, now))?;

        if event::poll(TICK_INTERVAL)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if !handle_key(&mut app, k.code, k.modifiers) {
                        return Ok(());
                    }
                    app.clamp_selection();
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        let elapsed = last_refresh.elapsed();
        let due = elapsed >= refresh_interval;
        let triggered = watcher.as_ref().map(|w| w.drain()).unwrap_or(false);
        if due || (triggered && elapsed >= WATCHER_DEBOUNCE) {
            refresh(&mut app);
            last_refresh = Instant::now();
        }
    }
}

fn handle_key(app: &mut AppState, code: KeyCode, mods: KeyModifiers) -> bool {
    // Filter edit mode. Printable keys go into the filter string; the rest
    // are wired to either common readline editing (Ctrl+W / Ctrl+U / Ctrl+H)
    // or table navigation (arrows / PgUp / PgDn) so the user can scroll the
    // filtered rows without first exiting edit mode. Enter keeps the filter
    // applied; Esc exits AND clears.
    if app.filter_active {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc => {
                app.filter.clear();
                app.filter_active = false;
            }
            KeyCode::Enter => {
                // Exit edit AND jump in one keystroke. Avoids the awkward
                // "press Enter to exit edit, press Enter again to jump"
                // dance — less/vim search confirm-and-go behavior.
                app.filter_active = false;
                return jump_to_selected(app);
            }
            // Readline-style line edits.
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                delete_prev_word(&mut app.filter);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                app.filter.clear();
            }
            KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
                app.filter.pop();
            }
            KeyCode::Backspace => {
                app.filter.pop();
            }
            // Navigation passes through to the table so the user can scan
            // filtered rows live. Plain j/k are letters that must type into
            // the filter, but Ctrl+J / Ctrl+K are free — wire them as vim-
            // style nav for users whose hands stay on the home row.
            KeyCode::Up => app.move_selection(-1),
            KeyCode::Down => app.move_selection(1),
            KeyCode::Char('k') if mods.contains(KeyModifiers::CONTROL) => app.move_selection(-1),
            KeyCode::Char('j') if mods.contains(KeyModifiers::CONTROL) => app.move_selection(1),
            KeyCode::PageUp => app.move_selection(-10),
            KeyCode::PageDown => app.move_selection(10),
            KeyCode::Char(c) => {
                app.filter.push(c);
            }
            _ => {}
        }
        return true;
    }
    // `g` is the first half of a `gg` chord (jump to top); `G` jumps to
    // bottom. Both the audit-log overlay and the main table support these
    // (and they share `pending_g` since the two key blocks are mutually
    // exclusive). Any non-`g` key clears the pending chord.
    // Audit-log overlay has its own input scheme: ↑↓/jk scrolls instead of
    // moving the table selection. Vim chords + half-page motion supported.
    if app.cost_overlay_open {
        let was_pending_g = app.pending_g;
        if !matches!(code, KeyCode::Char('g')) {
            app.pending_g = false;
        }
        match code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('$') | KeyCode::Esc => {
                app.cost_overlay_open = false;
                app.cost_overlay_offset = 0;
                app.pending_g = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_add(1);
            }
            KeyCode::PageUp => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_add(10);
            }
            KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_add(10);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                app.cost_overlay_offset = app.cost_overlay_offset.saturating_sub(10);
            }
            KeyCode::Char('g') => {
                if was_pending_g {
                    app.cost_overlay_offset = 0;
                    app.pending_g = false;
                } else {
                    app.pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                app.cost_overlay_offset = u16::MAX;
            }
            KeyCode::Home => app.cost_overlay_offset = 0,
            KeyCode::End => app.cost_overlay_offset = u16::MAX,
            _ => {}
        }
        return true;
    }
    if app.audit_log_open {
        // `g` is special — it might be the first half of a `gg` chord, OR a
        // standalone half-page-down (`Ctrl-d` shape) on its own. We use `gg`
        // for top, `G` for bottom, `Ctrl-d`/`Ctrl-u` for half-page.
        let was_pending_g = app.pending_g;
        // Default: any key that isn't `g` clears the pending chord.
        if !matches!(code, KeyCode::Char('g')) {
            app.pending_g = false;
        }
        match code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Char('H') | KeyCode::Esc => {
                app.audit_log_open = false;
                app.audit_log_offset = 0;
                app.pending_g = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.audit_log_offset = app.audit_log_offset.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.audit_log_offset = app.audit_log_offset.saturating_add(1);
            }
            KeyCode::PageUp => {
                app.audit_log_offset = app.audit_log_offset.saturating_sub(10);
            }
            KeyCode::PageDown => {
                app.audit_log_offset = app.audit_log_offset.saturating_add(10);
            }
            KeyCode::Char('d') if mods.contains(KeyModifiers::CONTROL) => {
                // Ctrl-D: half-page down (~10 lines).
                app.audit_log_offset = app.audit_log_offset.saturating_add(10);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                // Ctrl-U: half-page up (~10 lines).
                app.audit_log_offset = app.audit_log_offset.saturating_sub(10);
            }
            KeyCode::Char('g') => {
                if was_pending_g {
                    // gg: jump to top.
                    app.audit_log_offset = 0;
                    app.pending_g = false;
                } else {
                    app.pending_g = true;
                }
            }
            KeyCode::Char('G') => {
                // Jump to bottom. The next draw's clamp will pull this back
                // to (total_lines - visible_height); using u16::MAX here is
                // a "snap to end" sentinel.
                app.audit_log_offset = u16::MAX;
            }
            KeyCode::Home => {
                app.audit_log_offset = 0;
            }
            KeyCode::End => {
                app.audit_log_offset = u16::MAX;
            }
            _ => {}
        }
        return true;
    }

    let was_pending_g = app.pending_g;
    if !matches!(code, KeyCode::Char('g')) {
        app.pending_g = false;
    }
    match code {
        KeyCode::Char('q') => return false,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Char('g') => {
            if was_pending_g {
                app.select_first();
                app.pending_g = false;
            } else {
                app.pending_g = true;
            }
        }
        KeyCode::Char('G') => app.select_last(),
        KeyCode::Char(' ') => app.detail_open = !app.detail_open,
        KeyCode::Char('m') => {
            app.toggle_mute_selected();
        }
        KeyCode::Char('w') => {
            if let Some((now_watching, label)) = app.toggle_watch_selected() {
                app.status_msg = Some(format!(
                    "{}: {label}",
                    if now_watching { "watching" } else { "unwatched" }
                ));
            } else {
                app.status_msg = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('a') => app.status_msg = Some(deliver_approve(app)),
        KeyCode::Char('d') => app.status_msg = Some(deliver_deny(app)),
        KeyCode::Char('h') => {
            app.toggle_approval_mode();
            app.status_msg = Some(format!("approve mode: {}", app.approval_mode.label()));
        }
        KeyCode::Char('A') => {
            app.toggle_autonomous();
            app.status_msg = Some(format!(
                "autonomous mode: {}",
                if app.autonomous { "ON" } else { "off" }
            ));
        }
        KeyCode::Char('p') => {
            app.toggle_phone_push();
            app.status_msg = Some(format!(
                "phone push: {}",
                if app.phone_push_enabled { "ON" } else { "off" }
            ));
        }
        KeyCode::Char('H') => {
            app.toggle_audit_log();
            app.status_msg = Some(format!(
                "audit log {}",
                if app.audit_log_open { "open" } else { "closed" }
            ));
        }
        KeyCode::Char('$') => {
            app.toggle_cost_overlay();
            app.status_msg = Some(format!(
                "cost overlay {}",
                if app.cost_overlay_open { "open" } else { "closed" }
            ));
        }
        KeyCode::Char('/') => {
            // Enter filter edit mode without clearing the existing query,
            // so the user can keep refining a typed string. Esc inside
            // edit mode is the clear path.
            app.filter_active = true;
            app.pending_g = false;
        }
        KeyCode::Char('r') => {
            app.status_msg = Some("refreshing…".to_string());
        }
        KeyCode::Enter => return jump_to_selected(app),
        _ => {}
    }
    true
}

/// Approve via whichever path is wired: in Hook mode, prefer the hook decision
/// file; if there's no pending UUID (e.g. Claude is in `permission_mode=auto`
/// and our hook bailed), fall back to tmux send-keys when the selected session
/// is genuinely paused. Tmux mode always sends keys directly. Returns the
/// status-bar string to show.
fn deliver_approve(app: &AppState) -> String {
    if app.approval_mode == models::ApprovalMode::Hook
        && let Some(uuid) = app.oldest_pending_uuid()
    {
        approval::approve(&uuid);
        return "approved (hook)".to_string();
    }
    let target = app
        .selected_session()
        .filter(|s| s.status == "waiting")
        .and_then(|s| s.pane.as_ref().map(|p| p.target.clone()));
    let Some(t) = target else {
        return "session not at a prompt (or no pane)".to_string();
    };
    match tmux::send_keys(&t, &["1", "Enter"]) {
        Ok(()) if app.approval_mode == models::ApprovalMode::Hook => {
            format!("approved → {t} (tmux fallback)")
        }
        Ok(()) => format!("approved → {t}"),
        Err(e) => format!("approve failed: {e}"),
    }
}

fn deliver_deny(app: &AppState) -> String {
    if app.approval_mode == models::ApprovalMode::Hook
        && let Some(uuid) = app.oldest_pending_uuid()
    {
        approval::deny(&uuid, "denied via triage");
        return "denied (hook)".to_string();
    }
    let target = app
        .selected_session()
        .filter(|s| s.status == "waiting")
        .and_then(|s| s.pane.as_ref().map(|p| p.target.clone()));
    let Some(t) = target else {
        return "session not at a prompt (or no pane)".to_string();
    };
    match tmux::send_keys(&t, &["Escape"]) {
        Ok(()) if app.approval_mode == models::ApprovalMode::Hook => {
            format!("denied → {t} (tmux fallback)")
        }
        Ok(()) => format!("denied → {t}"),
        Err(e) => format!("deny failed: {e}"),
    }
}

fn refresh(app: &mut AppState) {
    // Cache calling client's width once per tick — `should_zoom_on_jump`
    // reads this to decide whether to auto-zoom on Enter. Cheap (single
    // tmux subprocess). Falls to 0 outside tmux; that disables auto-zoom,
    // leaving the explicit --zoom-on-jump / --exit-on-jump flags as the
    // only zoom path. See specs/notify-self-host.md context for why pane
    // width was wrong (laptop split-screen produces narrow pane on a
    // wide-client terminal).
    app.last_client_width = tmux::current_client_width().unwrap_or(0);
    let mut sessions = discovery::discover_live_sessions();
    let panes = tmux::list_panes();

    let now = SystemTime::now();
    let ppid_map = tmux::build_ppid_map();
    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, &ppid_map, 8);
    }
    transcript::assign_transcripts(&mut sessions, &mut app.digest_cache);
    for s in &mut sessions {
        transcript::enrich(s, now, &mut app.digest_cache);
    }
    // For sessions paused at a permission prompt, the pending tool_use isn't
    // yet in the JSONL — Claude only flushes tool_use+tool_result together
    // after the round-trip completes. Capture the pane and pull the brief
    // from the prompt UI directly. Tool name comes from sessions JSON
    // `waitingFor` ("approve Bash" → "Bash").
    for s in &mut sessions {
        if s.status == "waiting"
            && s.last_tool_use.is_none()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane(&pane.target)
            && let Some(brief) = tmux::parse_pending_brief(&content)
        {
            let name = s
                .waiting_for
                .as_deref()
                .and_then(|w| w.strip_prefix("approve "))
                .unwrap_or("?")
                .to_string();
            s.last_tool_use = Some((name, brief));
        }
    }
    // Pending approvals attach before classify so genuinely waiting sessions
    // can render the hook-captured tool input in the headline/detail.
    let pending = approval::read_pending();
    approval::attach_to_sessions(pending, &mut sessions);
    // Deterministic Blocked scan: for any busy session whose cheaper
    // signals haven't already flagged it (no status=waiting, no hook
    // pending file), capture the pane tail and look for the `❯ 1. Yes` +
    // `Esc to cancel` anchor that only the live permission UI prints.
    // Catches cases where Claude is on its native permission prompt and
    // sessions JSON never wrote status=waiting. No `last_tool_use` or time
    // gate: a pending tool_use is exactly what a permission prompt looks
    // like at the transcript level, so excluding those would miss the
    // case. The strict anchor keeps the false-positive risk negligible.
    for s in &mut sessions {
        if s.status == "busy"
            && s.pending_approvals.is_empty()
            && let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane_tail(&pane.target, 15)
            && tmux::has_pending_permission_prompt(&content)
        {
            s.pane_blocked = true;
        }
    }
    for s in &mut sessions {
        s.state = classifier::classify(s, now);
    }
    app.digest_cache.evict_missing();

    // Auto-unmute any session whose user-text timestamp has advanced past the
    // mute-at time. The user typing in a muted pane is the strongest possible
    // signal that they want it surfaced again. Mute entries for sessions that
    // are no longer live are kept on disk — the session might come back when
    // the user opens that pane again.
    let mute_count_before = app.muted.len();
    app.muted.retain(|key, mute_at| {
        let session = sessions
            .iter()
            .find(|s| s.cwd == key.cwd && s.started_at_ms == key.started_at_ms);
        match session {
            // Not currently live — keep the entry; it'll apply if the session
            // shows up again, and become orphaned otherwise (still harmless).
            None => true,
            Some(s) => match s.last_prompt_at {
                Some(ts) if ts > *mute_at => false,
                _ => true,
            },
        }
    });
    for s in &mut sessions {
        let key = crate::persist::MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        s.muted = app.muted.contains_key(&key);
        s.watched = app.watched.contains(&key);
    }
    if app.muted.len() != mute_count_before {
        app.persist_state();
    }

    drive_autonomous(app, &sessions);

    sessions.sort_by(|a, b| {
        a.muted
            .cmp(&b.muted) // unmuted first
            .then_with(|| a.state.priority().cmp(&b.state.priority()))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });

    // Fire macOS notifications when a session enters an actionable state for
    // the first time we've seen this pid. "Actionable" means Claude itself is
    // waiting on the user (`Blocked`) or the last stop hook errored. Pending
    // hook files alone are not actionable because PreToolUse also fires for
    // auto-approved tool calls.
    for s in &sessions {
        if s.muted {
            continue;
        }
        let is_actionable = matches!(
            s.state,
            models::AttentionState::Blocked | models::AttentionState::Error
        );
        if !is_actionable {
            continue;
        }
        let prev = app.last_states.get(&s.pid).copied();
        if prev == Some(s.state) {
            continue;
        }
        // Defer phone push for Blocked transitions when auto-mode is on:
        // the auditor will likely route the prompt without user action,
        // and a phone buzz that resolves itself a few seconds later is
        // noise. Phone push fires later only if the auditor returns WAIT
        // (see `drive_autonomous` verdict drain). Error transitions still
        // fire phone immediately — no auditor involvement on Error.
        // Also gated on the user-level `phone_push_enabled` toggle (T-79):
        // when off, never POST to ntfy regardless of state. Mac local
        // banner still fires from `notify_os::alert` unchanged.
        let phone_push = app.phone_push_enabled
            && !(app.autonomous && s.state == models::AttentionState::Blocked);
        notify_os::alert(s, &app.config, phone_push);
    }
    // T-81 watch fire: any watched session that just transitioned into
    // `JustFinished` gets a "finished" banner (Mac local + ntfy gated on the
    // phone toggle). Watch is sticky — we deliberately do NOT remove from
    // `app.watched`; the user clears it with `w` when they're done.
    for s in &sessions {
        if !s.watched {
            continue;
        }
        if s.state != models::AttentionState::JustFinished {
            continue;
        }
        let prev = app.last_states.get(&s.pid).copied();
        if prev == Some(models::AttentionState::JustFinished) {
            continue;
        }
        notify_os::notify_session_done(s, &app.config, app.phone_push_enabled);
    }
    app.last_states = sessions.iter().map(|s| (s.pid, s.state)).collect();

    app.sessions = sessions;
    app.clamp_selection();
    app.status_msg = None;
}

/// T-56: autonomous-mode driver. Runs every refresh tick whether the toggle is
/// on or off — drains finished verdicts (so an in-flight worker doesn't leak
/// channel capacity after toggle-off) and clears stale `audit_decided` entries.
/// Only the spawn-new-workers path is gated on `app.autonomous`.
///
/// Invariants:
/// - one worker per pid at a time (`audit_in_flight`)
/// - one decision per (cwd, started_at_ms) per Blocked spell (`audit_decided`,
///   reset when the session leaves Blocked)
/// - muted sessions are skipped in both routing and spawning
/// - if the user toggled autonomous off while a worker was running, the
///   returning verdict is logged in `audit_notes` for visibility but NOT
///   actioned
fn drive_autonomous(app: &mut ui::AppState, sessions: &[models::Session]) {
    use std::collections::HashSet;

    // Reset audit_decided for sessions that have left the Blocked state. Next
    // time they pause on a permission prompt we'll audit again.
    let blocked_keys: HashSet<persist::MuteKey> = sessions
        .iter()
        .filter(|s| s.state == models::AttentionState::Blocked)
        .map(|s| persist::MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        })
        .collect();
    app.audit_decided.retain(|k| blocked_keys.contains(k));

    // Drain returning verdicts. Routing happened in the worker thread (so the
    // decision file lands before the claim is removed and the hook reacts).
    // Here we just record state + emit a status message. Action gated on
    // app.autonomous: if the user toggled off mid-flight, we still want the
    // note for visibility, but the worker already routed — that's a known
    // race, mitigated by the "session no longer Blocked" check the worker
    // would skip on.
    while let Ok(v) = app.audit_rx.try_recv() {
        app.audit_in_flight.remove(&v.pid);
        // Include cost + duration alongside the decision so the user can see
        // what each audit ran them ($) and how long Sonnet took (rate-limit
        // diagnosis). Both fields are None on subprocess failures.
        let perf = match (v.cost_usd, v.duration_ms) {
            (Some(c), Some(ms)) => {
                format!(" ({:.1}s, ${:.4})", ms as f64 / 1000.0, c)
            }
            (Some(c), None) => format!(" (${:.4})", c),
            (None, Some(ms)) => format!(" ({:.1}s)", ms as f64 / 1000.0),
            (None, None) => String::new(),
        };
        let note = format!("{}{} — {}", v.decision, perf, v.reason);
        app.audit_notes.insert(v.pid, (SystemTime::now(), note));
        if !app.autonomous {
            continue;
        }
        let Some(s) = sessions.iter().find(|s| {
            s.pid == v.pid && s.state == models::AttentionState::Blocked && !s.muted
        }) else {
            continue;
        };
        let key = persist::MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        app.audit_decided.insert(key);
        app.status_msg = Some(format!(
            "auditor {}: pid {} ({})",
            v.decision, v.pid, v.tool_name
        ));
        // Phone push for the WAIT verdict — this is the case the user
        // actually needs to act on. APPROVE/DENY were handled by the
        // auditor's hook routing, so a phone buzz would be noise. The
        // original `notify_os::alert` call from refresh() already fired
        // the desktop notification with `phone_push=false`; this is the
        // deferred phone fan-out.
        if v.decision == "WAIT" && app.phone_push_enabled {
            notify_os::push_to_phone(s, &app.config);
        }
    }

    if !app.autonomous {
        return;
    }

    // Spawn new workers for Blocked sessions not in flight, not yet decided,
    // not muted, and with an actual tool_use to feed the auditor.
    for s in sessions
        .iter()
        .filter(|s| s.state == models::AttentionState::Blocked && !s.muted)
    {
        if app.audit_in_flight.contains_key(&s.pid) {
            continue;
        }
        let key = persist::MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        if app.audit_decided.contains(&key) {
            continue;
        }
        // Prefer hook-captured (richer, structured, FULL untruncated input).
        // When the hook didn't fire (timed out for a stale Blocked, or the
        // session is in `permission_mode=auto` so the hook bailed), do a
        // fresh pane capture and parse the full pending command — NOT the
        // UI brief in `last_tool_use.1`, which is line-capped to 20 and
        // joined with spaces (auditor was refusing on "truncated heredoc"
        // for legitimate Bash commands).
        let (tool_name, tool_input) = if let Some(a) = s.pending_approvals.first() {
            (a.tool_name.clone(), a.tool_input_full.clone())
        } else if let Some(pane) = &s.pane
            && let Some(content) = tmux::capture_pane(&pane.target)
            && let Some(full_input) = tmux::parse_pending_full(&content)
        {
            let tool_name = s
                .last_tool_use
                .as_ref()
                .map(|(n, _)| n.clone())
                .or_else(|| {
                    s.waiting_for
                        .as_deref()
                        .and_then(|w| w.strip_prefix("approve "))
                        .map(String::from)
                })
                .unwrap_or_else(|| "?".to_string());
            (tool_name, full_input)
        } else if let Some((n, b)) = &s.last_tool_use {
            // Last-resort: the row already had a tool_use from transcript +
            // brief from earlier capture, but the pane scrape just failed
            // (process gone, etc.). Use the brief — better than nothing.
            (n.clone(), b.clone())
        } else {
            continue;
        };
        let intent = s
            .last_prompt
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        // Pass the away_summary recap as broader work context so the auditor
        // doesn't have to anchor the entire decision on the most recent
        // user message (which is often a refinement question, not a
        // green-light directive).
        let recent_recap = s.headline.clone();
        let pid = s.pid;
        let cwd = s.cwd.clone();
        // Capture everything the worker thread needs to route the decision
        // synchronously (so the decision file lands BEFORE remove_claim
        // signals the hook). The hook path needs the uuid; tmux fallback
        // needs the pane target; we capture both and let the worker pick.
        let uuid = s.pending_approvals.first().map(|a| a.uuid.clone());
        let pane_target = s.pane.as_ref().map(|p| p.target.clone());
        let approval_mode = app.approval_mode;
        let tx = app.audit_tx.clone();
        // Stake the claim BEFORE spawning so the hook sees it on its next
        // poll (within 500ms) and extends its deadline.
        if let Some(ref uuid) = uuid {
            approval::write_claim(uuid);
        }
        app.audit_in_flight.insert(pid, SystemTime::now());
        std::thread::spawn(move || {
            let v = auditor::run_audit(
                pid,
                &cwd,
                recent_recap.as_deref(),
                &intent,
                &tool_name,
                &tool_input,
            );
            // Route APPROVE/DENY here so the decision file lands BEFORE
            // remove_claim. WAIT writes nothing — the hook sees claim removal
            // with no decision and bails to Claude's native flow.
            match v.decision.as_str() {
                "APPROVE" => {
                    route_decision(approval_mode, uuid.as_deref(), pane_target.as_deref(), true, &v.reason);
                }
                "DENY" => {
                    route_decision(approval_mode, uuid.as_deref(), pane_target.as_deref(), false, &v.reason);
                }
                _ => {} // WAIT: leave for human
            }
            let _ = tx.send(v);
            if let Some(uuid) = uuid {
                approval::remove_claim(&uuid);
            }
        });
    }
}

/// Route the auditor's APPROVE/DENY through the same machinery as manual
/// `a`/`d`. Runs in the auditor's worker thread (not the main thread) so the
/// decision file lands BEFORE `remove_claim` signals the hook — otherwise the
/// hook would react to claim removal and bail to Claude's native flow before
/// our decision had a chance to be picked up. Takes captured-by-value fields
/// instead of `&Session` because the session list is local to the main
/// thread's refresh and may be gone by the time the auditor returns.
fn route_decision(
    mode: models::ApprovalMode,
    uuid: Option<&str>,
    pane_target: Option<&str>,
    approve: bool,
    reason: &str,
) {
    if mode == models::ApprovalMode::Hook
        && let Some(uuid) = uuid
    {
        if approve {
            approval::approve(uuid);
        } else {
            approval::deny(uuid, reason);
        }
        return;
    }
    let Some(target) = pane_target else { return };
    let keys: &[&str] = if approve { &["1", "Enter"] } else { &["Escape"] };
    let _ = tmux::send_keys(target, keys);
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// Readline-style Ctrl+W: delete the last word from `s`. Strips trailing
/// whitespace first (so `foo bar  <^W>` becomes `foo `), then pops
/// non-whitespace until the word is gone (so the next press gets `foo`).
fn delete_prev_word(s: &mut String) {
    while s.ends_with(char::is_whitespace) {
        s.pop();
    }
    while let Some(c) = s.chars().last() {
        if c.is_whitespace() {
            break;
        }
        s.pop();
    }
}

/// Jump to the currently-selected session's pane. Returns the value to
/// propagate from `handle_key` — `false` only when `exit_on_jump` is set
/// and the jump succeeded (popup-launch lifecycle). On success the filter
/// is cleared so the next visit starts unfiltered; on failure the filter
/// stays so the user can retry without retyping.
fn jump_to_selected(app: &mut AppState) -> bool {
    let Some(s) = app.selected_session() else {
        return true;
    };
    let Some(pane) = &s.pane else {
        app.status_msg = Some("no tmux pane for this session".to_string());
        return true;
    };
    let target = pane.target.clone();
    match tmux::jump_to(&target, app.should_zoom_on_jump()) {
        Ok(()) => {
            app.status_msg = Some(format!("jumped → {target}"));
            app.filter.clear();
            if app.exit_on_jump {
                return false;
            }
        }
        Err(e) => {
            app.status_msg = Some(format!("jump failed: {e}"));
        }
    }
    true
}
