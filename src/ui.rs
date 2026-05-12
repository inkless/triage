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
use crate::config::Config;
use crate::models::{ApprovalMode, AttentionState, Session};
use crate::persist::{self, MuteKey};
use crate::transcript::DigestCache;

pub struct AppState {
    pub sessions: Vec<Session>,
    pub selected: TableState,
    pub detail_open: bool,
    pub status_msg: Option<String>,
    pub digest_cache: DigestCache,
    /// (cwd, started_at_ms) → time of mute. Keyed on a stable identity rather
    /// than pid so the entries survive a triage restart and don't accidentally
    /// re-mute a recycled pid.
    pub muted: HashMap<MuteKey, SystemTime>,
    /// In-memory set of sessions to fire a "finished" notification for on each
    /// transition into `JustFinished` (T-81). Sticky — only the user can
    /// clear an entry by pressing `w` again on the row. Not persisted across
    /// restarts; a watch only makes sense while the session exists.
    pub watched: HashSet<MuteKey>,
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
    /// ntfy phone push gate (T-79). Toggle with `p`. When off, app-driven
    /// ntfy POSTs are suppressed — Mac local banners still fire. The
    /// `triage notify` CLI (T-77) bypasses this gate (user-initiated pings
    /// are explicit and always pass through).
    pub phone_push_enabled: bool,
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
    /// User's default model from `~/.claude/settings.json` (e.g. `"opus[1m]"`).
    /// Read once at startup; the only deterministic source of the variant tag
    /// since the transcript's per-message `model` field strips `[1m]`.
    pub default_model: Option<String>,
    /// When true, the main area renders the audit-history overlay instead of
    /// the session table. Toggle with `H` (only effective when autonomous
    /// mode is on — there's no history to look at otherwise).
    pub audit_log_open: bool,
    /// Scroll offset into the audit log (0 = newest entry at top).
    pub audit_log_offset: u16,
    /// Total content-line count of the audit overlay (set by `draw_audit_log`
    /// each draw). Used to clamp scroll on `j`/`Ctrl-D` and to compute the
    /// `G`/bottom target.
    pub audit_log_total_lines: u16,
    /// Vim chord state: `g` waits for a follow-up `g` to mean "top". Reset
    /// by any other key. Inside the audit-log overlay only.
    pub pending_g: bool,
    /// Cached parse of the audit log, keyed on file (mtime, size). Avoids
    /// re-parsing the whole JSONL on every draw — most draws happen with
    /// no new audit, so the cache hit rate is high.
    pub audit_log_cache: Option<(SystemTime, u64, Vec<serde_json::Value>)>,
    /// When true, triage exits cleanly after a successful `Enter` jump. Set
    /// by `--exit-on-jump`. Designed for the tmux popup launch pattern: the
    /// popup closes when triage exits, so a single keypress (`Enter`) both
    /// jumps to the target pane AND dismisses the overlay. Without this,
    /// the popup would stay open showing triage and the user would have to
    /// press `q` afterwards. Implies `zoom_on_jump` (popup is small, you
    /// always want the target zoomed).
    pub exit_on_jump: bool,
    /// Force zoom-on-jump regardless of pane width. Set by `--zoom-on-jump`.
    /// Most users don't need this — Enter auto-zooms when `last_pane_width <
    /// MOBILE_WIDTH_THRESHOLD`, so mobile clients (which resize tmux panes
    /// to phone-narrow) get the right behavior automatically. The flag is
    /// for "I want zoom even on a wide pane" overrides.
    pub zoom_on_jump: bool,
    /// Most recent table-area width from ratatui's draw cycle. Just for the
    /// header indicator — NOT the auto-zoom signal anymore (laptop split-
    /// screen makes pane width narrow even on a desktop terminal). See
    /// `last_client_width` for the actual zoom decision.
    pub last_pane_width: u16,
    /// Most recent tmux `client_width` reading. This is the actual
    /// terminal/device width, not the pane subset. Refreshed once per
    /// `refresh()` tick (cheap — single tmux subprocess). Zero when not
    /// in tmux or query failed.
    pub last_client_width: u16,
    /// Loaded once at startup (config file + env overrides). Read-only
    /// after this point.
    pub config: Config,
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
            detail_open: false,
            status_msg: None,
            digest_cache: DigestCache::new(),
            muted,
            watched: HashSet::new(),
            last_states: HashMap::new(),
            approval_mode: loaded.approval_mode,
            autonomous: loaded.autonomous,
            phone_push_enabled: loaded.phone_push_enabled,
            audit_in_flight: HashMap::new(),
            audit_decided: HashSet::new(),
            audit_notes: HashMap::new(),
            audit_tx,
            audit_rx,
            default_model: crate::approval::read_default_model(),
            audit_log_open: false,
            audit_log_offset: 0,
            audit_log_total_lines: 0,
            pending_g: false,
            audit_log_cache: None,
            exit_on_jump: false,
            zoom_on_jump: false,
            last_pane_width: 0,
            last_client_width: 0,
            config: Config::default(),
        }
    }

    pub fn toggle_audit_log(&mut self) -> bool {
        if !self.autonomous && !self.audit_log_open {
            // Don't open the log when auto mode is off — there's nothing
            // useful to look at, and the empty panel would be confusing.
            return false;
        }
        self.audit_log_open = !self.audit_log_open;
        if !self.audit_log_open {
            self.audit_log_offset = 0;
            self.pending_g = false;
        }
        true
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

    pub fn toggle_phone_push(&mut self) {
        self.phone_push_enabled = !self.phone_push_enabled;
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

    /// Toggle watch on the selected session. Returns the new state + a label
    /// so the caller can build a status message. Watches are in-memory; no
    /// `persist_state` call.
    pub fn toggle_watch_selected(&mut self) -> Option<(bool, String)> {
        let s = self.selected_session()?;
        let key = MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        let label = s
            .name
            .clone()
            .or_else(|| s.cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "session".to_string());
        let now_watching = if self.watched.remove(&key) {
            false
        } else {
            self.watched.insert(key);
            true
        };
        Some((now_watching, label))
    }

    pub fn persist_state(&self) {
        persist::save_state(
            self.muted.iter(),
            self.approval_mode,
            self.autonomous,
            self.phone_push_enabled,
        );
    }

    /// Decide whether `Enter` should zoom the destination pane. Three sources:
    /// `--exit-on-jump` (popup mode, always wants zoom), `--zoom-on-jump`
    /// (explicit force-on), and auto-detect by current pane width. Auto-
    /// detect is the path most users hit — running triage with no flag still
    /// "does the right thing" on a phone-narrow client.
    pub fn should_zoom_on_jump(&self) -> bool {
        self.exit_on_jump
            || self.zoom_on_jump
            || (self.last_client_width > 0
                && self.last_client_width < self.config.thresholds.mobile_width)
    }

    pub fn visible(&self) -> Vec<&Session> {
        self.sessions.iter().collect()
    }

    /// Vim `gg` — jump to the first visible row.
    pub fn select_first(&mut self) {
        if !self.visible().is_empty() {
            self.selected.select(Some(0));
        }
    }

    /// Vim `G` — jump to the last visible row.
    pub fn select_last(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected.select(Some(n - 1));
        }
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
    if app.audit_log_open {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)])
            .split(f.area());
        draw_header(f, chunks[0], app);
        draw_audit_log(f, chunks[1], app, now);
        // draw_footer borrows app immutably — clone the small bits we need
        // and let it run after draw_audit_log writes back its line count.
        draw_footer(f, chunks[2], app);
        return;
    }
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
    let total = app.sessions.len();
    let counts = format!("{total} session{}", if total == 1 { "" } else { "s" });
    // Show pane width inline + a `zoom` indicator when auto-detect is active,
    // so it's obvious whether Enter will zoom on the current device.
    let zoom_marker = if app.should_zoom_on_jump() { " · zoom" } else { "" };
    // Show pane width (left, what ratatui drew into) and client width
    // (right, what tmux says the terminal is). The client width drives
    // auto-zoom; pane being narrow alone doesn't.
    let dims = if app.last_client_width > 0 && app.last_client_width != app.last_pane_width {
        format!(
            "{}cols (pane) / {}cols (client){zoom_marker}",
            app.last_pane_width, app.last_client_width
        )
    } else {
        format!("{}cols{zoom_marker}", app.last_pane_width)
    };
    let line = Line::from(vec![
        Span::styled("triage", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(counts, Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
        Span::styled(dims, Style::default().fg(Color::DarkGray)),
    ]);
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
    // Stash for the Enter handler's auto-zoom decision (see `should_zoom_on_jump`).
    // Set before borrowing `visible` from app to avoid an aliasing conflict.
    app.last_pane_width = area.width;
    let visible = app.visible();
    let selected_idx = app.selected.selected();
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
        .enumerate()
        .map(|(i, s)| build_row(s, now, headline_width, layout, Some(i) == selected_idx))
        .collect();

    let header = Row::new(header_cells)
        .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD));

    // Selected row gets uniform REVERSED on top of cells that have already
    // been rendered with neutralized colors (see `build_row` `is_selected`
    // path). REVERSED flips fg/bg per-cell — when each cell starts from the
    // same default fg, the result is a single uniform band rather than the
    // multicolor ribbon we got when each cell had its own color (green
    // state / DarkGray age / bold-white session / DarkGray cwd / default
    // headline). REVERSED was picked over a literal bg() to avoid painting
    // the bottom_margin gap.
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌ ")
        .block(Block::default().borders(Borders::TOP));

    f.render_stateful_widget(table, area, &mut app.selected);
}

