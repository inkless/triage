use std::sync::mpsc;
use std::time::SystemTime;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use std::collections::{HashMap, HashSet};

use crate::auditor::Verdict;
use crate::classifier::idle_age;
use crate::models::{ApprovalMode, AttentionState, Session};
use crate::persist::{self, MuteKey};
use crate::transcript::DigestCache;

pub struct AppState {
    pub sessions: Vec<Session>,
    pub selected: TableState,
    pub filter: String,
    pub filter_active: bool,
    pub detail_open: bool,
    pub status_msg: Option<String>,
    pub digest_cache: DigestCache,
    /// (cwd, started_at_ms) → time of mute. Keyed on a stable identity rather
    /// than pid so the entries survive a triage restart and don't accidentally
    /// re-mute a recycled pid.
    pub muted: HashMap<MuteKey, SystemTime>,
    /// pid → most recently observed AttentionState. Used to detect transitions
    /// (e.g. into `Blocked`) so we can fire a desktop notification once per
    /// transition rather than on every refresh while the session stays blocked.
    pub last_states: HashMap<u32, AttentionState>,
    /// Which mechanism `a`/`d` use to deliver an approval. Toggled with `h`.
    pub approval_mode: ApprovalMode,
    /// Autonomous mode (T-56). Toggle with `A`. When on, the refresh loop
    /// spawns a `claude -p` auditor for each `waiting` session and routes
    /// APPROVE/DENY through the same machinery as manual `a`/`d`.
    pub autonomous: bool,
    /// Pids whose auditor is currently running. Prevents double-firing on
    /// successive refresh ticks while a verdict is in flight.
    pub audit_in_flight: HashSet<u32>,
    /// (cwd, started_at_ms) → last decision the auditor returned for this
    /// stable session identity. Re-deciding on every tick would burn tokens;
    /// we only re-fire when the session leaves and re-enters `waiting`.
    pub audit_decided: HashSet<MuteKey>,
    /// Per-session verdict annotation, surfaced in detail view + status line.
    pub audit_notes: HashMap<u32, (SystemTime, String)>,
    /// Worker threads send completed verdicts here; `refresh()` drains.
    pub audit_tx: mpsc::Sender<Verdict>,
    pub audit_rx: mpsc::Receiver<Verdict>,
}

impl AppState {
    pub fn new() -> Self {
        let mut state = TableState::default();
        state.select(Some(0));
        let loaded = persist::load_state();
        let muted = loaded.mutes.into_iter().collect();
        let (audit_tx, audit_rx) = mpsc::channel();
        Self {
            sessions: Vec::new(),
            selected: state,
            filter: String::new(),
            filter_active: false,
            detail_open: false,
            status_msg: None,
            digest_cache: DigestCache::new(),
            muted,
            last_states: HashMap::new(),
            approval_mode: loaded.approval_mode,
            autonomous: loaded.autonomous,
            audit_in_flight: HashSet::new(),
            audit_decided: HashSet::new(),
            audit_notes: HashMap::new(),
            audit_tx,
            audit_rx,
        }
    }

    pub fn toggle_approval_mode(&mut self) {
        self.approval_mode = self.approval_mode.toggled();
        self.persist_state();
    }

    pub fn toggle_autonomous(&mut self) {
        self.autonomous = !self.autonomous;
        if !self.autonomous {
            // Drop the "already decided" set so re-enabling is fresh.
            self.audit_decided.clear();
        }
        self.persist_state();
    }

    pub fn oldest_pending_uuid(&self) -> Option<String> {
        self.selected_session()
            .filter(|s| s.status == "waiting")
            .and_then(|s| s.pending_approvals.first())
            .map(|a| a.uuid.clone())
    }

    pub fn toggle_mute_selected(&mut self) {
        let Some(s) = self.selected_session() else { return };
        let key = MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        if self.muted.remove(&key).is_none() {
            self.muted.insert(key, SystemTime::now());
        }
        self.persist_state();
    }

