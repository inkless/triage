mod agent_comm;
mod approval;
mod auditor;
mod classifier;
mod codex;
mod config;
mod cost_rollup;
mod discovery;
mod models;
mod notify_os;
mod persist;
mod snapshot;
mod spawn_agent;
mod tmux;
mod transcript;
mod ui;
mod watcher;

use std::io;
use std::time::{Duration, Instant, SystemTime};

use clap::{Parser, Subcommand};
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
/// Preview-rail re-capture interval for the *same* pane (TRI-138). Selection
/// changes retarget immediately; this just bounds the live-refresh of a pane
/// you're already hovering. 250ms (~4fps) reads as live for watching a pane and
/// costs one `capture-pane` per interval for a single pane.
const PREVIEW_REFRESH: Duration = Duration::from_millis(250);

/// Top-level CLI. A bare `triage` (no subcommand, no mode flag) launches the
/// TUI; everything else is a one-shot. The mode flags are kept as flags (rather
/// than promoted to subcommands) so existing invocations wired into tmux
/// bindings and internal spawn strings — `triage --jump-to-self`,
/// `triage --zoom-on-jump` — keep working unchanged.
#[derive(Parser, Debug)]
#[command(
    name = "triage",
    version,
    about = "TUI to monitor parallel Claude Code sessions across tmux panes",
    after_help = AFTER_HELP,
    group = clap::ArgGroup::new("hook_action").args(["install_hooks", "uninstall_hooks"]).multiple(true)
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// print joined session table (non-TUI smoke test)
    #[arg(long)]
    probe: bool,
    /// install triage's PreToolUse hook into ~/.claude/settings.json
    #[arg(long)]
    install_hooks: bool,
    /// remove triage's PreToolUse hook entries
    #[arg(long)]
    uninstall_hooks: bool,
    /// print manual hook-install instructions
    #[arg(long)]
    install_hooks_hint: bool,
    /// with --install-hooks / --uninstall-hooks: show changes without writing
    #[arg(long, requires = "hook_action")]
    dry_run: bool,
    /// feed the given session pid's pending tool_use to the auditor
    #[arg(long, value_name = "PID")]
    audit: Option<u32>,
    /// print the auditor prompt to stdout
    #[arg(long)]
    audit_prompt: bool,
    /// focus an existing triage pane (tmux-binding entrypoint)
    #[arg(long)]
    jump_to_self: bool,
    /// with --jump-to-self: zoom the target pane (mobile)
    #[arg(long, requires = "jump_to_self")]
    zoom: bool,
    /// exit cleanly after a successful Enter jump (popup-launch mode)
    #[arg(long)]
    exit_on_jump: bool,
    /// force zoom-on-jump regardless of pane width
    #[arg(long)]
    zoom_on_jump: bool,
    /// spawn a new TUI instance even if one is already alive
    #[arg(long)]
    force_new: bool,
}

