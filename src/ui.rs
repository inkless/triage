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
use crate::models::{ApprovalMode, AttentionState, Provider, Session, session_display_label};
use crate::persist::{self, AliasKey, MuteKey};
use crate::transcript::DigestCache;

pub struct AppState {
    pub sessions: Vec<Session>,
    pub selected: TableState,
    pub detail_open: bool,
    pub status_msg: Option<String>,
    pub digest_cache: DigestCache,
    pub codex_cache: crate::codex::CodexDigestCache,
    /// Triage-local row aliases keyed by provider + alias session id. For
    /// Codex this is the root thread id so aliases follow spawned child
    /// threads without following unrelated pane reuse.
    /// These never mutate Claude/Codex/tmux state.
    pub aliases: HashMap<AliasKey, String>,
    /// (cwd, started_at_ms) → time of mute. Keyed on a stable identity rather
    /// than pid so the entries survive a triage restart and don't accidentally
    /// re-mute a recycled pid.
    pub muted: HashMap<MuteKey, SystemTime>,
    /// In-memory set of sessions to fire a "finished" notification for on each
    /// transition into `JustFinished` (T-81). Sticky — only the user can
    /// clear an entry by pressing `w` again on the row. Not persisted across
    /// restarts; a watch only makes sense while the session exists.
    pub watched: HashSet<MuteKey>,
    /// (cwd, started_at_ms) of sessions pinned to the top via `*`. Sticky —
    /// only `*` clears an entry — and persisted across restarts.
    pub pinned: HashSet<MuteKey>,
    /// pid → most recently observed AttentionState. Used to detect transitions
    /// (e.g. into `Blocked`) so we can fire a desktop notification once per
    /// transition rather than on every refresh while the session stays blocked.
    pub last_states: HashMap<u32, AttentionState>,
    /// Which mechanism Claude `a`/`d` use to deliver an approval. Configured
    /// via `[approval].mode`; Codex approvals always use the tmux path.
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
    /// Full keybinding help view. Toggle with `?` from normal mode.
    pub key_help_open: bool,
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
    /// `$` overlay state — daily/weekly cost rollup across all sessions.
    /// Computed lazily on open + cached for `COST_OVERLAY_TTL` (60s) so
    /// re-opens within the same minute don't re-walk ~/.claude/projects.
    pub cost_overlay_open: bool,
    pub cost_overlay_offset: u16,
    pub cost_overlay_total_lines: u16,
    pub cost_cache: Option<(SystemTime, crate::cost_rollup::Rollup)>,
    /// Active filter query. Empty = no filter applied. Case-insensitive
    /// substring match against any of: Claude `--name`, tmux window_name,
    /// tmux session_name, full cwd path.
    pub filter: String,
    /// True while the user is typing into the filter after `/`. Printable
    /// keys append to `filter`; Enter/Esc exit edit mode (Enter keeps the
    /// query, Esc clears it). Outside edit mode the filter just applies
    /// and all other keybindings work normally on the filtered subset.
    pub filter_active: bool,
    /// True while editing a triage-local alias after pressing `R`.
    pub rename_active: bool,
    pub rename_key: Option<AliasKey>,
    pub rename_buffer: String,
    /// True while composing a one-line user reply to the selected agent.
    pub reply_active: bool,
    pub reply_target: Option<String>,
    pub reply_target_label: String,
    pub reply_buffer: String,
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
    /// `N` picker state for launching a configured agent in a new tmux
    /// window from one of the known session working directories.
    pub spawn_picker_open: bool,
    pub spawn_picker_selected: usize,
}

impl AppState {
    pub fn new() -> Self {
        let mut state = TableState::default();
        state.select(Some(0));
        let loaded = persist::load_state();
        let muted = loaded.mutes.into_iter().collect();
        let pinned = loaded.pins.into_iter().collect();
        let aliases = loaded.aliases.into_iter().collect();
        let (audit_tx, audit_rx) = mpsc::channel();
        Self {
            sessions: Vec::new(),
            selected: state,
            detail_open: false,
            status_msg: None,
            digest_cache: DigestCache::new(),
            codex_cache: crate::codex::CodexDigestCache::new(),
            aliases,
            muted,
            pinned,
            watched: HashSet::new(),
            last_states: HashMap::new(),
            approval_mode: ApprovalMode::default(),
            autonomous: loaded.autonomous,
            phone_push_enabled: loaded.phone_push_enabled,
            audit_in_flight: HashMap::new(),
            audit_decided: HashSet::new(),
            audit_notes: HashMap::new(),
            audit_tx,
            audit_rx,
            default_model: crate::approval::read_default_model(),
            audit_log_open: false,
            key_help_open: false,
            audit_log_offset: 0,
            audit_log_total_lines: 0,
            pending_g: false,
            audit_log_cache: None,
            cost_overlay_open: false,
            cost_overlay_offset: 0,
            cost_overlay_total_lines: 0,
            cost_cache: None,
            filter: String::new(),
            filter_active: false,
            rename_active: false,
            rename_key: None,
            rename_buffer: String::new(),
            reply_active: false,
            reply_target: None,
            reply_target_label: String::new(),
            reply_buffer: String::new(),
            exit_on_jump: false,
            zoom_on_jump: false,
            last_pane_width: 0,
            last_client_width: 0,
            config: Config::default(),
            spawn_picker_open: false,
            spawn_picker_selected: 0,
        }
    }

