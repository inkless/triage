use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::classifier::idle_age;
use crate::models::{AttentionState, Session};

pub struct AppState {
    pub sessions: Vec<Session>,
    pub selected: TableState,
    pub filter: String,
    pub filter_active: bool,
    pub detail_open: bool,
    pub status_msg: Option<String>,
}

impl AppState {
    pub fn new() -> Self {
        let mut state = TableState::default();
        state.select(Some(0));
        Self {
            sessions: Vec::new(),
            selected: state,
            filter: String::new(),
            filter_active: false,
            detail_open: false,
            status_msg: None,
        }
    }

    pub fn visible(&self) -> Vec<&Session> {
        if self.filter.is_empty() {
            return self.sessions.iter().collect();
        }
        let q = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| {
                s.cwd.to_string_lossy().to_lowercase().contains(&q)
                    || s.name
                        .as_deref()
                        .map(|n| n.to_lowercase().contains(&q))
                        .unwrap_or(false)
                    || s.headline
                        .as_deref()
                        .map(|h| h.to_lowercase().contains(&q))
                        .unwrap_or(false)
                    || s.pane
                        .as_ref()
                        .map(|p| p.target.to_lowercase().contains(&q))
                        .unwrap_or(false)
            })
            .collect()
    }

    pub fn selected_session(&self) -> Option<&Session> {
        let v = self.visible();
        let idx = self.selected.selected()?;
        v.get(idx).copied()
    }

    pub fn move_selection(&mut self, delta: i32) {
        let len = self.visible().len();
        if len == 0 {
            self.selected.select(None);
            return;
        }
        let cur = self.selected.selected().unwrap_or(0) as i32;
        let next = (cur + delta).rem_euclid(len as i32) as usize;
        self.selected.select(Some(next));
    }

    pub fn clamp_selection(&mut self) {
        let len = self.visible().len();
        if len == 0 {
            self.selected.select(None);
        } else if self.selected.selected().unwrap_or(0) >= len {
            self.selected.select(Some(len - 1));
        } else if self.selected.selected().is_none() {
            self.selected.select(Some(0));
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut AppState, now: SystemTime) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                                    // header
            Constraint::Min(5),                                       // table
            Constraint::Length(if app.detail_open { 8 } else { 0 }),  // detail
            Constraint::Length(1),                                    // footer
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app, now);
    if app.detail_open {
        draw_detail(f, chunks[2], app);
    }
    draw_footer(f, chunks[3], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &AppState) {
    let line = if app.filter_active {
        Line::from(vec![
            Span::styled("agent-orchestrator", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   filter: "),
            Span::styled(&app.filter, Style::default().fg(Color::Yellow)),
            Span::styled("_", Style::default().fg(Color::Yellow).add_modifier(Modifier::SLOW_BLINK)),
        ])
    } else {
        let count = app.visible().len();
        let total = app.sessions.len();
        let counts = if app.filter.is_empty() {
            format!("{total} session{}", if total == 1 { "" } else { "s" })
        } else {
            format!("{count}/{total} sessions  (filter: {})", app.filter)
        };
        Line::from(vec![
            Span::styled("agent-orchestrator", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(counts, Style::default().fg(Color::DarkGray)),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut AppState, now: SystemTime) {
    let visible = app.visible();
    let rows: Vec<Row> = visible
        .iter()
        .map(|s| build_row(s, now))
        .collect();

    let widths = [
        Constraint::Length(7),    // STATE
        Constraint::Length(5),    // AGE
        Constraint::Length(20),   // SESSION
        Constraint::Length(28),   // CWD
        Constraint::Min(20),      // HEADLINE
    ];

    let header = Row::new(vec![
        Cell::from("STATE"),
        Cell::from("AGE"),
        Cell::from("SESSION"),
        Cell::from("CWD"),
        Cell::from("HEADLINE"),
    ])
    .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ")
        .block(Block::default().borders(Borders::TOP));

    f.render_stateful_widget(table, area, &mut app.selected);
}

fn build_row(s: &Session, now: SystemTime) -> Row<'static> {
    let (state_str, color) = state_glyph(s.state);
    let age = idle_age(s, now)
        .map(format_duration)
        .unwrap_or_else(|| "—".to_string());

    let session_label = s
        .name
        .clone()
        .or_else(|| {
            s.cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "?".to_string());

    let cwd_short = shorten_path(&s.cwd.to_string_lossy(), 28);
    let headline = s
        .headline
        .clone()
        .or_else(|| s.last_prompt.clone())
        .map(|t| t.replace('\n', " "))
        .unwrap_or_else(|| "(no transcript)".to_string());

    Row::new(vec![
        Cell::from(state_str).style(Style::default().fg(color)),
        Cell::from(age),
        Cell::from(session_label),
        Cell::from(cwd_short),
        Cell::from(headline),
    ])
}

fn state_glyph(state: AttentionState) -> (String, Color) {
    let color = match state {
        AttentionState::Error => Color::Red,
        AttentionState::JustFinished => Color::Green,
        AttentionState::Working => Color::Cyan,
        AttentionState::Fresh => Color::White,
        AttentionState::IdleShort => Color::DarkGray,
        AttentionState::IdleLong => Color::DarkGray,
        AttentionState::Unknown => Color::DarkGray,
    };
    (state.label().to_string(), color)
}

fn format_duration(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

fn shorten_path(path: &str, max: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if p.len() <= max {
        return p;
    }
    let cut = p.len() - (max - 1);
    format!("…{}", &p[cut..])
}

fn draw_detail(f: &mut Frame, area: Rect, app: &AppState) {
    let Some(s) = app.selected_session() else {
        return;
    };

    let mut lines = Vec::new();
    if let Some(pane) = &s.pane {
        lines.push(Line::from(vec![
            Span::styled("pane: ", Style::default().fg(Color::DarkGray)),
            Span::raw(pane.target.clone()),
            Span::styled("  pid: ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.pid.to_string()),
            Span::styled("  status: ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.status.clone()),
        ]));
    }
    if let (Some(d), Some(c)) = (s.last_turn_duration_ms, s.last_turn_msg_count) {
        lines.push(Line::from(vec![
            Span::styled("last turn: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}.{}s · {} msgs", d / 1000, (d % 1000) / 100, c)),
        ]));
    }
    if let Some(p) = &s.last_prompt {
        lines.push(Line::from(vec![
            Span::styled("last prompt: ", Style::default().fg(Color::DarkGray)),
            Span::raw(truncate(p, 200)),
        ]));
    }
    if let Some(h) = &s.headline {
        lines.push(Line::from(vec![
            Span::styled("recap: ", Style::default().fg(Color::DarkGray)),
            Span::raw(truncate(h, 400)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(" detail ", Style::default().fg(Color::DarkGray)));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn truncate(s: &str, n: usize) -> String {
    let cleaned = s.replace('\n', " ");
    if cleaned.chars().count() <= n {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn draw_footer(f: &mut Frame, area: Rect, app: &AppState) {
    let line = if app.filter_active {
        Line::from(Span::styled(
            "  [enter] apply  [esc] cancel",
            Style::default().fg(Color::DarkGray),
        ))
    } else if let Some(msg) = &app.status_msg {
        Line::from(Span::styled(msg.clone(), Style::default().fg(Color::Yellow)))
    } else {
        Line::from(Span::styled(
            "  ↑↓ select  ⏎ jump  / filter  space detail  r refresh  q quit",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}