/// One-shot subcommands. Each captures its remaining args verbatim and hands
/// them to the module's own parser — clap owns top-level routing, `--help`, and
/// unknown-input rejection; the subcommands keep their established flag parsing
/// (and their own `--help`) untouched. `disable_help_flag` lets `--help` pass
/// through to those handlers rather than being intercepted here.
#[derive(Subcommand, Debug)]
enum Command {
    /// Send a one-shot notification: desktop banner + ntfy phone push (both by default)
    #[command(disable_help_flag = true)]
    Notify {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Daily/weekly Claude spend across every transcript
    #[command(disable_help_flag = true)]
    Cost {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List peer agents, or `whoami` to introspect the calling pane
    #[command(disable_help_flag = true)]
    Agents {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a guarded message to a live agent pane
    #[command(disable_help_flag = true)]
    Send {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch a configured Claude/Codex agent tmux window
    #[command(disable_help_flag = true)]
    Launch {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

const AFTER_HELP: &str = "\
With no subcommand, triage launches the TUI (or silent-attaches to a running
instance in the current tmux session).

In-TUI keybindings:
  ⏎ jump · a/d approve/deny · A toggle auto mode · p preview · > flip preview side
  P phone push · Space detail · r reply · m mute · w watch · * pin · R rename · N new agent · / filter
  l audit log · $ cost overlay · ? keys · q quit

Docs:
  README:      https://github.com/inkless/triage
  config file: ~/.config/triage/config.toml (optional)";

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    // One-shot subcommands: clap routed them; their own parsers handle flags.
    if let Some(command) = cli.command {
        match command {
            Command::Notify { args } => return notify_os::cli_notify(&args),
            Command::Cost { args } => return cost_rollup::cli_cost(&args),
            Command::Agents { args } => std::process::exit(agent_comm::cli_agents(&args)),
            Command::Send { args } => std::process::exit(agent_comm::cli_send(&args)),
            Command::Launch { args } => std::process::exit(spawn_agent::cli_launch(&args)),
        }
    }

    // One-shot mode flags, dispatched in the original precedence order
    // (probe first, then the hook actions, audit, and the jump entrypoint).
    if cli.probe {
        return probe();
    }
    if cli.install_hooks_hint {
        approval::print_install_hint();
        return Ok(());
    }
    if cli.install_hooks {
        approval::install_hooks(cli.dry_run)?;
        return Ok(());
    }
    if cli.uninstall_hooks {
        approval::uninstall_hooks(cli.dry_run)?;
        return Ok(());
    }
    // T-56 spike: feed the selected session's pending tool_use to a separate
    // `claude -p` and print the auditor's verdict. No actual approve/deny.
    if let Some(pid) = cli.audit {
        return auditor::audit(pid);
    }
    if cli.audit_prompt {
        auditor::print_prompt();
        return Ok(());
    }
    // Tmux-binding entrypoint: focus an existing triage pane or spawn one.
    // Skips all discovery / transcript / watcher init so the focus switch is
    // essentially just tmux subprocess overhead (<30ms cold). `--zoom`
    // additionally `resize-pane -Z`s the triage pane so it fills the screen —
    // for the mobile binding (M-/) where pane multitasking is unworkable.
    if cli.jump_to_self {
        return tmux::jump_to_self(cli.zoom);
    }

    let exit_on_jump = cli.exit_on_jump;
    // Per-launch only — not persisted. Mobile users either launch their
    // long-lived triage with this flag (and accept that desktop also zooms on
    // Enter), or rely on the auto-detect path inside Enter.
    let zoom_on_jump = exit_on_jump || cli.zoom_on_jump;
    let force_new = cli.force_new;

    // Silent attach: typing `triage` (no special flags) when one's already
    // running just switches the user to it and exits 0. Skipped when:
    //   - --force-new (explicit "I want a second instance, e.g. for debug")
    //   - --exit-on-jump (popup-launch context — caller handles lifecycle)
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

fn probe() -> io::Result<()> {
    let now = SystemTime::now();
    let panes = tmux::list_panes().len();
    let loaded_state = persist::load_state();
    let aliases = loaded_state.aliases.into_iter().collect();
    let mut digest_cache = transcript::DigestCache::new();
    let mut codex_cache = codex::CodexDigestCache::new();
    let sessions = snapshot::discover_sessions(now, &mut digest_cache, &mut codex_cache, &aliases);
    let claude_count = sessions
        .iter()
        .filter(|s| s.provider == models::Provider::Claude)
        .count();
    let codex_count = sessions
        .iter()
        .filter(|s| s.provider == models::Provider::Codex)
        .count();
    println!(
        "# discovered {} live sessions ({} Claude, {} Codex), {} tmux panes\n",
        sessions.len(),
        claude_count,
        codex_count,
        panes
    );
    for s in &sessions {
        let pane = s
            .pane
            .as_ref()
            .map(|p| p.target.as_str())
            .unwrap_or("(none)");
        let headline = s
            .headline
            .as_deref()
            .or(s.last_prompt.as_deref())
            .unwrap_or("(no transcript)")
            .replace('\n', " ");
        let head_short: String = headline.chars().take(80).collect();
        let name_short: String = s.name.as_deref().unwrap_or("-").chars().take(28).collect();
        println!(
            "  {:<2} pid={:<6} state={:<6} status={:<5} pane={:<24} name={:<28} cwd={}",
            s.provider.label(),
            s.pid,
            s.state.label(),
            s.status,
            pane,
            name_short,
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
    app.approval_mode = app.config.approval_mode;
    app.exit_on_jump = exit_on_jump;
    app.zoom_on_jump = zoom_on_jump;
    let refresh_interval = Duration::from_secs(app.config.thresholds.refresh_seconds.max(1));
    let watcher = FsWatcher::spawn().ok();

    refresh(&mut app);
    let mut last_refresh = Instant::now();
    // Preview-rail capture cadence (TRI-138): retarget instantly when the
    // selected pane changes, otherwise re-capture the same pane at most every
    // PREVIEW_REFRESH. Keeps holding `j`/`k` from firing a capture per repeat.
    let mut last_preview = Instant::now();
    let mut last_preview_pane: Option<String> = None;

    loop {
        let now = SystemTime::now();
        if app.preview_open {
            let target = app
                .selected_session()
                .and_then(|s| s.pane.as_ref())
                .map(|p| p.pane_id.clone());
            let retargeted = target != last_preview_pane;
            if retargeted || last_preview.elapsed() >= PREVIEW_REFRESH {
                app.capture_selected_preview();
                last_preview = Instant::now();
                last_preview_pane = target;
            }
        } else if last_preview_pane.is_some() {
            last_preview_pane = None;
        }
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
    if app.key_help_open {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                app.key_help_open = false;
                app.pending_g = false;
            }
            _ => {}
        }
        return true;
    }

    if app.reply_active {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc => {
                app.cancel_reply();
                app.status_msg = Some("reply canceled".to_string());
            }
            KeyCode::Enter => {
                app.status_msg = Some(send_reply(app));
            }
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                delete_prev_word(&mut app.reply_buffer);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                app.reply_buffer.clear();
            }
            KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
                app.reply_buffer.pop();
            }
            KeyCode::Backspace => {
                app.reply_buffer.pop();
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                app.reply_buffer.push(c);
            }
            _ => {}
        }
        return true;
    }

    if app.rename_active {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc => {
                app.cancel_rename();
                app.status_msg = Some("rename canceled".to_string());
            }
            KeyCode::Enter => {
                app.status_msg = app
                    .commit_rename_selected()
                    .or_else(|| Some("no session selected".to_string()));
            }
            KeyCode::Char('w') if mods.contains(KeyModifiers::CONTROL) => {
                delete_prev_word(&mut app.rename_buffer);
            }
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                app.rename_buffer.clear();
            }
            KeyCode::Char('h') if mods.contains(KeyModifiers::CONTROL) => {
                app.rename_buffer.pop();
            }
            KeyCode::Backspace => {
                app.rename_buffer.pop();
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                app.rename_buffer.push(c);
            }
            _ => {}
        }
        return true;
    }

    if app.spawn_picker_open {
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Esc | KeyCode::Char('q') => {
                app.cancel_spawn_picker();
                app.status_msg = Some("new agent canceled".to_string());
            }
            KeyCode::Enter => {
                app.status_msg = Some(launch_new_agent_from_picker(app));
            }
            KeyCode::Char('?') => {
                app.key_help_open = true;
                app.pending_g = false;
            }
            KeyCode::Up | KeyCode::Char('k') => app.move_spawn_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => app.move_spawn_selection(1),
            KeyCode::PageUp => app.move_spawn_selection(-10),
            KeyCode::PageDown => app.move_spawn_selection(10),
            KeyCode::Home => app.spawn_picker_selected = 0,
            KeyCode::End => {
                let len = app.spawn_cwd_choices().len();
                if len > 0 {
                    app.spawn_picker_selected = len - 1;
                }
            }
            _ => {}
        }
        return true;
    }

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
            // filtered rows live. j/k are deliberately NOT here — they're
            // letters that must type into the filter.
            KeyCode::Up => app.move_selection(-1),
            KeyCode::Down => app.move_selection(1),
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
            KeyCode::Char('?') => {
                app.key_help_open = true;
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
            KeyCode::Char('l') | KeyCode::Char('H') | KeyCode::Esc => {
                app.audit_log_open = false;
                app.audit_log_offset = 0;
                app.pending_g = false;
            }
            KeyCode::Char('?') => {
                app.key_help_open = true;
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
        KeyCode::Char('?') => {
            app.key_help_open = true;
            app.status_msg = None;
        }
        KeyCode::Char(' ') => app.toggle_detail(),
        KeyCode::Char('p') => {
            app.toggle_preview();
            app.status_msg = Some(format!(
                "preview {}",
                if app.preview_open { "on" } else { "off" }
            ));
        }
        KeyCode::Char('>') if app.preview_open => {
            let pos = app.flip_preview_pos();
            app.status_msg = Some(format!(
                "preview: {}",
                match pos {
                    ui::PreviewPos::Right => "right",
                    ui::PreviewPos::Bottom => "bottom",
                }
            ));
        }
        KeyCode::Char('m') => {
            app.toggle_mute_selected();
        }
        KeyCode::Char('*') => {
            if let Some((now_pinned, label)) = app.toggle_pin_selected() {
                app.status_msg = Some(format!(
                    "{}: {label}",
                    if now_pinned { "pinned" } else { "unpinned" }
                ));
            } else {
                app.status_msg = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('w') => {
            if let Some((now_watching, label)) = app.toggle_watch_selected() {
                app.status_msg = Some(format!(
                    "{}: {label}",
                    if now_watching {
                        "watching"
                    } else {
                        "unwatched"
                    }
                ));
            } else {
                app.status_msg = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('R') => {
            if app.start_rename_selected().is_some() {
                app.status_msg = None;
            } else {
                app.status_msg = Some("no session selected".to_string());
            }
        }
        KeyCode::Char('N') => {
            app.start_spawn_picker();
            app.status_msg = None;
        }
        KeyCode::Char('a') => app.status_msg = Some(deliver_approve(app)),
        KeyCode::Char('d') => app.status_msg = Some(deliver_deny(app)),
        KeyCode::Char('A') => {
            app.toggle_autonomous();
            app.status_msg = Some(format!(
                "autonomous mode: {}",
                if app.autonomous { "ON" } else { "off" }
            ));
        }
        KeyCode::Char('P') => {
            app.toggle_phone_push();
            app.status_msg = Some(format!(
                "phone push: {}",
                if app.phone_push_enabled { "ON" } else { "off" }
            ));
        }
        // `l` = log (the easy primary); `H` = History (kept as an alias so
        // existing muscle memory still works).
        KeyCode::Char('l') | KeyCode::Char('H') => {
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
                if app.cost_overlay_open {
                    "open"
                } else {
                    "closed"
                }
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
            if app.start_reply_selected().is_some() {
                app.status_msg = None;
            } else {
                app.status_msg = Some("no tmux pane for reply target".to_string());
            }
        }
        KeyCode::Enter => return jump_to_selected(app),
        _ => {}
    }
    true
}

fn send_reply(app: &mut AppState) -> String {
    if app.reply_buffer.trim().is_empty() {
        return "reply is empty".to_string();
    }
    let Some(target) = app.reply_target.clone() else {
        app.cancel_reply();
        return "no reply target".to_string();
    };
    let body = app.reply_buffer.clone();
    app.cancel_reply();
    match agent_comm::send_user_reply(&target, &body) {
        Ok(msg) => msg,
        Err(e) => format!("reply failed: {e}"),
    }
}

fn launch_new_agent_from_picker(app: &mut AppState) -> String {
    let Some(cwd) = app.selected_spawn_cwd() else {
        app.cancel_spawn_picker();
        return "no working directory selected".to_string();
    };
    let provider = app.config.new_agent.provider.name();
    match spawn_agent::launch(&app.config.new_agent, &cwd) {
        Ok(outcome) => {
            app.cancel_spawn_picker();
            format!(
                "launched {provider} in window {} ({})",
                outcome.window_name,
                cwd.display()
            )
        }
        Err(e) => {
            app.cancel_spawn_picker();
            format!("launch failed: {e}")
        }
    }
}

/// Approve via whichever path is wired: in Hook mode, prefer the hook decision
/// file; if there's no pending UUID (e.g. Claude is in `permission_mode=auto`
/// and our hook bailed), fall back to tmux send-keys when the selected session
/// is genuinely paused. Tmux mode always sends keys directly. Returns the
/// status-bar string to show.
fn deliver_approve(app: &AppState) -> String {
    let Some(selected) = app.selected_session() else {
        return "no session selected".to_string();
    };
    if selected.provider == models::Provider::Codex {
        return deliver_codex_decision(selected, true);
    }

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
    // Just Enter — Claude's permission prompt defaults the highlight to
    // option 1 (Yes). Sending a literal "1" first used to be belt-and-
    // suspenders, but in the status-stale window where the prompt has
    // already been dismissed the "1" lands as text in the chat input.
    // Enter on an empty prompt is a no-op; "1" is visible damage.
    match tmux::send_keys(&t, &["Enter"]) {
        Ok(()) if app.approval_mode == models::ApprovalMode::Hook => {
            format!("approved → {t} (tmux fallback)")
        }
        Ok(()) => format!("approved → {t}"),
        Err(e) => format!("approve failed: {e}"),
    }
}

fn deliver_deny(app: &AppState) -> String {
    let Some(selected) = app.selected_session() else {
        return "no session selected".to_string();
    };
    if selected.provider == models::Provider::Codex {
        return deliver_codex_decision(selected, false);
    }

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

fn deliver_codex_decision(s: &models::Session, approve: bool) -> String {
    if s.state != models::AttentionState::Blocked || !s.approval_prompt_pending {
        return "codex session not blocked".to_string();
    }
    let Some(target) = s.pane.as_ref().map(|p| p.target.clone()) else {
        return "codex session has no pane".to_string();
    };
    match route_codex_decision(&target, approve) {
        Ok(()) if approve => format!("approved codex → {target}"),
        Ok(()) => format!("denied codex → {target}"),
        Err(e) if approve => format!("codex approve failed: {e}"),
        Err(e) => format!("codex deny failed: {e}"),
    }
}

fn route_codex_decision(target: &str, approve: bool) -> Result<(), String> {
    let Some(content) = tmux::capture_pane_tail(target, 80) else {
        return Err(format!("prompt check failed: {target}"));
    };
    if !tmux::has_codex_permission_prompt(&content) {
        return Err("prompt no longer visible".to_string());
    }
    let Some(selected) = tmux::codex_selected_permission_choice(&content) else {
        return Err("prompt has no selected choice".to_string());
    };
    if approve && selected != tmux::CodexPromptChoice::Yes {
        return Err("Yes is not selected".to_string());
    }

    let key = if approve { "Enter" } else { "Escape" };
    tmux::send_keys(target, &[key]).map_err(|e| e.to_string())
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
    let now = SystemTime::now();
    let mut sessions = snapshot::discover_sessions(
        now,
        &mut app.digest_cache,
        &mut app.codex_cache,
        &app.aliases,
    );

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
            Some(s) => !matches!(s.last_prompt_at, Some(ts) if ts > *mute_at),
        }
    });
    for s in &mut sessions {
        let key = crate::persist::MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        s.muted = app.muted.contains_key(&key);
        s.watched = app.watched.contains(&key);
        // Pin and mute are mutually exclusive; toggles enforce it, but guard
        // here too so any legacy state.json with both set honors mute-wins.
        s.pinned = app.pinned.contains(&key) && !s.muted;
    }
    if app.muted.len() != mute_count_before {
        app.persist_state();
    }

    drive_autonomous(app, &sessions);

    snapshot::sort_sessions(&mut sessions);

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
        let phone_push =
            app.phone_push_enabled && !(app.autonomous && is_auto_auditable_blocked(s));
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

fn is_auto_auditable_blocked(s: &models::Session) -> bool {
    matches!(
        s.provider,
        models::Provider::Claude | models::Provider::Codex
    ) && s.state == models::AttentionState::Blocked
}

fn audit_payload_for_session(s: &models::Session) -> Option<(String, String)> {
    match s.provider {
        models::Provider::Claude => audit_payload_for_claude(s),
        models::Provider::Codex => audit_payload_for_codex(s),
    }
}

fn audit_payload_for_claude(s: &models::Session) -> Option<(String, String)> {
    // Prefer hook-captured (richer, structured, FULL untruncated input).
    // When the hook didn't fire (timed out for a stale Blocked, or the
    // session is in `permission_mode=auto` so the hook bailed), do a fresh
    // pane capture and parse the full pending command, not the UI brief.
    if let Some(a) = s.pending_approvals.first() {
        return Some((a.tool_name.clone(), a.tool_input_full.clone()));
    }
    if let Some(pane) = &s.pane
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
        return Some((tool_name, full_input));
    }
    s.last_tool_use
        .as_ref()
        .map(|(name, brief)| (name.clone(), brief.clone()))
}

fn audit_payload_for_codex(s: &models::Session) -> Option<(String, String)> {
    let pane = s.pane.as_ref()?;
    let content = tmux::capture_pane_tail(&pane.target, 80)?;
    if !tmux::has_codex_permission_prompt(&content)
        || tmux::codex_selected_permission_choice(&content).is_none()
    {
        return None;
    }
    let tool_name = s
        .last_tool_use
        .as_ref()
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "codex tool".to_string());
    let tool_input = tmux::parse_codex_pending_full(&content)
        .or_else(|| s.last_tool_use.as_ref().map(|(_, brief)| brief.clone()))?;
    Some((tool_name, tool_input))
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
        .filter(|s| is_auto_auditable_blocked(s))
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
        let Some(s) = sessions
            .iter()
            .find(|s| s.pid == v.pid && is_auto_auditable_blocked(s) && !s.muted)
        else {
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
        .filter(|s| is_auto_auditable_blocked(s) && !s.muted)
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
        let Some((tool_name, tool_input)) = audit_payload_for_session(s) else {
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
        let provider = s.provider;
        let approval_mode = app.approval_mode;
        let tx = app.audit_tx.clone();
        // Stake the claim BEFORE spawning so the hook sees it on its next
        // poll (within 500ms) and extends its deadline.
        if let Some(ref uuid) = uuid {
            approval::write_claim(uuid);
        }
        app.audit_in_flight.insert(pid, SystemTime::now());
        std::thread::spawn(move || {
            let mut v = auditor::run_audit(
                pid,
                &cwd,
                recent_recap.as_deref(),
                &intent,
                &tool_name,
                &tool_input,
            );
            // Route APPROVE/DENY here so the decision file lands BEFORE
            // remove_claim. WAIT writes nothing — the hook sees claim removal
            // with no decision and bails to Claude's native flow. Codex has
            // no hook path, so route it through the same fresh prompt check
            // as manual `a`/`d`.
            let route_result = match (provider, v.decision.as_str()) {
                (models::Provider::Claude, "APPROVE") => {
                    route_decision(
                        approval_mode,
                        uuid.as_deref(),
                        pane_target.as_deref(),
                        true,
                        &v.reason,
                    );
                    Ok(())
                }
                (models::Provider::Claude, "DENY") => {
                    route_decision(
                        approval_mode,
                        uuid.as_deref(),
                        pane_target.as_deref(),
                        false,
                        &v.reason,
                    );
                    Ok(())
                }
                (models::Provider::Codex, "APPROVE") => {
                    if let Some(target) = pane_target.as_deref() {
                        route_codex_decision(target, true)
                    } else {
                        Err("codex session has no pane".to_string())
                    }
                }
                (models::Provider::Codex, "DENY") => {
                    if let Some(target) = pane_target.as_deref() {
                        route_codex_decision(target, false)
                    } else {
                        Err("codex session has no pane".to_string())
                    }
                }
                _ => Ok(()), // WAIT: leave for human
            };
            if let Err(e) = route_result {
                let audited_decision = v.decision.clone();
                v.decision = "WAIT".to_string();
                v.reason = format!("auditor {audited_decision} but routing failed: {e}");
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
    let keys: &[&str] = if approve { &["Enter"] } else { &["Escape"] };
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

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::{CommandFactory, Parser};

    fn parse(parts: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("triage").chain(parts.iter().copied()))
    }

    #[test]
    fn clap_config_is_valid() {
        // Catches derive misconfigurations (conflicting flags, bad arg specs).
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_is_a_tui_launch() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
        assert!(!cli.jump_to_self && !cli.probe && !cli.force_new);
    }

    #[test]
    fn mode_flags_parse() {
        let cli = parse(&["--jump-to-self", "--zoom"]).unwrap();
        assert!(cli.jump_to_self && cli.zoom);
        assert_eq!(parse(&["--audit", "1234"]).unwrap().audit, Some(1234));
    }

    #[test]
    fn audit_requires_numeric_pid() {
        assert!(parse(&["--audit", "notapid"]).is_err());
    }

    #[test]
    fn subcommand_args_pass_through_verbatim() {
        // Everything after the subcommand name (including flags) is captured
        // raw for the module parser — clap must not try to interpret it.
        match parse(&["agents", "whoami", "--json"]).unwrap().command {
            Some(Command::Agents { args }) => assert_eq!(args, ["whoami", "--json"]),
            other => panic!("expected agents passthrough, got {other:?}"),
        }
        match parse(&["send", "--to", "%1", "hello"]).unwrap().command {
            Some(Command::Send { args }) => assert_eq!(args, ["--to", "%1", "hello"]),
            other => panic!("expected send passthrough, got {other:?}"),
        }
    }

    #[test]
    fn unknown_subcommand_or_flag_is_rejected() {
        assert!(parse(&["bogus"]).is_err());
        assert!(parse(&["--prob"]).is_err());
    }

    // Regression guards vs the pre-clap dispatch (parity audit).

    #[test]
    fn help_word_is_still_accepted() {
        // `triage help` printed top-level help before clap; clap's built-in
        // help subcommand must remain enabled so this keeps working.
        let err = parse(&["help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn meaningless_flag_combos_are_rejected() {
        // Old TRI-124 strictness: these no-op-on-their-own flags must error
        // rather than silently launch the TUI.
        assert!(parse(&["--zoom"]).is_err(), "--zoom needs --jump-to-self");
        assert!(
            parse(&["--dry-run"]).is_err(),
            "--dry-run needs a hook action"
        );
        // ...but the valid pairings still parse.
        assert!(parse(&["--jump-to-self", "--zoom"]).is_ok());
        assert!(parse(&["--install-hooks", "--dry-run"]).is_ok());
        assert!(parse(&["--uninstall-hooks", "--dry-run"]).is_ok());
    }
}