    /// Open / close the `$` cost overlay. Always succeeds — unlike the audit
    /// log there's no on/off prerequisite (every user has historical
    /// transcripts). On open, drops the scroll offset and warms the cache if
    /// it's stale.
    pub fn toggle_cost_overlay(&mut self) {
        self.cost_overlay_open = !self.cost_overlay_open;
        if !self.cost_overlay_open {
            self.cost_overlay_offset = 0;
            self.pending_g = false;
            return;
        }
        // Opening: clear scroll + invalidate stale cache so the open shows
        // fresh data. The actual scan happens lazily in `cost_rollup_cached`
        // on the next draw — keeps the keypress non-blocking.
        self.cost_overlay_offset = 0;
        self.pending_g = false;
        const TTL: std::time::Duration = std::time::Duration::from_secs(60);
        let fresh = self
            .cost_cache
            .as_ref()
            .and_then(|(t, _)| SystemTime::now().duration_since(*t).ok())
            .is_some_and(|age| age < TTL);
        if !fresh {
            self.cost_cache = None;
        }
    }

    /// Return the cached rollup, computing one if missing. Called from the
    /// overlay draw path; the first draw after `toggle_cost_overlay` opens
    /// the overlay pays the scan cost (~hundreds of ms in release).
    pub fn cost_rollup_cached(&mut self) -> &crate::cost_rollup::Rollup {
        if self.cost_cache.is_none() {
            let r = crate::cost_rollup::compute_rollup();
            self.cost_cache = Some((SystemTime::now(), r));
        }
        &self.cost_cache.as_ref().unwrap().1
    }

    pub fn toggle_audit_log(&mut self) {
        // No autonomous gate: the log file persists historical entries
        // across auto-mode toggles, so the user might genuinely want to
        // review past decisions even when auto is currently off. The
        // renderer already shows a sensible empty-state message.
        self.audit_log_open = !self.audit_log_open;
        if !self.audit_log_open {
            self.audit_log_offset = 0;
            self.pending_g = false;
        }
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
        let Some(s) = self.selected_session() else {
            return;
        };
        let key = MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        if self.muted.remove(&key).is_none() {
            // Mute and pin are mutually exclusive opposites — muting a pinned
            // row clears the pin so it sinks to the bottom rather than staying
            // floated at the top.
            self.pinned.remove(&key);
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
        let label = session_display_label(s);
        let now_watching = if self.watched.remove(&key) {
            false
        } else {
            self.watched.insert(key);
            true
        };
        Some((now_watching, label))
    }

    /// Toggle the pin on the selected session. Pinned sessions sort to the top
    /// (see `sort_sessions`). Persisted so pins survive a restart. Returns
    /// `(now_pinned, label)` for the status line, or None if nothing selected.
    pub fn toggle_pin_selected(&mut self) -> Option<(bool, String)> {
        let s = self.selected_session()?;
        let key = MuteKey {
            cwd: s.cwd.clone(),
            started_at_ms: s.started_at_ms,
        };
        let label = session_display_label(s);
        let now_pinned = if self.pinned.remove(&key) {
            false
        } else {
            // Pinning clears any mute — they're mutually exclusive opposites.
            self.muted.remove(&key);
            self.pinned.insert(key);
            true
        };
        self.persist_state();
        Some((now_pinned, label))
    }

    pub fn persist_state(&self) {
        persist::save_state(
            self.muted.iter(),
            self.pinned.iter(),
            self.aliases.iter(),
            self.autonomous,
            self.phone_push_enabled,
        );
    }

    fn persist_state_replace_aliases(&self) {
        persist::save_state_replace_aliases(
            self.muted.iter(),
            self.pinned.iter(),
            self.aliases.iter(),
            self.autonomous,
            self.phone_push_enabled,
        );
    }

    pub fn start_rename_selected(&mut self) -> Option<()> {
        let s = self.selected_session()?;
        let key = AliasKey::for_session(s);
        self.rename_buffer = AliasKey::candidates_for_session(s)
            .iter()
            .find_map(|key| self.aliases.get(key).cloned())
            .unwrap_or_default();
        self.rename_active = true;
        self.rename_key = Some(key);
        self.pending_g = false;
        Some(())
    }

    pub fn cancel_rename(&mut self) {
        self.rename_active = false;
        self.rename_key = None;
        self.rename_buffer.clear();
    }

    pub fn start_reply_selected(&mut self) -> Option<()> {
        let (target, label) = {
            let s = self.selected_session()?;
            let pane = s.pane.as_ref()?;
            (pane.pane_id.clone(), target_label(s))
        };
        self.reply_active = true;
        self.reply_target = Some(target);
        self.reply_target_label = label;
        self.reply_buffer.clear();
        self.audit_log_open = false;
        self.cost_overlay_open = false;
        self.filter_active = false;
        self.pending_g = false;
        Some(())
    }

    pub fn cancel_reply(&mut self) {
        self.reply_active = false;
        self.reply_target = None;
        self.reply_target_label.clear();
        self.reply_buffer.clear();
    }

    pub fn start_spawn_picker(&mut self) {
        let choices = self.spawn_cwd_choices();
        let selected_cwd = self.selected_session().map(|s| s.cwd.clone());
        self.spawn_picker_selected = selected_cwd
            .and_then(|cwd| choices.iter().position(|choice| *choice == cwd))
            .unwrap_or(0);
        self.spawn_picker_open = true;
        self.audit_log_open = false;
        self.cost_overlay_open = false;
        self.filter_active = false;
        self.pending_g = false;
    }

    pub fn cancel_spawn_picker(&mut self) {
        self.spawn_picker_open = false;
        self.spawn_picker_selected = 0;
    }

    pub fn spawn_cwd_choices(&self) -> Vec<std::path::PathBuf> {
        crate::spawn_agent::cwd_choices(&self.sessions)
    }

    pub fn selected_spawn_cwd(&self) -> Option<std::path::PathBuf> {
        let choices = self.spawn_cwd_choices();
        choices.get(self.spawn_picker_selected).cloned()
    }

    pub fn move_spawn_selection(&mut self, delta: i32) {
        let len = self.spawn_cwd_choices().len();
        if len == 0 {
            self.spawn_picker_selected = 0;
            return;
        }
        let cur = self.spawn_picker_selected.min(len - 1) as i32;
        self.spawn_picker_selected = (cur + delta).rem_euclid(len as i32) as usize;
    }

    pub fn clamp_spawn_selection(&mut self) {
        let len = self.spawn_cwd_choices().len();
        if len == 0 {
            self.spawn_picker_selected = 0;
        } else if self.spawn_picker_selected >= len {
            self.spawn_picker_selected = len - 1;
        }
    }

    pub fn commit_rename_selected(&mut self) -> Option<String> {
        let keys_to_clear = self
            .selected_session()
            .map(AliasKey::candidates_for_session)
            .unwrap_or_default();
        let key = self
            .rename_key
            .take()
            .or_else(|| self.selected_session().map(AliasKey::for_session))?;
        let alias = self
            .rename_buffer
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        self.rename_active = false;
        self.rename_buffer.clear();
        for candidate in keys_to_clear {
            self.aliases.remove(&candidate);
        }
        let msg = if alias.is_empty() {
            self.aliases.remove(&key);
            format!("alias cleared for {}", key.provider)
        } else {
            self.aliases.insert(key.clone(), alias.clone());
            format!("alias: {alias}")
        };
        for session in &mut self.sessions {
            if AliasKey::for_session(session) == key {
                session.name = (!alias.is_empty()).then_some(alias.clone());
            }
        }
        apply_aliases_to_sessions(&mut self.sessions, &self.aliases);
        self.persist_state_replace_aliases();
        Some(msg)
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
        if self.filter.is_empty() {
            return self.sessions.iter().collect();
        }
        let q = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| session_matches_filter(s, &q))
            .collect()
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
        if self.spawn_picker_open {
            self.clamp_spawn_selection();
        }
    }
}