    pub fn persist_state(&self) {
        persist::save_state(self.muted.iter(), self.approval_mode, self.autonomous);
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
            Span::styled("triage", Style::default().add_modifier(Modifier::BOLD)),
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
            Span::styled("triage", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(counts, Style::default().fg(Color::DarkGray)),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Terminal-width tiers. Picked once per draw based on `area.width`.
/// `Narrow` is for phone-sized SSH (~40–60 cols), `Medium` is a split-screen
/// laptop window, `Wide` is the standard desktop layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    Narrow,
    Medium,
    Wide,
}

impl LayoutMode {
    fn from_width(w: u16) -> Self {
        if w < 60 {
            LayoutMode::Narrow
        } else if w < 100 {
            LayoutMode::Medium
        } else {
            LayoutMode::Wide
        }
    }
}

fn draw_table(f: &mut Frame, area: Rect, app: &mut AppState, now: SystemTime) {
    let visible = app.visible();
    let layout = LayoutMode::from_width(area.width);

    // Fixed = sum of non-headline column widths + per-column gap (1) + highlight indent (2).
    let (fixed, widths, header_cells): (usize, Vec<Constraint>, Vec<Cell>) = match layout {
        LayoutMode::Narrow => (
            7 + 1 + 2,
            vec![Constraint::Length(7), Constraint::Min(20)],
            vec![Cell::from("STATE"), Cell::from("HEADLINE")],
        ),
        LayoutMode::Medium => (
            7 + 5 + 16 + 3 + 2,
            vec![
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(16),
                Constraint::Min(20),
            ],
            vec![
                Cell::from("STATE"),
                Cell::from("AGE"),
                Cell::from("SESSION"),
                Cell::from("HEADLINE"),
            ],
        ),
        LayoutMode::Wide => (
            7 + 5 + 20 + 28 + 4 + 2,
            vec![
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(20),
                Constraint::Length(28),
                Constraint::Min(20),
            ],
            vec![
                Cell::from("STATE"),
                Cell::from("AGE"),
                Cell::from("SESSION"),
                Cell::from("CWD"),
                Cell::from("HEADLINE"),
            ],
        ),
    };

    let headline_width = (area.width as usize).saturating_sub(fixed).max(1);
    let rows: Vec<Row> = visible
        .iter()
        .map(|s| build_row(s, now, headline_width, layout))
        .collect();

    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ")
        .block(Block::default().borders(Borders::TOP));

    f.render_stateful_widget(table, area, &mut app.selected);
}

fn build_row(
    s: &Session,
    now: SystemTime,
    headline_width: usize,
    layout: LayoutMode,
) -> Row<'static> {
    let (state_str, color) = state_glyph(s.state);
    let age = idle_age(s, now)
        .map(format_duration)
        .unwrap_or_else(|| "—".to_string());

    // Order: Claude `/rename` (most deliberate, session-specific) → tmux
    // session name (workspace label the user actively chose) → cwd basename
    // (default). Tmux's auto-assigned numeric names ("0", "1", …) are skipped
    // because they're worse than the cwd basename for telling rows apart.
    let session_label = s
        .name
        .clone()
        .or_else(|| {
            s.pane
                .as_ref()
                .map(|p| p.tmux_session.as_str())
                .filter(|n| !n.is_empty() && !n.chars().all(|c| c.is_ascii_digit()))
                .map(|n| n.to_string())
        })
        .or_else(|| {
            s.cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "?".to_string());

    let cwd_short = shorten_path(&s.cwd.to_string_lossy(), 28);
    // Only show a permission headline when Claude itself is actually paused on
    // user input. Pending files alone are not enough: the hook also sees
    // auto-approved tool calls.
    let headline_raw = if s.status == "waiting" {
        if let Some(a) = s.pending_approvals.first() {
            if a.tool_input_brief.is_empty() {
                format!("⏸ approve? {}", a.tool_name)
            } else {
                format!("⏸ approve? {} — {}", a.tool_name, a.tool_input_brief)
            }
        // Prefer the actual tool_use (full Claude question) over `waitingFor`
        // (just "approve Bash"). Both come from Claude itself; the tool_use
        // is the same data the hook would have shown.
        } else if let Some((name, brief)) = &s.last_tool_use {
            if brief.is_empty() {
                format!("⏸ approve {name}?")
            } else {
                format!("⏸ approve {name}? — {brief}")
            }
        } else {
            let what = s.waiting_for.as_deref().unwrap_or("input");
            format!("⏸ {what}?")
        }
    } else {
        s.headline
            .clone()
            .or_else(|| s.last_prompt.clone())
            .map(|t| t.replace('\n', " "))
            .unwrap_or_else(|| "(no transcript)".to_string())
    };

    // Narrow layout has only STATE+HEADLINE columns, so prefix the headline
    // with session label + age — otherwise the user can't tell rows apart.
    let headline_raw = match layout {
        LayoutMode::Narrow => format!("{session_label}  {age}  · {headline_raw}"),
        _ => headline_raw,
    };

    let wrapped = wrap_text(&headline_raw, headline_width, 4);
    let height = wrapped.len().max(1) as u16;
    let headline_lines: Vec<Line> = wrapped.into_iter().map(Line::from).collect();

    // Muted rows render dimmed across all columns so the user's eye skips
    // them. The state glyph still shows its label but in a muted color.
    let (state_label, state_color) = if s.muted {
        ("muted".to_string(), Color::DarkGray)
    } else {
        (state_str, color)
    };
    let row_style = if s.muted {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let session_style = if s.muted {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let cwd_style = Style::default().fg(Color::DarkGray);

    let cells: Vec<Cell> = match layout {
        LayoutMode::Narrow => vec![
            Cell::from(state_label).style(Style::default().fg(state_color)),
            Cell::from(Text::from(headline_lines)),
        ],
        LayoutMode::Medium => vec![
            Cell::from(state_label).style(Style::default().fg(state_color)),
            Cell::from(age).style(cwd_style),
            Cell::from(session_label).style(session_style),
            Cell::from(Text::from(headline_lines)),
        ],
        LayoutMode::Wide => vec![
            Cell::from(state_label).style(Style::default().fg(state_color)),
            Cell::from(age).style(cwd_style),
            Cell::from(session_label).style(session_style),
            Cell::from(cwd_short).style(cwd_style),
            Cell::from(Text::from(headline_lines)),
        ],
    };

    Row::new(cells)
        .height(height)
        .bottom_margin(1)
        .style(row_style)
}

fn wrap_text(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_chars = 0usize;
    let mut truncated = false;

    for word in text.split_whitespace() {
        if out.len() >= max_lines {
            truncated = true;
            break;
        }
        let wlen = word.chars().count();
        let need = if line.is_empty() { wlen } else { line_chars + 1 + wlen };
        if need <= width {
            if !line.is_empty() {
                line.push(' ');
                line_chars += 1;
            }
            line.push_str(word);
            line_chars += wlen;
        } else {
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line_chars = 0;
                if out.len() >= max_lines {
                    truncated = true;
                    break;
                }
            }
            if wlen > width {
                let mut chars = word.chars().peekable();
                while chars.peek().is_some() {
                    let chunk: String = chars.by_ref().take(width).collect();
                    let chunk_len = chunk.chars().count();
                    if chunk_len < width || chars.peek().is_none() {
                        line = chunk;
                        line_chars = chunk_len;
                        break;
                    }
                    out.push(chunk);
                    if out.len() >= max_lines {
                        truncated = chars.peek().is_some();
                        break;
                    }
                }
            } else {
                line = word.to_string();
                line_chars = wlen;
            }
        }
    }
    if !line.is_empty() && out.len() < max_lines {
        out.push(line);
    }
    if truncated && !out.is_empty() {
        let last = out.last_mut().unwrap();
        let cap = width.saturating_sub(1);
        while last.chars().count() > cap {
            last.pop();
        }
        last.push('…');
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn state_glyph(state: AttentionState) -> (String, Color) {
    let color = match state {
        AttentionState::Error => Color::Red,
        AttentionState::Blocked => Color::Yellow,
        AttentionState::JustFinished => Color::Green,
        AttentionState::Working => Color::Cyan,
        AttentionState::Fresh => Color::White,
        AttentionState::IdleShort => Color::DarkGray,
        AttentionState::IdleLong => Color::DarkGray,
        AttentionState::Stale => Color::DarkGray,
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
    if let Some((_, note)) = app.audit_notes.get(&s.pid) {
        lines.push(Line::from(vec![
            Span::styled("auditor: ", Style::default().fg(Color::Magenta)),
            Span::raw(note.clone()),
        ]));
    }
    if let (Some(d), Some(c)) = (s.last_turn_duration_ms, s.last_turn_msg_count) {
        lines.push(Line::from(vec![
            Span::styled("last turn: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{}.{}s · {} msgs", d / 1000, (d % 1000) / 100, c)),
        ]));
    }
    // Surface what Claude is currently asking permission for. Headline shows
    // the same content, but here we render it untruncated and wrapped so the
    // user can see a long Bash command in full.
    if s.status == "waiting" {
        if let Some(a) = s.pending_approvals.first() {
            lines.push(Line::from(vec![
                Span::styled("pending: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    a.tool_name.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(a.tool_input_brief.clone()),
            ]));
        } else if let Some((name, brief)) = &s.last_tool_use {
            lines.push(Line::from(vec![
                Span::styled("pending: ", Style::default().fg(Color::DarkGray)),
                Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::raw(brief.clone()),
            ]));
        }
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
        let mode = app.approval_mode.label();
        let auto = if app.autonomous {
            let n = app.audit_in_flight.len();
            if n > 0 { format!("AUTO·{n}") } else { "AUTO".to_string() }
        } else {
            "auto:off".to_string()
        };
        let hint = match LayoutMode::from_width(area.width) {
            LayoutMode::Narrow => format!(" ⏎ a d h:{mode} A:{auto} / q"),
            LayoutMode::Medium => format!(
                " ⏎ jump  a/d  h [{mode}]  A [{auto}]  m mute  / filter  q"
            ),
            LayoutMode::Wide => format!(
                "  ↑↓ select  ⏎ jump  / filter  space detail  a approve  d deny  h mode [{mode}]  A auto [{auto}]  m mute  r refresh  q quit"
            ),
        };
        let style = if app.autonomous {
            Style::default().fg(Color::Magenta)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Line::from(Span::styled(hint, style))
    };
    f.render_widget(Paragraph::new(line), area);
}
