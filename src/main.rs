mod classifier;
mod discovery;
mod models;
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

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--probe") {
        return probe();
    }

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
    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, 8);
        transcript::enrich(s);
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

        let due = last_refresh.elapsed() >= REFRESH_INTERVAL;
        let triggered = watcher.as_ref().map(|w| w.drain()).unwrap_or(false);
        if due || triggered {
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

    for s in &mut sessions {
        s.pane = tmux::find_owning_pane(s.pid, &panes, 8);
        transcript::enrich(s);
        s.state = classifier::classify(s, SystemTime::now());
    }

    sessions.sort_by(|a, b| {
        a.state
            .priority()
            .cmp(&b.state.priority())
            .then_with(|| a.cwd.cmp(&b.cwd))
    });

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