pub fn apply_aliases_to_sessions(sessions: &mut [Session], aliases: &HashMap<AliasKey, String>) {
    for session in sessions {
        if let Some(alias) = AliasKey::candidates_for_session(session)
            .iter()
            .find_map(|key| aliases.get(key))
            && !alias.trim().is_empty()
        {
            session.name = Some(alias.clone());
        }
    }
}

pub fn draw(f: &mut Frame, app: &mut AppState, now: SystemTime) {
    if app.key_help_open {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(f.area());
        draw_header(f, chunks[0], app);
        draw_key_help(f, chunks[1]);
        draw_footer(f, chunks[2], app);
        return;
    }
    if app.spawn_picker_open {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(f.area());
        draw_header(f, chunks[0], app);
        draw_spawn_picker(f, chunks[1], app);
        draw_footer(f, chunks[2], app);
        return;
    }
    if app.audit_log_open {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(f.area());
        draw_header(f, chunks[0], app);
        draw_audit_log(f, chunks[1], app, now);
        // draw_footer borrows app immutably — clone the small bits we need
        // and let it run after draw_audit_log writes back its line count.
        draw_footer(f, chunks[2], app);
        return;
    }
    if app.cost_overlay_open {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(f.area());
        draw_header(f, chunks[0], app);
        draw_cost_overlay(f, chunks[1], app);
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
    let visible = app.visible().len();
    let counts = if app.filter.is_empty() {
        format!("{total} session{}", if total == 1 { "" } else { "s" })
    } else {
        format!("{visible}/{total} sessions")
    };
    // Show pane width inline + a `zoom` indicator when auto-detect is active,
    // so it's obvious whether Enter will zoom on the current device.
    let zoom_marker = if app.should_zoom_on_jump() {
        " · zoom"
    } else {
        ""
    };
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
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("triage", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(counts, Style::default().fg(Color::DarkGray)),
        Span::raw("   "),
    ];
    spans.extend(header_status_spans(app, area.width));
    if app.reply_active {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("reply to {}: ", app.reply_target_label),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("{}_", app.reply_buffer),
            Style::default().fg(Color::Yellow),
        ));
    } else if app.rename_active {
        let cursor = "_";
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "rename: ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("{}{cursor}", app.rename_buffer),
            Style::default().fg(Color::Yellow),
        ));
    } else if app.filter_active || !app.filter.is_empty() {
        let cursor = if app.filter_active { "_" } else { "" };
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "filter: ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("{}{cursor}", app.filter),
            Style::default().fg(Color::Yellow),
        ));
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(dims, Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn header_status_spans(app: &AppState, width: u16) -> Vec<Span<'static>> {
    let mode = LayoutMode::from_width(width);
    let separator = match mode {
        LayoutMode::Narrow => " ",
        LayoutMode::Medium | LayoutMode::Wide => " · ",
    };
    vec![
        Span::styled(
            auto_mode_label_parts(app.autonomous, app.audit_in_flight.len(), width),
            auto_mode_style(app),
        ),
        Span::styled(separator, Style::default().fg(Color::DarkGray)),
        Span::styled(
            phone_mode_label_parts(app.phone_push_enabled, width),
            phone_mode_style(app.phone_push_enabled),
        ),
    ]
}

fn auto_mode_label_parts(autonomous: bool, in_flight: usize, width: u16) -> String {
    let mode = LayoutMode::from_width(width);
    if autonomous {
        match mode {
            LayoutMode::Narrow => {
                if in_flight > 0 {
                    format!("A:on·{in_flight}")
                } else {
                    "A:on".to_string()
                }
            }
            LayoutMode::Medium => {
                if in_flight > 0 {
                    format!("AUTO on·{in_flight}")
                } else {
                    "AUTO on".to_string()
                }
            }
            LayoutMode::Wide => {
                if in_flight > 0 {
                    format!(
                        "AUTO on · {in_flight} audit{}",
                        if in_flight == 1 { "" } else { "s" }
                    )
                } else {
                    "AUTO on".to_string()
                }
            }
        }
    } else {
        match mode {
            LayoutMode::Narrow => "A:off".to_string(),
            LayoutMode::Medium | LayoutMode::Wide => "AUTO off".to_string(),
        }
    }
}

