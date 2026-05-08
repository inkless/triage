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
    /// Pids whose auditor is currently running, keyed to the SystemTime the
    /// worker thread was spawned. Prevents double-firing on successive refresh
    /// ticks while a verdict is in flight; the timestamp drives the
    /// "auditor running… (Xs)" indicator in the detail view.
    pub audit_in_flight: HashMap<u32, SystemTime>,
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
            audit_in_flight: HashMap::new(),
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
            Constraint::Length(if app.detail_open { 18 } else { 0 }), // detail
            Constraint::Length(1),                                    // footer
        ])
        .split(f.area());

    draw_header(f, chunks[0], app);
    draw_table(f, chunks[1], app, now);
    if app.detail_open {
        draw_detail(f, chunks[2], app, now);
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

fn draw_detail(f: &mut Frame, area: Rect, app: &AppState, now: SystemTime) {
    let Some(s) = app.selected_session() else {
        return;
    };

    let dim = || Style::default().fg(Color::DarkGray);
    let bold = || Style::default().add_modifier(Modifier::BOLD);
    let mag = || Style::default().fg(Color::Magenta);
    let yellow = || Style::default().fg(Color::Yellow);

    let mut lines = Vec::new();

    // Row 1: pane / pid / status / state. Hardest-to-derive identity first.
    let pane_target = s
        .pane
        .as_ref()
        .map(|p| p.target.clone())
        .unwrap_or_else(|| "(none)".to_string());
    lines.push(Line::from(vec![
        Span::styled("pane: ", dim()),
        Span::raw(pane_target),
        Span::styled("  pid: ", dim()),
        Span::raw(s.pid.to_string()),
        Span::styled("  status: ", dim()),
        Span::raw(s.status.clone()),
        Span::styled("  state: ", dim()),
        Span::raw(s.state.label().to_string()),
    ]));

    // Row 2: short session id, uptime, approve mode, mute flag.
    let short_id: String = s.session_id.chars().take(8).collect();
    let uptime = format_uptime(s.started_at_ms, now);
    let mut row2 = vec![
        Span::styled("session: ", dim()),
        Span::raw(short_id),
        Span::styled("  uptime: ", dim()),
        Span::raw(uptime),
        Span::styled("  mode: ", dim()),
        Span::raw(app.approval_mode.label().to_string()),
    ];
    if s.muted {
        row2.push(Span::styled("  [muted]", yellow()));
    }
    lines.push(Line::from(row2));

    // Auditor: in-flight indicator OR the most recent decision.
    if let Some(start) = app.audit_in_flight.get(&s.pid) {
        let secs = now.duration_since(*start).map(|d| d.as_secs()).unwrap_or(0);
        lines.push(Line::from(vec![
            Span::styled("auditor: ", mag()),
            Span::styled("running…", mag().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  ({}s)", secs), dim()),
        ]));
    } else if let Some((ts, note)) = app.audit_notes.get(&s.pid) {
        let ago = now.duration_since(*ts).map(|d| d.as_secs()).unwrap_or(0);
        lines.push(Line::from(vec![
            Span::styled("auditor: ", mag()),
            Span::raw(note.clone()),
            Span::styled(format!("  ({}s ago)", ago), dim()),
        ]));
    }

    // Event timing — useful to tell "actively progressing" from "stuck".
    let events_line = format!(
        "last {} · prompt {} · stop {}",
        format_age_opt(s.last_event_at, now),
        format_age_opt(s.last_prompt_at, now),
        format_age_opt(s.last_stop_at, now),
    );
    lines.push(Line::from(vec![
        Span::styled("events: ", dim()),
        Span::raw(events_line),
    ]));

    // Last turn timing (when both fields are populated).
    if let (Some(d), Some(c)) = (s.last_turn_duration_ms, s.last_turn_msg_count) {
        let mut spans = vec![
            Span::styled("last turn: ", dim()),
            Span::raw(format!("{}.{}s · {} msgs", d / 1000, (d % 1000) / 100, c)),
        ];
        if s.user_prompt_count > 0 {
            spans.push(Span::styled(
                format!("  ·  {} prompts total", s.user_prompt_count),
                dim(),
            ));
        }
        lines.push(Line::from(spans));
    }

    // Approximate session cost. Computed from per-message usage × per-model
    // rates; cross-check against `/cost` slash command for the canonical
    // figure. We show it here because in this view it's already focused —
    // saves bouncing into the pane to ask Claude itself.
    if s.total_cost_usd > 0.0 || s.total_tokens_in > 0 || s.total_tokens_out > 0 {
        lines.push(Line::from(vec![
            Span::styled("cost: ", dim()),
            Span::raw(format_cost(s.total_cost_usd)),
            Span::styled("  ·  ", dim()),
            Span::raw(format!(
                "{} in / {} out / {} cache",
                format_tokens(s.total_tokens_in),
                format_tokens(s.total_tokens_out),
                format_tokens(s.total_tokens_cache_write + s.total_tokens_cache_read),
            )),
            Span::styled("  (approx)", dim()),
        ]));
    }

    // Context-window occupancy. Most claude models cap at 200k; we hardcode
    // that for the percentage display since the variant tag (e.g. 1M Sonnet)
    // isn't in the transcript message's `model` field.
    if s.latest_context_tokens > 0 {
        let window = context_window_for(s.latest_model.as_deref());
        let pct = (s.latest_context_tokens as f64 / window as f64) * 100.0;
        let pct_color = if pct >= 95.0 {
            Color::Red
        } else if pct >= 80.0 {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        lines.push(Line::from(vec![
            Span::styled("context: ", dim()),
            Span::raw(format!(
                "{} / {}",
                format_tokens(s.latest_context_tokens),
                format_tokens(window),
            )),
            Span::styled("  ", dim()),
            Span::styled(format!("({:.0}%)", pct), Style::default().fg(pct_color)),
        ]));
    }

    // Most recent assistant text (Claude's explanation, often immediately
    // before the pending tool_use). Show before "pending:" so the reader
    // gets the *why* before the *what*. Skip when it duplicates the recap
    // (rare but possible if the headline is the same text).
    if let Some(t) = &s.latest_assistant_text
        && s.headline.as_ref().is_none_or(|h| h.trim() != t.trim())
    {
        lines.push(Line::from(vec![
            Span::styled("agent: ", dim()),
            Span::raw(truncate(t, 300)),
        ]));
    }

    // Pending tool input. Two paths depending on capture source:
    //   - Hook (richer): full `tool_input_full` JSON, parsed into a per-tool
    //     pretty rendering (Bash command with real newlines, Edit path + diff).
    //   - Tmux scrape (truncated): show the brief verbatim with a "(tmux
    //     scrape, may be truncated)" tag so the user knows why the auditor
    //     might WAIT on this row.
    if s.status == "waiting" {
        if let Some(a) = s.pending_approvals.first() {
            lines.push(Line::from(vec![
                Span::styled("pending: ", yellow()),
                Span::styled(a.tool_name.clone(), bold()),
                Span::styled("  (hook)", dim()),
            ]));
            for line in a.tool_input_detail().lines().take(6) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(truncate(line, 400)),
                ]));
            }
        } else if let Some((name, brief)) = &s.last_tool_use {
            lines.push(Line::from(vec![
                Span::styled("pending: ", yellow()),
                Span::styled(name.clone(), bold()),
                Span::styled("  (tmux scrape, may be truncated)", dim()),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(truncate(brief, 400)),
            ]));
        }
    }

    if let Some(p) = &s.last_prompt {
        lines.push(Line::from(vec![
            Span::styled("last prompt: ", dim()),
            Span::raw(truncate(p, 200)),
        ]));
    }
    if let Some(h) = &s.headline {
        lines.push(Line::from(vec![
            Span::styled("recap: ", dim()),
            Span::raw(truncate(h, 400)),
        ]));
    }

    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(" detail ", dim()));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn format_age_opt(ts: Option<SystemTime>, now: SystemTime) -> String {
    let Some(ts) = ts else { return "—".to_string() };
    let Ok(d) = now.duration_since(ts) else {
        return "—".to_string();
    };
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

fn format_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${:.2}", usd)
    } else if usd >= 0.01 {
        format!("${:.3}", usd)
    } else if usd > 0.0 {
        format!("${:.5}", usd)
    } else {
        "$0".to_string()
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Context-window size by model family. All current 4.x models (Opus, Sonnet,
/// Haiku) ship with a 200k default. Long-context Sonnet (1M) is gated behind
/// a beta header that doesn't show up in the transcript's `model` field, so
/// we'd just under-report on those — accept the imprecision; the percentage
/// would simply read >100% to flag "you have more headroom than this shows."
fn context_window_for(_model: Option<&str>) -> u64 {
    200_000
}

fn format_uptime(started_at_ms: u64, now: SystemTime) -> String {
    if started_at_ms == 0 {
        return "—".to_string();
    }
    let Ok(now_ms) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return "—".to_string();
    };
    let now_ms = now_ms.as_millis() as u64;
    if now_ms <= started_at_ms {
        return "0s".to_string();
    }
    let s = (now_ms - started_at_ms) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{}h", s / 86400, (s % 86400) / 3600)
    }
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
