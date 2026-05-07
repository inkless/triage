mod approval;
mod classifier;
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

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const TICK_INTERVAL: Duration = Duration::from_millis(250);
/// Minimum gap between watcher-triggered refreshes. Without this, an actively
/// writing jsonl can fire fs events fast enough that refresh runs every loop
/// iteration, blocking key handling. The 2s REFRESH_INTERVAL still applies as
/// the upper bound when nothing is changing.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(400);

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--probe") {
        return probe();
    }
    if args.iter().any(|a| a == "--install-hooks-hint") {
        approval::print_install_hint();
        return Ok(());
    }

    // Aliveness guard sticks around for the whole interactive session. The
    // hook checks for ~/.claude/triage/.alive; without this it bails out and
    // Claude's normal permission prompt takes over.
    let _alive = approval::AliveGuard::install();

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal()?;
    result
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

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = AppState::new();
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
                    if !app.filter_active {
                        app.clamp_selection();
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        let elapsed = last_refresh.elapsed();
        let due = elapsed >= REFRESH_INTERVAL;
        let triggered = watcher.as_ref().map(|w| w.drain()).unwrap_or(false);
        if due || (triggered && elapsed >= WATCHER_DEBOUNCE) {
            refresh(&mut app);
            last_refresh = Instant::now();
        }
    }
}

fn handle_key(app: &mut AppState, code: KeyCode, mods: KeyModifiers) -> bool {
    if app.filter_active {
        match code {
            KeyCode::Esc => {
                app.filter.clear();
                app.filter_active = false;
            }
            KeyCode::Enter => {
                app.filter_active = false;
            }
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Char(c) => {
                app.filter.push(c);
            }
            _ => {}
        }
        return true;
    }

    match code {
        KeyCode::Char('q') => return false,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Char(' ') => app.detail_open = !app.detail_open,
        KeyCode::Char('m') => {
            app.toggle_mute_selected();
        }
        KeyCode::Char('a') => match app.approval_mode {
            models::ApprovalMode::Hook => {
                if let Some(uuid) = app.oldest_pending_uuid() {
                    approval::approve(&uuid);
                    app.status_msg = Some("approved (hook)".to_string());
                } else {
                    app.status_msg =
                        Some("session not at a prompt (or hook not running)".to_string());
                }
            }
            models::ApprovalMode::Tmux => {
                let target = app
                    .selected_session()
                    .filter(|s| s.status == "waiting")
                    .and_then(|s| s.pane.as_ref().map(|p| p.target.clone()));
                match target {
                    Some(t) => match tmux::send_keys(&t, &["1", "Enter"]) {
                        Ok(()) => app.status_msg = Some(format!("approved → {t}")),
                        Err(e) => app.status_msg = Some(format!("approve failed: {e}")),
                    },
                    None => {
                        app.status_msg = Some("session not at a prompt (or no pane)".to_string());
                    }
                }
            }
        },
        KeyCode::Char('d') => match app.approval_mode {
            models::ApprovalMode::Hook => {
                if let Some(uuid) = app.oldest_pending_uuid() {
                    approval::deny(&uuid, "denied via triage");
                    app.status_msg = Some("denied (hook)".to_string());
                } else {
                    app.status_msg =
                        Some("session not at a prompt (or hook not running)".to_string());
                }
            }
            models::ApprovalMode::Tmux => {
                let target = app
                    .selected_session()
                    .filter(|s| s.status == "waiting")
                    .and_then(|s| s.pane.as_ref().map(|p| p.target.clone()));
                match target {
                    Some(t) => match tmux::send_keys(&t, &["Escape"]) {
                        Ok(()) => app.status_msg = Some(format!("denied → {t}")),
                        Err(e) => app.status_msg = Some(format!("deny failed: {e}")),
                    },
                    None => {
                        app.status_msg = Some("session not at a prompt (or no pane)".to_string());
                    }
                }
            }
        },
        KeyCode::Char('h') => {
            app.toggle_approval_mode();
            app.status_msg = Some(format!("approve mode: {}", app.approval_mode.label()));
        }
        KeyCode::Char('/') => {
            app.filter_active = true;
            app.filter.clear();
        }
        KeyCode::Char('r') => {
            app.status_msg = Some("refreshing…".to_string());
        }
        KeyCode::Enter => {
            if let Some(s) = app.selected_session() {
                if let Some(pane) = &s.pane {
                    let target = pane.target.clone();
                    match tmux::jump_to(&target) {
                        Ok(()) => app.status_msg = Some(format!("jumped → {target}")),
                        Err(e) => app.status_msg = Some(format!("jump failed: {e}")),
                    }
                } else {
                    app.status_msg = Some("no tmux pane for this session".to_string());
                }
            }
        }
        _ => {}
    }
    true
}

fn refresh(app: &mut AppState) {
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
    }
    if app.muted.len() != mute_count_before {
        app.persist_state();
    }

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
        notify_os::alert(s);
    }
    app.last_states = sessions.iter().map(|s| (s.pid, s.state)).collect();

    app.sessions = sessions;
    app.clamp_selection();
    app.status_msg = None;
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