fn phone_mode_label_parts(phone_push_enabled: bool, width: u16) -> String {
    let mode = LayoutMode::from_width(width);
    match (mode, phone_push_enabled) {
        (LayoutMode::Narrow, true) => "ph:on".to_string(),
        (LayoutMode::Narrow, false) => "ph:off".to_string(),
        (_, true) => "phone on".to_string(),
        (_, false) => "phone off".to_string(),
    }
}

fn auto_mode_style(app: &AppState) -> Style {
    if app.autonomous {
        if app.audit_in_flight.is_empty() {
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        }
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn phone_mode_style(phone_push_enabled: bool) -> Style {
    if phone_push_enabled {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    }
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
            7 + 5 + 3 + 16 + 4 + 2,
            vec![
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Length(16),
                Constraint::Min(20),
            ],
            vec![
                Cell::from("STATE"),
                Cell::from("AGE"),
                Cell::from("AI"),
                Cell::from("SESSION"),
                Cell::from("HEADLINE"),
            ],
        ),
        LayoutMode::Wide => (
            7 + 5 + 3 + 18 + 24 + 5 + 2,
            vec![
                Constraint::Length(7),
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Length(18),
                Constraint::Length(24),
                Constraint::Min(20),
            ],
            vec![
                Cell::from("STATE"),
                Cell::from("AGE"),
                Cell::from("AI"),
                Cell::from("SESSION"),
                Cell::from("CWD"),
                Cell::from("HEADLINE"),
            ],
        ),
    };

    let headline_width = (area.width as usize).saturating_sub(fixed).max(1);
    let mut rows: Vec<Row> = visible
        .iter()
        .enumerate()
        .map(|(i, s)| build_row(s, now, headline_width, layout, Some(i) == selected_idx))
        .collect();

    // Thin rule between the pinned block (which sorts to the top) and the rest,
    // shown only when both groups are present. Pinned rows are contiguous at
    // the front, so the boundary is just the count of leading pinned rows.
    //
    // The divider is a render-only Row: `app.selected` stays a logical index
    // into `visible`, and we translate to the divider-shifted visual space on a
    // throwaway TableState here. That keeps navigation / jump / selection
    // (which all index `visible`) oblivious to the extra row.
    let pin_count = visible.iter().take_while(|s| s.pinned).count();
    let show_divider = pin_count > 0 && pin_count < visible.len();
    let mut render_state = app.selected;
    if show_divider {
        let col_widths: Vec<usize> = widths
            .iter()
            .map(|c| match c {
                Constraint::Length(n) => *n as usize,
                _ => headline_width,
            })
            .collect();
        let divider_cells: Vec<Cell> = col_widths
            .iter()
            .map(|w| Cell::from("─".repeat(*w)).style(Style::default().fg(Color::DarkGray)))
            .collect();
        rows.insert(
            pin_count,
            Row::new(divider_cells).height(1).bottom_margin(1),
        );

        if let Some(sel) = render_state.selected()
            && sel >= pin_count
        {
            render_state.select(Some(sel + 1));
        }
        let off = render_state.offset();
        if off >= pin_count {
            *render_state.offset_mut() = off + 1;
        }
    }

    let header = Row::new(header_cells).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

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

    f.render_stateful_widget(table, area, &mut render_state);

    // Persist the (possibly auto-scrolled) offset back into the logical state,
    // undoing the divider shift so the next frame starts from the right place.
    let voff = render_state.offset();
    let loff = if show_divider && voff > pin_count {
        voff - 1
    } else {
        voff
    };
    *app.selected.offset_mut() = loff;
}