fn build_row(
    s: &Session,
    now: SystemTime,
    headline_width: usize,
    layout: LayoutMode,
    is_selected: bool,
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
    let mut headline_lines: Vec<Line> = wrapped.into_iter().map(Line::from).collect();
    // T-81: prepend a bold cyan ● on the first line of watched rows. Span
    // approach (vs string prefix) keeps the wrap correct and gives the
    // marker its own color independent of row state.
    if s.watched && !headline_lines.is_empty() {
        let first_line = std::mem::take(&mut headline_lines[0]);
        let mut spans = vec![Span::styled(
            "● ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(first_line.spans);
        headline_lines[0] = Line::from(spans);
    }

    // Muted rows render dimmed across all columns so the user's eye skips
    // them. The state glyph still shows its label but in a muted color.
    // Watched rows append `·w` to the state label so the user can see the
    // arm-state in the column they're already scanning for state changes.
    let (state_label, state_color) = if s.muted {
        ("muted".to_string(), Color::DarkGray)
    } else if s.watched {
        (format!("{state_str}·w"), color)
    } else {
        (state_str, color)
    };
    let row_style = if s.muted {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    // Selected rows neutralize per-cell fg so the table-level REVERSED
    // applies as one uniform band instead of flipping each cell's color
    // (green state → green band, DarkGray cwd → gray band, etc.). Muted
    // rows still keep their DarkGray dimming even when selected so the
    // "I've seen this, skip it" visual contract holds across selection.
    let state_cell_style = if is_selected && !s.muted {
        Style::default()
    } else {
        Style::default().fg(state_color)
    };
    let session_style = if s.muted {
        Style::default().fg(Color::DarkGray)
    } else if is_selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let cwd_style = if is_selected && !s.muted {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let cells: Vec<Cell> = match layout {
        LayoutMode::Narrow => vec![
            Cell::from(state_label).style(state_cell_style),
            Cell::from(Text::from(headline_lines)),
        ],
        LayoutMode::Medium => vec![
            Cell::from(state_label).style(state_cell_style),
            Cell::from(age).style(cwd_style),
            Cell::from(session_label).style(session_style),
            Cell::from(Text::from(headline_lines)),
        ],
        LayoutMode::Wide => vec![
            Cell::from(state_label).style(state_cell_style),
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

    let mut lines: Vec<Line> = Vec::new();

    // ── HEADER ────────────────────────────────────────────────────
    // Single line, no labels. State (colored) · pane · uptime · mode.
    // Drop pid / session_id / status — all debug-tier; can be recovered by
    // grepping transcripts. State is the canonical "what's this row doing"
    // signal; status is redundant.
    let (state_str, state_color) = state_glyph(s.state);
    let pane_target = s
        .pane
        .as_ref()
        .map(|p| p.target.clone())
        .unwrap_or_else(|| "(no pane)".to_string());
    let uptime = format_uptime(s.started_at_ms, now);
    let sep = || Span::styled("  ·  ", dim());
    // Compute the context window once up front so both the header model
    // annotation and the stats line use the same value.
    let app_peak = app
        .sessions
        .iter()
        .map(|s| s.peak_context_tokens)
        .max()
        .unwrap_or(0);
    let window = context_window_for_session(
        s,
        app_peak,
        app.default_model.as_deref(),
        app.config.model.context_window,
    );
    let is_1m = window >= 1_000_000;

    let mut header = vec![
        Span::styled(
            state_str,
            Style::default().fg(state_color).add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(pane_target, bold()),
    ];
    // Model label: prefer the per-message `model` (most precise — includes
    // the version, e.g. `opus-4-7`); fall back to the user's global default
    // from settings.json (e.g. `opus[1m]`). Strip the `claude-` prefix and
    // append ` (1M)` when we've confirmed the 1M variant via any signal.
    let model_raw = s.latest_model.as_deref().or(app.default_model.as_deref());
    if let Some(m) = model_raw {
        let short = m.strip_prefix("claude-").unwrap_or(m);
        let label = if is_1m && !short.contains("[1m]") && !short.contains("(1M)") {
            format!("{} (1M)", short)
        } else {
            short.to_string()
        };
        header.push(sep());
        header.push(Span::styled(label, dim()));
    }
    header.push(sep());
    header.push(Span::styled(uptime, dim()));
    header.push(sep());
    header.push(Span::styled(
        format!("{} mode", app.approval_mode.label()),
        dim(),
    ));
    if s.muted {
        header.push(sep());
        header.push(Span::styled("[muted]", yellow()));
    }
    if s.watched {
        header.push(sep());
        header.push(Span::styled("[watch]", Style::default().fg(Color::Cyan)));
    }
    lines.push(Line::from(header));

    // ── BODY ──────────────────────────────────────────────────────
    // Most actionable content first: what the agent is doing/saying and
    // what tool it's asking permission for. Blank line separates header
    // from body so the eye can land on it.
    let body_start_idx = lines.len();

    if let Some(t) = &s.latest_assistant_text
        && s.headline.as_ref().is_none_or(|h| h.trim() != t.trim())
    {
        lines.push(Line::from(vec![
            Span::styled("agent  ", dim()),
            Span::raw(truncate(t, 300)),
        ]));
    }

    if s.status == "waiting" {
        if let Some(a) = s.pending_approvals.first() {
            lines.push(Line::from(vec![
                Span::styled(
                    a.tool_name.clone(),
                    yellow().add_modifier(Modifier::BOLD),
                ),
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
                Span::styled(name.clone(), yellow().add_modifier(Modifier::BOLD)),
                Span::styled("  (tmux scrape, may be truncated)", dim()),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::raw(truncate(brief, 400)),
            ]));
        }
    }

    if let Some(h) = &s.headline {
        lines.push(Line::from(vec![
            Span::styled("recap  ", dim()),
            Span::raw(truncate(h, 400)),
        ]));
    }
    if let Some(p) = &s.last_prompt {
        lines.push(Line::from(vec![
            Span::styled("prompt ", dim()),
            Span::raw(truncate(p, 200)),
        ]));
    }

    // Insert a spacer between header and body if body has content. Doing
    // it after construction (rather than always pushing) keeps the layout
    // tight when a session has nothing actionable to show.
    if lines.len() > body_start_idx {
        lines.insert(body_start_idx, Line::from(""));
    }

    // ── FOOTER ────────────────────────────────────────────────────
    // Auditor decision + numeric stats consolidated at the bottom. All
    // dim by default so the eye reads them as metadata, not content.

    let footer_start_idx = lines.len();

    if let Some(start) = app.audit_in_flight.get(&s.pid) {
        let secs = now.duration_since(*start).map(|d| d.as_secs()).unwrap_or(0);
        lines.push(Line::from(vec![
            Span::styled("audit  ", dim()),
            Span::styled(format!("running ({}s)", secs), mag()),
        ]));
    } else if let Some((ts, note)) = app.audit_notes.get(&s.pid) {
        let ago = now.duration_since(*ts).map(|d| d.as_secs()).unwrap_or(0);
        lines.push(Line::from(vec![
            Span::styled("audit  ", dim()),
            Span::styled(note.clone(), mag()),
            Span::styled(format!("  ({}s ago)", ago), dim()),
        ]));
    }

    // Cost + context + tokens — all on one line so cost data scans as a
    // single unit. Skip when there's nothing to show (fresh session).
    if s.total_cost_usd > 0.0 || s.total_tokens_in > 0 {
        let mut stats: Vec<Span> = vec![
            Span::styled("cost   ", dim()),
            Span::raw(format_cost(s.total_cost_usd)),
        ];
        if s.latest_context_tokens > 0 {
            // `window` was computed once up front for the header's (1M)
            // annotation; reuse it here so header and stats agree.
            let pct = (s.latest_context_tokens as f64 / window as f64) * 100.0;
            let pct_color = if pct >= 95.0 {
                Color::Red
            } else if pct >= 80.0 {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            stats.push(sep());
            stats.push(Span::raw(format!(
                "{}/{}",
                format_tokens(s.latest_context_tokens),
                format_tokens(window),
            )));
            stats.push(Span::styled(
                format!(" ({:.0}%)", pct),
                Style::default().fg(pct_color),
            ));
            stats.push(Span::styled(" ctx", dim()));
        }
        stats.push(sep());
        stats.push(Span::styled(
            format!(
                "{} in · {} out · {} cache",
                format_tokens(s.total_tokens_in),
                format_tokens(s.total_tokens_out),
                format_tokens(s.total_tokens_cache_write + s.total_tokens_cache_read),
            ),
            dim(),
        ));
        stats.push(Span::styled("  (approx)", dim()));
        lines.push(Line::from(stats));
    }

    // Events line: timing + last-turn shape + lifetime prompt count, all
    // on one line. Replaces the prior 2 separate `events:` and `last
    // turn:` rows.
    let mut events: Vec<Span> = vec![
        Span::styled("events ", dim()),
        Span::raw(format!(
            "last {} · prompt {} · stop {}",
            format_age_opt(s.last_event_at, now),
            format_age_opt(s.last_prompt_at, now),
            format_age_opt(s.last_stop_at, now),
        )),
    ];
    if let (Some(d), Some(c)) = (s.last_turn_duration_ms, s.last_turn_msg_count) {
        events.push(sep());
        events.push(Span::styled(
            format!("turn {}.{}s/{} msgs", d / 1000, (d % 1000) / 100, c),
            dim(),
        ));
    }
    if s.user_prompt_count > 0 {
        events.push(sep());
        events.push(Span::styled(
            format!("{} prompts", s.user_prompt_count),
            dim(),
        ));
    }
    lines.push(Line::from(events));

    // Spacer between body and footer, mirroring the body spacer logic.
    if lines.len() > footer_start_idx {
        lines.insert(footer_start_idx, Line::from(""));
    }

    let block = Block::default().borders(Borders::TOP);
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

/// Context-window size for a session. Five signals in priority order before
/// falling back to 200k:
///
/// 1. `config.model.context_window` (also `TRIAGE_CONTEXT_WINDOW` env via the
///    config's env-override layer) — explicit override.
/// 2. The session's own model name carries a `[1m]` tag (future-proof; today
///    the transcript strips it).
/// 3. `~/.claude/settings.json` default model has a `[1m]` tag — this is the
///    deterministic source for the user's global preference.
/// 4. Per-session peak `> 210k` tokens — empirical proof the variant isn't
///    200k-capped.
/// 5. Fleet-wide peak `> 210k` tokens — same proof from a sibling session.
fn context_window_for_session(
    s: &Session,
    app_peak: u64,
    default_model: Option<&str>,
    override_window: Option<u64>,
) -> u64 {
    if let Some(n) = override_window
        && n > 0
    {
        return n;
    }
    if let Some(m) = &s.latest_model
        && m.contains("[1m]")
    {
        return 1_000_000;
    }
    if default_model.is_some_and(|m| m.contains("[1m]")) {
        return 1_000_000;
    }
    if s.peak_context_tokens > 210_000 || app_peak > 210_000 {
        return 1_000_000;
    }
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

/// Read recent entries from `~/.config/triage/auto-decisions.jsonl`, with a
/// cache keyed on (mtime, size). Most draws happen with no new audit, so we
/// return the cached parse instead of re-reading the whole file. Newest-first.
/// On any I/O or parse error, returns an empty vec.
fn read_audit_log_cached(app: &mut AppState, limit: usize) -> Vec<serde_json::Value> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let path = std::path::PathBuf::from(home).join(".config/triage/auto-decisions.jsonl");
    let meta = std::fs::metadata(&path).ok();
    let key = meta.as_ref().and_then(|m| {
        let mtime = m.modified().ok()?;
        Some((mtime, m.len()))
    });
    if let (Some((mt, sz)), Some((cmt, csz, entries))) = (key, app.audit_log_cache.as_ref())
        && mt == *cmt
        && sz == *csz
    {
        return entries.iter().take(limit).cloned().collect();
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        app.audit_log_cache = None;
        return Vec::new();
    };
    let entries: Vec<serde_json::Value> = content
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let out = entries.iter().take(limit).cloned().collect();
    if let Some((mt, sz)) = key {
        app.audit_log_cache = Some((mt, sz, entries));
    }
    out
}

fn draw_audit_log(f: &mut Frame, area: Rect, app: &mut AppState, now: SystemTime) {
    let dim = || Style::default().fg(Color::DarkGray);
    let bold = || Style::default().add_modifier(Modifier::BOLD);

    let entries = read_audit_log_cached(app, 200);
    let total = entries.len();

    let mut lines: Vec<Line> = Vec::new();

    if entries.is_empty() {
        let msg = if app.autonomous {
            "no audits yet — auto mode is on but hasn't fired against a Blocked session"
        } else {
            "auto mode is off — no audits to show"
        };
        lines.push(Line::from(Span::styled(msg, dim())));
    }

    for v in &entries {
        let ts = v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0);
        let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0);
        let cwd = v.get("cwd").and_then(|c| c.as_str()).unwrap_or("");
        let cwd_short = std::path::Path::new(cwd)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| cwd.to_string());
        let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("?");
        let decision = v
            .get("decision")
            .and_then(|d| d.as_str())
            .unwrap_or("?")
            .to_string();
        let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("");
        let cost = v.get("cost_usd").and_then(|c| c.as_f64());
        let duration_ms = v.get("duration_ms").and_then(|d| d.as_u64());

        let now_secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ago = if now_secs > ts && ts > 0 {
            let s = now_secs - ts;
            if s < 60 {
                format!("{s}s ago")
            } else if s < 3600 {
                format!("{}m ago", s / 60)
            } else if s < 86400 {
                format!("{}h ago", s / 3600)
            } else {
                format!("{}d ago", s / 86400)
            }
        } else {
            "?".to_string()
        };

        let decision_color = match decision.as_str() {
            "APPROVE" => Color::Green,
            "DENY" => Color::Red,
            "WAIT" => Color::Yellow,
            _ => Color::DarkGray,
        };

        // Row 1: time · decision · tool · pid+cwd · cost/duration
        let mut row1: Vec<Span> = vec![
            Span::styled(format!("{:>10}", ago), dim()),
            Span::raw("  "),
            Span::styled(format!("{:<7}", decision), Style::default().fg(decision_color).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(format!("{:<6}", tool), bold()),
            Span::raw("  "),
            Span::styled(format!("pid {pid} {cwd_short}"), dim()),
        ];
        let perf = match (cost, duration_ms) {
            (Some(c), Some(ms)) => Some(format!("{:.1}s · ${:.4}", ms as f64 / 1000.0, c)),
            (Some(c), None) => Some(format!("${:.4}", c)),
            (None, Some(ms)) => Some(format!("{:.1}s", ms as f64 / 1000.0)),
            (None, None) => None,
        };
        if let Some(p) = perf {
            row1.push(Span::styled("  ·  ", dim()));
            row1.push(Span::styled(p, dim()));
        }
        lines.push(Line::from(row1));
        // Row 2: reason, indented
        if !reason.is_empty() {
            lines.push(Line::from(vec![
                Span::raw("            "),
                Span::raw(reason.to_string()),
            ]));
        }
        // Spacer
        lines.push(Line::from(""));
    }

    // Save total content lines so the key handler can clamp scroll on j/G.
    app.audit_log_total_lines = lines.len() as u16;
    // Clamp current offset against fresh content (e.g. user pressed `j` past
    // the end before this draw, or content shrank somehow).
    let max_offset = app
        .audit_log_total_lines
        .saturating_sub(area.height.saturating_sub(2)); // 2 for top border + breathing room
    if app.audit_log_offset > max_offset {
        app.audit_log_offset = max_offset;
    }

    let summary = if total > 0 {
        format!(" auto-decisions  ·  {} entries  ·  newest first ", total)
    } else {
        " auto-decisions ".to_string()
    };
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(summary, dim()));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.audit_log_offset, 0));
    f.render_widget(para, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &AppState) {
    if app.audit_log_open {
        let hint = if app.pending_g {
            "  g … press g again to jump to top, any other key cancels".to_string()
        } else {
            "  j/k scroll  ·  ^d/^u half-page  ·  gg top  ·  G bottom  ·  H/Esc close  ·  q quit"
                .to_string()
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    let line = if let Some(msg) = &app.status_msg {
        Line::from(Span::styled(msg.clone(), Style::default().fg(Color::Yellow)))
    } else {
        let mode = app.approval_mode.label();
        let auto = if app.autonomous {
            let n = app.audit_in_flight.len();
            if n > 0 { format!("AUTO·{n}") } else { "AUTO".to_string() }
        } else {
            "auto:off".to_string()
        };
        // Phone-push indicator: only shown when off (the non-default state).
        // ON is silent — same minimalism as autonomous's "auto:off" hint.
        // Narrow mode skips the indicator to stay under the budget.
        let phone_off_seg = if !app.phone_push_enabled {
            " [phone:off]"
        } else {
            ""
        };
        let hint = match LayoutMode::from_width(area.width) {
            LayoutMode::Narrow => format!(" ⏎ a d h:{mode} A:{auto} q"),
            LayoutMode::Medium => format!(
                " ⏎ jump  a/d  h [{mode}]  A [{auto}]{phone_off_seg}  q"
            ),
            LayoutMode::Wide => format!(
                "  ⏎ jump  a/d approve/deny  h [{mode}]  A [{auto}]  p phone{phone_off_seg}  m mute  H log  q quit"
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