fn draw_spawn_picker(f: &mut Frame, area: Rect, app: &mut AppState) {
    app.clamp_spawn_selection();
    let choices = app.spawn_cwd_choices();
    let selected = app
        .spawn_picker_selected
        .min(choices.len().saturating_sub(1));
    let provider = app.config.new_agent.provider.name();
    let command = app.config.new_agent.command.as_str();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(area);

    let intro = vec![
        Line::from(vec![
            Span::styled(
                format!("New {provider} agent"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("command: {command}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            "Choose a working directory from current triage sessions.",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(intro), chunks[0]);

    let capacity = chunks[1].height.saturating_sub(2).max(1) as usize;
    let offset = selected.saturating_sub(capacity.saturating_sub(1));
    let path_width = chunks[1].width.saturating_sub(10).max(10) as usize;
    let rows = choices
        .iter()
        .enumerate()
        .skip(offset)
        .take(capacity)
        .map(|(i, cwd)| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("{}/{}", i + 1, choices.len())),
                Cell::from(shorten_path(&cwd.display().to_string(), path_width)),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();

    let table = Table::new(rows, vec![Constraint::Length(8), Constraint::Min(10)])
        .header(
            Row::new(vec![Cell::from("#"), Cell::from("CWD")]).style(
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().borders(Borders::TOP));
    f.render_widget(table, chunks[1]);
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

    let session_label = session_display_label(s);
    let provider_label = s.provider.label();

    let cwd_short = shorten_path(&s.cwd.to_string_lossy(), 24);
    // Only show a permission headline when Claude itself is actually paused on
    // user input. Pending files alone are not enough: the hook also sees
    // auto-approved tool calls.
    let headline_raw = if s.provider == Provider::Claude && s.status == "waiting" {
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
    } else if s.provider == Provider::Codex && s.state == AttentionState::Blocked {
        if let Some((name, brief)) = &s.last_tool_use {
            if brief.is_empty() {
                format!("⏸ approve {name}?")
            } else {
                format!("⏸ approve {name}? — {brief}")
            }
        } else {
            "⏸ codex needs input".to_string()
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
        LayoutMode::Narrow => format!("{provider_label} {session_label}  {age}  · {headline_raw}"),
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
    // Pinned rows are set apart by a divider rule in the table (drawn in
    // draw_table) rather than a per-row glyph, so no pin marker here.
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
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let provider_style = if is_selected && !s.muted {
        Style::default()
    } else {
        match s.provider {
            Provider::Claude => Style::default().fg(Color::DarkGray),
            Provider::Codex => Style::default().fg(Color::Cyan),
        }
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
            Cell::from(provider_label).style(provider_style),
            Cell::from(session_label).style(session_style),
            Cell::from(Text::from(headline_lines)),
        ],
        LayoutMode::Wide => vec![
            Cell::from(state_label).style(state_cell_style),
            Cell::from(age).style(cwd_style),
            Cell::from(provider_label).style(provider_style),
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
        let need = if line.is_empty() {
            wlen
        } else {
            line_chars + 1 + wlen
        };
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
    // Single line, no labels. State (colored) · pane · model · uptime.
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
    let default_model = (s.provider == Provider::Claude)
        .then_some(app.default_model.as_deref())
        .flatten();
    let window =
        context_window_for_session(s, app_peak, default_model, app.config.model.context_window);
    let is_1m = window >= 1_000_000;

    let mut header = vec![
        Span::styled(
            state_str,
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(pane_target, bold()),
    ];
    // Model label: prefer the per-message `model` (most precise — includes
    // the version, e.g. `opus-4-7`); fall back to the user's global default
    // from settings.json (e.g. `opus[1m]`). Strip the `claude-` prefix and
    // append ` (1M)` when we've confirmed the 1M variant via any signal.
    let model_raw = s.latest_model.as_deref().or(default_model);
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
    if s.pinned {
        header.push(sep());
        header.push(Span::styled("[pinned]", yellow()));
    }
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

    if s.provider == Provider::Claude && s.status == "waiting" {
        if let Some(a) = s.pending_approvals.first() {
            lines.push(Line::from(vec![
                Span::styled(a.tool_name.clone(), yellow().add_modifier(Modifier::BOLD)),
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
    } else if s.provider == Provider::Codex && s.state == AttentionState::Blocked {
        lines.push(Line::from(vec![
            Span::styled("codex  ", dim()),
            Span::styled("approval requested in pane", yellow()),
        ]));
        if let Some((name, brief)) = &s.last_tool_use {
            lines.push(Line::from(vec![
                Span::styled(name.clone(), yellow().add_modifier(Modifier::BOLD)),
                Span::styled("  (visible prompt)", dim()),
            ]));
            if !brief.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(truncate(brief, 400)),
                ]));
            }
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
    if s.total_cost_usd > 0.0 || s.total_tokens_in > 0 || s.latest_context_tokens > 0 {
        let mut stats: Vec<Span> = vec![Span::styled(
            if s.provider == Provider::Codex {
                "usage  "
            } else {
                "cost   "
            },
            dim(),
        )];
        let mut needs_sep = false;
        if s.provider == Provider::Claude {
            stats.push(Span::raw(format_cost(s.total_cost_usd)));
            needs_sep = true;
        }
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
            if needs_sep {
                stats.push(sep());
            }
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
            needs_sep = true;
        }
        if s.total_tokens_in > 0 || s.total_tokens_out > 0 || s.total_tokens_cache_read > 0 {
            if needs_sep {
                stats.push(sep());
            }
            stats.push(Span::styled(
                format!(
                    "{} in · {} out · {} cache",
                    format_tokens(s.total_tokens_in),
                    format_tokens(s.total_tokens_out),
                    format_tokens(s.total_tokens_cache_write + s.total_tokens_cache_read),
                ),
                dim(),
            ));
        }
        if s.provider == Provider::Claude {
            stats.push(Span::styled("  (approx)", dim()));
        }
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
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn format_age_opt(ts: Option<SystemTime>, now: SystemTime) -> String {
    let Some(ts) = ts else {
        return "—".to_string();
    };
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
    if let Some(n) = s.context_window
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
            Span::styled(
                format!("{:<7}", decision),
                Style::default()
                    .fg(decision_color)
                    .add_modifier(Modifier::BOLD),
            ),
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

#[derive(Debug, Default)]
struct LiveCodexTokenRollup {
    session_count: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: u64,
    total_tokens: u64,
    top_cwds: Vec<LiveCodexCwdBucket>,
    models: Vec<LiveCodexModelBucket>,
    max_context: Option<LiveCodexContextPeak>,
}

#[derive(Debug)]
struct LiveCodexCwdBucket {
    cwd: String,
    session_count: u64,
    total_tokens: u64,
}

#[derive(Debug)]
struct LiveCodexModelBucket {
    model: String,
    session_count: u64,
    total_tokens: u64,
}

#[derive(Debug)]
struct LiveCodexContextPeak {
    cwd: String,
    session_id: String,
    tokens: u64,
    window: Option<u64>,
}

fn live_codex_token_rollup(sessions: &[Session]) -> LiveCodexTokenRollup {
    let mut rollup = LiveCodexTokenRollup::default();
    let mut cwd_buckets: HashMap<String, LiveCodexCwdBucket> = HashMap::new();
    let mut model_buckets: HashMap<String, LiveCodexModelBucket> = HashMap::new();

    for s in sessions.iter().filter(|s| s.provider == Provider::Codex) {
        let total_tokens = s.total_tokens_in + s.total_tokens_out;
        if total_tokens == 0 && s.latest_context_tokens == 0 {
            continue;
        }
        rollup.session_count += 1;
        rollup.tokens_in += s.total_tokens_in;
        rollup.tokens_out += s.total_tokens_out;
        rollup.cache_read += s.total_tokens_cache_read;
        rollup.total_tokens += total_tokens;

        let cwd = s.cwd.display().to_string();
        let cwd_bucket = cwd_buckets
            .entry(cwd.clone())
            .or_insert_with(|| LiveCodexCwdBucket {
                cwd: cwd.clone(),
                session_count: 0,
                total_tokens: 0,
            });
        cwd_bucket.session_count += 1;
        cwd_bucket.total_tokens += total_tokens;

        if let Some(model) = s.latest_model.as_deref()
            && !model.trim().is_empty()
        {
            let model_bucket =
                model_buckets
                    .entry(model.to_string())
                    .or_insert_with(|| LiveCodexModelBucket {
                        model: model.to_string(),
                        session_count: 0,
                        total_tokens: 0,
                    });
            model_bucket.session_count += 1;
            model_bucket.total_tokens += total_tokens;
        }

        if s.latest_context_tokens > 0
            && rollup
                .max_context
                .as_ref()
                .is_none_or(|peak| context_rank_session(s) > context_rank_peak(peak))
        {
            rollup.max_context = Some(LiveCodexContextPeak {
                cwd,
                session_id: s.session_id.clone(),
                tokens: s.latest_context_tokens,
                window: s.context_window,
            });
        }
    }

    rollup.top_cwds = cwd_buckets.into_values().collect();
    rollup.top_cwds.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| b.session_count.cmp(&a.session_count))
            .then_with(|| a.cwd.cmp(&b.cwd))
    });
    rollup.models = model_buckets.into_values().collect();
    rollup.models.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| b.session_count.cmp(&a.session_count))
            .then_with(|| a.model.cmp(&b.model))
    });
    rollup
}

fn context_rank_session(s: &Session) -> u64 {
    s.context_window
        .filter(|window| *window > 0)
        .map(|window| s.latest_context_tokens.saturating_mul(10_000) / window)
        .unwrap_or(s.latest_context_tokens)
}

fn context_rank_peak(peak: &LiveCodexContextPeak) -> u64 {
    peak.window
        .filter(|window| *window > 0)
        .map(|window| peak.tokens.saturating_mul(10_000) / window)
        .unwrap_or(peak.tokens)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::models::Pane;

    use super::*;

    #[test]
    fn live_codex_token_rollup_uses_only_codex_sessions() {
        let mut codex = Session::new(
            Provider::Codex,
            1,
            "codex-session".to_string(),
            PathBuf::from("/repo/triage"),
            None,
            "idle".to_string(),
            0,
            0,
            None,
        );
        codex.total_tokens_in = 1_000;
        codex.total_tokens_out = 200;
        codex.total_tokens_cache_read = 700;
        codex.latest_context_tokens = 900;
        codex.context_window = Some(1_800);
        codex.latest_model = Some("gpt-5.5".to_string());

        let mut claude = Session::new(
            Provider::Claude,
            2,
            "claude-session".to_string(),
            PathBuf::from("/repo/other"),
            None,
            "idle".to_string(),
            0,
            0,
            None,
        );
        claude.total_tokens_in = 9_000;

        let rollup = live_codex_token_rollup(&[codex, claude]);

        assert_eq!(rollup.session_count, 1);
        assert_eq!(rollup.tokens_in, 1_000);
        assert_eq!(rollup.tokens_out, 200);
        assert_eq!(rollup.cache_read, 700);
        assert_eq!(rollup.total_tokens, 1_200);
        assert_eq!(rollup.top_cwds[0].cwd, "/repo/triage");
        assert_eq!(rollup.models[0].model, "gpt-5.5");
        assert_eq!(rollup.max_context.as_ref().unwrap().tokens, 900);
    }

    #[test]
    fn auto_mode_label_is_compact_on_narrow_widths() {
        assert_eq!(auto_mode_label_parts(false, 0, 40), "A:off");
        assert_eq!(auto_mode_label_parts(true, 0, 40), "A:on");
        assert_eq!(auto_mode_label_parts(true, 2, 40), "A:on·2");
    }

    #[test]
    fn auto_mode_label_shows_audit_count_on_wide_widths() {
        assert_eq!(auto_mode_label_parts(false, 0, 120), "AUTO off");
        assert_eq!(auto_mode_label_parts(true, 0, 120), "AUTO on");
        assert_eq!(auto_mode_label_parts(true, 1, 120), "AUTO on · 1 audit");
        assert_eq!(auto_mode_label_parts(true, 2, 120), "AUTO on · 2 audits");
    }

    #[test]
    fn phone_mode_label_shows_on_and_off() {
        assert_eq!(phone_mode_label_parts(true, 40), "ph:on");
        assert_eq!(phone_mode_label_parts(false, 40), "ph:off");
        assert_eq!(phone_mode_label_parts(true, 120), "phone on");
        assert_eq!(phone_mode_label_parts(false, 120), "phone off");
    }

    #[test]
    fn start_reply_selected_records_pane_target_and_label() {
        let mut app = AppState::new();
        let mut session = Session::new(
            Provider::Claude,
            1,
            "sid".to_string(),
            PathBuf::from("/repo/triage"),
            Some("agent-triage".to_string()),
            "idle".to_string(),
            0,
            0,
            None,
        );
        session.pane = Some(Pane {
            target: "main:1.0".to_string(),
            tmux_session: "main".to_string(),
            window_name: "triage-work".to_string(),
            pane_id: "%42".to_string(),
            pid: 123,
            tty: "/dev/ttys001".to_string(),
            current_command: "claude".to_string(),
            cwd: PathBuf::from("/repo/triage"),
            active: true,
        });
        app.sessions.push(session);
        app.selected.select(Some(0));

        assert!(app.start_reply_selected().is_some());

        assert!(app.reply_active);
        assert_eq!(app.reply_target.as_deref(), Some("%42"));
        assert_eq!(app.reply_target_label, "cc agent-triage");
        assert!(app.reply_buffer.is_empty());
    }
}

fn draw_cost_overlay(f: &mut Frame, area: Rect, app: &mut AppState) {
    use crate::cost_rollup::{format_usd, short_cwd};
    let dim = || Style::default().fg(Color::DarkGray);
    let bold = || Style::default().add_modifier(Modifier::BOLD);
    let codex = live_codex_token_rollup(&app.sessions);

    // Pull the cached rollup (computes once per overlay-open within the TTL).
    // Clone the few small bits we render so the borrow ends before we touch
    // `app` again to write back total_lines.
    let r = app.cost_rollup_cached();
    let (
        total_today,
        total_7d,
        total_30d,
        total_all,
        scanned_files,
        scan_duration_ms,
        today_label,
        days,
        cwds,
        models,
    ) = (
        r.total_today,
        r.total_7d,
        r.total_30d,
        r.total_all,
        r.scanned_files,
        r.scan_duration_ms,
        r.today.format(),
        r.days.clone(),
        r.cwds.clone(),
        r.models.clone(),
    );

    let mut lines: Vec<Line> = Vec::new();

    // Headline totals — biggest fonts (relative), one per row.
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("today  ", dim()),
        Span::styled(format_usd(total_today), bold().fg(Color::Green)),
        Span::styled(format!("   ({today_label})"), dim()),
    ]));
    let avg7 = if total_7d > 0.0 { total_7d / 7.0 } else { 0.0 };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("7-day  ", dim()),
        Span::styled(format_usd(total_7d), bold()),
        Span::styled(format!("   (avg {}/day)", format_usd(avg7)), dim()),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("30-day ", dim()),
        Span::styled(format_usd(total_30d), bold()),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("all    ", dim()),
        Span::styled(format_usd(total_all), bold()),
        Span::styled(
            format!(
                "   ({scanned_files} session{} · {scan_duration_ms} ms)",
                if scanned_files == 1 { "" } else { "s" }
            ),
            dim(),
        ),
    ]));

    // Last-14-days strip.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  last 14 days",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let want_days = 14;
    let mut day_map: HashMap<crate::cost_rollup::DayKey, f64> =
        days.iter().map(|d| (d.key, d.cost_usd)).collect();
    // Walk back from today.
    let today_key = app.cost_cache.as_ref().unwrap().1.today;
    let mut strip: Vec<(crate::cost_rollup::DayKey, f64)> = Vec::new();
    {
        let today_secs = day_start_secs_from_key(today_key);
        for back in (0..want_days).rev() {
            let secs = today_secs - back as i64 * 86_400;
            let k = day_key_at_secs(secs);
            let cost = day_map.remove(&k).unwrap_or(0.0);
            strip.push((k, cost));
        }
    }
    let peak = strip
        .iter()
        .map(|(_, c)| *c)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let bar_width = 28u16;
    for (k, c) in &strip {
        let filled = ((*c / peak) * bar_width as f64).round() as usize;
        let bar: String = "█".repeat(filled);
        let label = if *k == today_key { "  (today)" } else { "" };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(k.format(), dim()),
            Span::raw("  "),
            Span::styled(format!("{:>8}", format_usd(*c)), bold()),
            Span::raw("  "),
            Span::styled(bar, Style::default().fg(Color::Cyan)),
            Span::styled(label, dim()),
        ]));
    }

    // Top-5 cwds.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  top cwds (all time)",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    let cwd_peak = cwds.first().map(|c| c.cost_usd).unwrap_or(0.01).max(0.01);
    for c in cwds.iter().take(5) {
        let filled = ((c.cost_usd / cwd_peak) * 24.0).round() as usize;
        let bar: String = "█".repeat(filled);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:>8}", format_usd(c.cost_usd)), bold()),
            Span::raw("  "),
            Span::styled(format!("{:>3} sess", c.session_count), dim()),
            Span::raw("  "),
            Span::raw(format!("{:<28}", short_cwd(&c.cwd))),
            Span::styled(bar, Style::default().fg(Color::Magenta)),
        ]));
    }

    // Per-model split.
    if !models.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  by model (all time)",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let total: f64 = models.iter().map(|m| m.cost_usd).sum();
        for m in &models {
            let pct = if total > 0.0 {
                100.0 * m.cost_usd / total
            } else {
                0.0
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<8}", m.model), bold()),
                Span::raw("  "),
                Span::styled(format!("{:>8}", format_usd(m.cost_usd)), bold()),
                Span::styled(format!("   {pct:>4.1}%"), dim()),
            ]));
        }
    }

    if codex.session_count > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  live codex tokens (no dollar data)",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:>8}", format_tokens(codex.total_tokens)), bold()),
            Span::styled(" total", dim()),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} in · {} out · {} cache",
                    format_tokens(codex.tokens_in),
                    format_tokens(codex.tokens_out),
                    format_tokens(codex.cache_read),
                ),
                dim(),
            ),
            Span::styled(
                format!(
                    "   ({} live session{})",
                    codex.session_count,
                    if codex.session_count == 1 { "" } else { "s" },
                ),
                dim(),
            ),
        ]));
        if let Some(peak) = codex.max_context.as_ref() {
            let pct = peak
                .window
                .filter(|window| *window > 0)
                .map(|window| (peak.tokens as f64 / window as f64) * 100.0);
            let pct_style = match pct {
                Some(p) if p >= 95.0 => Style::default().fg(Color::Red),
                Some(p) if p >= 80.0 => Style::default().fg(Color::Yellow),
                _ => dim(),
            };
            let window = peak
                .window
                .map(format_tokens)
                .unwrap_or_else(|| "?".to_string());
            let session_id: String = peak.session_id.chars().take(8).collect();
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("max ctx ", dim()),
                Span::styled(format!("{}/{}", format_tokens(peak.tokens), window), bold()),
                Span::styled(
                    pct.map(|p| format!(" ({p:.0}%)"))
                        .unwrap_or_else(|| " (?%)".to_string()),
                    pct_style,
                ),
                Span::styled("  ", dim()),
                Span::styled(session_id, dim()),
                Span::raw("  "),
                Span::raw(short_cwd(&peak.cwd)),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  top live codex cwds",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let codex_peak = codex
            .top_cwds
            .first()
            .map(|c| c.total_tokens)
            .unwrap_or(1)
            .max(1);
        for c in codex.top_cwds.iter().take(5) {
            let filled = ((c.total_tokens as f64 / codex_peak as f64) * 24.0).round() as usize;
            let bar: String = "█".repeat(filled);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:>8}", format_tokens(c.total_tokens)), bold()),
                Span::raw("  "),
                Span::styled(format!("{:>3} sess", c.session_count), dim()),
                Span::raw("  "),
                Span::raw(format!("{:<28}", short_cwd(&c.cwd))),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
            ]));
        }

        if !codex.models.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  live codex by model",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for m in codex.models.iter().take(5) {
                let pct = if codex.total_tokens > 0 {
                    100.0 * m.total_tokens as f64 / codex.total_tokens as f64
                } else {
                    0.0
                };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("{:<12}", m.model), bold()),
                    Span::raw("  "),
                    Span::styled(format!("{:>8}", format_tokens(m.total_tokens)), bold()),
                    Span::styled(format!("   {pct:>4.1}%"), dim()),
                    Span::styled(format!("  {} sess", m.session_count), dim()),
                ]));
            }
        }
    }

    app.cost_overlay_total_lines = lines.len() as u16;
    let max_offset = app
        .cost_overlay_total_lines
        .saturating_sub(area.height.saturating_sub(2));
    if app.cost_overlay_offset > max_offset {
        app.cost_overlay_offset = max_offset;
    }

    let title = format!(
        " cost  ·  {} Claude sessions  ·  {} live Codex  ·  scanned {} ms ",
        scanned_files, codex.session_count, scan_duration_ms
    );
    let block = Block::default()
        .borders(Borders::TOP)
        .title(Span::styled(title, dim()));
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.cost_overlay_offset, 0));
    f.render_widget(para, area);
}

fn draw_key_help(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Common", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from("  Enter jump to selected pane     r reply     a/d approve or deny"),
        Line::from("  / filter sessions              ? keys      q quit"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Navigation", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from("  Up/Down or j/k move selection  gg top      G bottom"),
        Line::from("  Space toggle detail            Esc closes overlays and edit modes"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Agent Controls",
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from("  A auto mode                    p phone push     m mute"),
        Line::from("  w watch                        * pin to top     R rename"),
        Line::from("  N new agent"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Views", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from("  H audit log                    $ cost overlay"),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Input Modes", Style::default().add_modifier(Modifier::BOLD)),
        ]),
        Line::from("  reply:  Enter send             Esc cancel       ^W word  ^U clear"),
        Line::from("  filter: Enter jump             Esc clear        arrows navigate"),
        Line::from("  rename: Enter save             Esc cancel       ^W word  ^U clear"),
        Line::from("  new:    Enter launch           Esc/q cancel     Up/Down or j/k choose"),
    ];

    let block = Block::default().borders(Borders::TOP).title(Span::styled(
        " keys  (?/Esc/q close) ",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

// Local copies of the day-second helpers (cost_rollup keeps them private to
// the module since their main consumer is `cli_cost`; the overlay uses the
// same arithmetic). Keeping them inline here avoids exposing more surface
// from cost_rollup than necessary.
fn day_start_secs_from_key(d: crate::cost_rollup::DayKey) -> i64 {
    days_from_civil_local(d.year as i64, d.month, d.day) * 86_400
}

fn day_key_at_secs(secs: i64) -> crate::cost_rollup::DayKey {
    crate::cost_rollup::local_day(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs((secs + 1).max(0) as u64),
    )
}

fn days_from_civil_local(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn draw_footer(f: &mut Frame, area: Rect, app: &AppState) {
    if app.key_help_open {
        let hint = "  ?/Esc/q close keys  ·  ^C quit";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
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
    if app.cost_overlay_open {
        let hint = if app.pending_g {
            "  g … press g again to jump to top, any other key cancels".to_string()
        } else {
            "  j/k scroll  ·  ^d/^u half-page  ·  gg top  ·  G bottom  ·  $/Esc close  ·  q quit"
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
    if app.spawn_picker_open {
        let hint = "  ↑↓/j/k choose cwd  ·  ↵ launch  ·  Esc/q cancel  ·  ^C quit";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    if app.reply_active {
        let hint = "  reply  ·  ↵ send  ·  Esc cancel  ·  ^W word  ·  ^U clear";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    if app.filter_active {
        let hint = "  type to filter  ·  ↑↓ nav  ·  ↵ keep  ·  Esc clear  ·  ^W word  ·  ^U line";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    if app.rename_active {
        let hint = "  rename alias  ·  ↵ save  ·  Esc cancel  ·  ^W word  ·  ^U clear";
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    let line = if let Some(msg) = &app.status_msg {
        Line::from(Span::styled(
            msg.clone(),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        let hint = match LayoutMode::from_width(area.width) {
            LayoutMode::Narrow => " ⏎ r a/d / ? q".to_string(),
            LayoutMode::Medium => " ⏎ jump  r reply  a/d approve  / filter  ? keys  q".to_string(),
            LayoutMode::Wide => {
                "  ⏎ jump  r reply  a/d approve  / filter  ? keys  q quit".to_string()
            }
        };
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Case-insensitive substring match against everything a user might want
/// to search by: the Claude session's `--name`, the tmux window name, the
/// tmux session name, and the full cwd path. `q` is already lowercased by
/// the caller. Returning early on any hit keeps this cheap on large fleets.
fn session_matches_filter(s: &Session, q: &str) -> bool {
    if s.provider.label().contains(q) || (s.provider == Provider::Codex && "codex".contains(q)) {
        return true;
    }
    if let Some(name) = &s.name
        && name.to_lowercase().contains(q)
    {
        return true;
    }
    if let Some(pane) = &s.pane {
        if !pane.window_name.is_empty() && pane.window_name.to_lowercase().contains(q) {
            return true;
        }
        if !pane.tmux_session.is_empty() && pane.tmux_session.to_lowercase().contains(q) {
            return true;
        }
    }
    s.cwd.to_string_lossy().to_lowercase().contains(q)
}

fn target_label(s: &Session) -> String {
    format!("{} {}", s.provider.label(), session_display_label(s))
}
