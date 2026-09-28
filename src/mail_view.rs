//! The `M` overlay: peer mail grouped into conversations between agent
//! pairs, with a timeline for the selected one. Also feeds the detail
//! pane's per-agent mail count.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::mailbox::{self, Store};
use crate::messages::Entry;
use crate::models::{Provider, Session};

const RESCAN_EVERY: Duration = Duration::from_secs(5);
const FOLD_BODY_LINES: usize = 6;
const LEGACY_LINES: usize = 200;

#[derive(Default)]
pub struct MailView {
    pub open: bool,
    pub selected: usize,
    pub offset: u16,
    pub total_lines: u16,
    pub pending_only: bool,
    /// `Some` while the overlay is narrowed to one agent's conversations.
    pub agent_filter: Option<String>,
    hovered_agent: Option<String>,
    entries: Vec<Entry>,
    legacy: Vec<String>,
    agent_by_pid: HashMap<u32, String>,
    scanned_at: Option<Instant>,
}

pub struct Conversation {
    pub key: ConversationKey,
    entries: Vec<usize>,
    last_id: String,
    pending: usize,
    undeliverable: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConversationKey {
    Pair(String, String),
    Legacy,
}

impl MailView {
    pub fn due(&self) -> bool {
        self.scanned_at.is_none_or(|t| t.elapsed() >= RESCAN_EVERY)
    }

    pub fn rescan(&mut self, sessions: &[Session]) {
        let store = Store::open_default();
        self.entries = crate::messages::collect(&store);
        self.agent_by_pid = sessions
            .iter()
            .filter_map(|s| {
                crate::agent_comm::session_agent_id(&store, s).map(|agent| (s.pid, agent))
            })
            .collect();
        self.legacy = read_legacy_log();
        self.scanned_at = Some(Instant::now());
        let count = self.conversations().len();
        if self.selected >= count {
            self.selected = count.saturating_sub(1);
        }
    }

    /// `(pending, total)` addressed to the session's agent.
    pub fn counts_for(&self, pid: u32) -> Option<(usize, usize)> {
        let agent = self.agent_by_pid.get(&pid)?;
        let mine = self.entries.iter().filter(|e| &e.msg.to.agent == agent);
        let (mut pending, mut total) = (0, 0);
        for e in mine {
            total += 1;
            if matches!(e.state, "pending" | "inflight") {
                pending += 1;
            }
        }
        (total > 0).then_some((pending, total))
    }

    pub fn open(&mut self, sessions: &[Session], hovered_pid: Option<u32>) {
        self.open = true;
        self.offset = 0;
        self.rescan(sessions);
        self.hovered_agent = hovered_pid.and_then(|pid| self.agent_by_pid.get(&pid).cloned());
    }

    pub fn close(&mut self) {
        self.open = false;
        self.offset = 0;
    }

    pub fn toggle_agent_filter(&mut self) {
        self.agent_filter = match self.agent_filter {
            Some(_) => None,
            None => self.hovered_agent.clone(),
        };
        self.selected = 0;
        self.offset = 0;
    }

    pub fn toggle_pending_only(&mut self) {
        self.pending_only = !self.pending_only;
        self.selected = 0;
        self.offset = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.conversations().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.offset = 0;
    }

    fn visible(&self, e: &Entry) -> bool {
        (!self.pending_only || matches!(e.state, "pending" | "inflight" | "undeliverable"))
            && self
                .agent_filter
                .as_ref()
                .is_none_or(|a| &e.msg.from.agent == a || &e.msg.to.agent == a)
    }

    pub fn conversations(&self) -> Vec<Conversation> {
        let mut by_pair: HashMap<ConversationKey, Conversation> = HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            if !self.visible(e) {
                continue;
            }
            let (a, b) = if e.msg.from.agent <= e.msg.to.agent {
                (e.msg.from.agent.clone(), e.msg.to.agent.clone())
            } else {
                (e.msg.to.agent.clone(), e.msg.from.agent.clone())
            };
            let key = ConversationKey::Pair(a, b);
            let conv = by_pair.entry(key.clone()).or_insert_with(|| Conversation {
                key,
                entries: Vec::new(),
                last_id: String::new(),
                pending: 0,
                undeliverable: 0,
            });
            conv.entries.push(i);
            conv.last_id = conv.last_id.clone().max(e.msg.id.clone());
            match e.state {
                "pending" | "inflight" => conv.pending += 1,
                "undeliverable" => conv.undeliverable += 1,
                _ => {}
            }
        }
        let mut conversations: Vec<Conversation> = by_pair.into_values().collect();
        conversations.sort_by(|x, y| y.last_id.cmp(&x.last_id));
        if !self.legacy.is_empty() && !self.pending_only && self.agent_filter.is_none() {
            conversations.push(Conversation {
                key: ConversationKey::Legacy,
                entries: Vec::new(),
                last_id: String::new(),
                pending: 0,
                undeliverable: 0,
            });
        }
        conversations
    }

    /// The pane to jump to: the other party of the selected conversation's
    /// latest message (its recipient, unless that is the hovered agent).
    pub fn jump_pid(&self) -> Option<u32> {
        let conversations = self.conversations();
        let conv = conversations.get(self.selected)?;
        let latest = self.entries.get(*conv.entries.last()?)?;
        let target = if Some(&latest.msg.to.agent) == self.hovered_agent.as_ref() {
            &latest.msg.from.agent
        } else {
            &latest.msg.to.agent
        };
        self.agent_by_pid
            .iter()
            .find(|(_, agent)| *agent == target)
            .map(|(pid, _)| *pid)
    }

    fn labels(&self) -> HashMap<&str, (String, Provider)> {
        self.entries
            .iter()
            .map(|e| {
                (
                    e.msg.from.agent.as_str(),
                    (
                        mailbox::display_label(&e.msg.from.label).to_string(),
                        e.msg.from.provider,
                    ),
                )
            })
            .collect()
    }
}

fn read_legacy_log() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let path = std::path::PathBuf::from(home).join(".config/triage/agent-messages.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LEGACY_LINES)..]
        .iter()
        .rev()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            format!(
                "{}  {} → {}  [{}]  {}",
                v.get("ts")
                    .and_then(|t| t.as_u64())
                    .map_or(String::new(), |t| clock(t * 1000)),
                s("from"),
                s("target_name"),
                s("verdict"),
                s("message_preview")
            )
        })
        .collect()
}

fn clock(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    format!(
        "{:02}-{:02} {:02}:{:02}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn provider_tag(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "cc",
        Provider::Codex => "cx",
    }
}

pub fn draw(f: &mut Frame, area: Rect, view: &mut MailView) {
    let dim = Style::default().fg(Color::DarkGray);
    let conversations = view.conversations();
    let labels = view.labels();
    let name = |agent: &str| match labels.get(agent) {
        Some((label, provider)) => format!(
            "{label} {} {}",
            provider_tag(*provider),
            mailbox::short_id(agent)
        ),
        None => mailbox::short_id(agent).to_string(),
    };

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let mut left: Vec<Line> = Vec::new();
    if conversations.is_empty() {
        left.push(Line::from(Span::styled("no peer mail", dim)));
    }
    for (i, conv) in conversations.iter().enumerate() {
        let mut spans = Vec::new();
        if conv.pending > 0 {
            spans.push(Span::styled(
                format!("● {} ", conv.pending),
                Style::default().fg(Color::Yellow),
            ));
        }
        if conv.undeliverable > 0 {
            spans.push(Span::styled(
                format!("✕ {} ", conv.undeliverable),
                Style::default().fg(Color::Red),
            ));
        }
        match &conv.key {
            ConversationKey::Pair(a, b) => {
                spans.push(Span::styled(format!("{} ", conv.entries.len()), dim));
                spans.push(Span::raw(format!("{} ↔ {}", name(a), name(b))));
            }
            ConversationKey::Legacy => {
                spans.push(Span::styled(format!("{} ", view.legacy.len()), dim));
                spans.push(Span::raw("legacy paste log"));
            }
        }
        let mut line = Line::from(spans);
        if i == view.selected {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        left.push(line);
    }
    let mut title = String::from(" conversations ");
    if view.pending_only {
        title.push_str("· pending only ");
    }
    if let Some(agent) = &view.agent_filter {
        title.push_str(&format!("· {} ", mailbox::short_id(agent)));
    }
    f.render_widget(
        Paragraph::new(left).block(
            Block::default()
                .borders(Borders::TOP | Borders::RIGHT)
                .title(Span::styled(title, dim)),
        ),
        columns[0],
    );

    let mut right: Vec<Line> = Vec::new();
    match conversations.get(view.selected) {
        None => {}
        Some(Conversation {
            key: ConversationKey::Legacy,
            ..
        }) => {
            for line in &view.legacy {
                right.push(Line::from(Span::raw(line.clone())));
            }
        }
        Some(conv) => {
            for &i in conv.entries.iter().rev() {
                let e = &view.entries[i];
                let state = match (e.state, e.msg.delivered_via, e.msg.delivered_at_ms) {
                    ("delivered" | "read", Some(via), Some(at)) => format!(
                        "delivered via {} · {}s",
                        serde_json::to_value(via)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default(),
                        at.saturating_sub(e.msg.created_at_ms) / 1000
                    ),
                    ("pasted", _, _) => "pasted (legacy)".to_string(),
                    (state, _, _) => state.to_string(),
                };
                let color = match e.state {
                    "pending" | "inflight" | "notified" => Color::Yellow,
                    "undeliverable" => Color::Red,
                    _ => Color::DarkGray,
                };
                right.push(Line::from(vec![
                    Span::styled(clock(e.msg.created_at_ms), dim),
                    Span::raw(format!(
                        "  {} → {}  ",
                        name(&e.msg.from.agent),
                        name(&e.msg.to.agent)
                    )),
                    Span::styled(format!("[{state}]"), Style::default().fg(color)),
                ]));
                let body: Vec<&str> = e.msg.body.lines().collect();
                for line in body.iter().take(FOLD_BODY_LINES) {
                    right.push(Line::from(Span::raw(format!("    {line}"))));
                }
                if body.len() > FOLD_BODY_LINES {
                    right.push(Line::from(Span::styled(
                        format!(
                            "    … {} more lines (triage inbox show {})",
                            body.len() - FOLD_BODY_LINES,
                            e.msg.id
                        ),
                        dim,
                    )));
                }
                right.push(Line::from(""));
            }
        }
    }
    view.total_lines = right.len() as u16;
    let max_offset = view
        .total_lines
        .saturating_sub(columns[1].height.saturating_sub(2));
    view.offset = view.offset.min(max_offset);
    f.render_widget(
        Paragraph::new(right)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .title(Span::styled(" timeline · newest first ", dim)),
            )
            .wrap(Wrap { trim: false })
            .scroll((view.offset, 0)),
        columns[1],
    );
}

pub const FOOTER_HINT: &str = "  j/k conversation  ·  ^d/^u scroll  ·  p pending only  ·  a this agent / all  ·  ⏎ jump  ·  M/Esc close  ·  q quit";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{Message, Recipient, Sender};

    const A: &str = "aaaaaaaa-0000-4000-8000-0000000000a1";
    const B: &str = "bbbbbbbb-0000-4000-8000-0000000000b2";
    const C: &str = "cccccccc-0000-4000-8000-0000000000c3";

    fn entry(n: u8, from: &str, label: &str, to: &str, state: &'static str, body: &str) -> Entry {
        Entry {
            state,
            msg: Message {
                v: 1,
                id: format!("01900000-0000-7000-8000-0000000000{n:02}"),
                created_at_ms: 1_790_000_000_000 + u64::from(n) * 1000,
                from: Sender {
                    agent: from.into(),
                    session: from.into(),
                    provider: Provider::Claude,
                    label: label.into(),
                    send_message_name: None,
                },
                to: Recipient {
                    agent: to.into(),
                    session_at_send: to.into(),
                },
                body: body.into(),
                attempt: 0,
                bounce_of: None,
                delivered_at_ms: None,
                delivered_via: None,
            },
            unlinked: false,
        }
    }

    fn view() -> MailView {
        MailView {
            entries: vec![
                entry(1, A, "TRI-1", B, "delivered", "hello b"),
                entry(2, B, "TRI-2", A, "pending", "reply to a"),
                entry(3, A, "TRI-1", C, "undeliverable", "gone"),
                entry(4, C, "TRI-3", B, "delivered", "newest"),
            ],
            agent_by_pid: HashMap::from([(10, A.into()), (20, B.into()), (30, C.into())]),
            hovered_agent: Some(A.into()),
            ..MailView::default()
        }
    }

    fn keys(v: &MailView) -> Vec<ConversationKey> {
        v.conversations().into_iter().map(|c| c.key).collect()
    }

    #[test]
    fn both_directions_form_one_conversation_newest_first() {
        let v = view();
        let conversations = v.conversations();
        assert_eq!(
            keys(&v),
            [
                ConversationKey::Pair(B.into(), C.into()),
                ConversationKey::Pair(A.into(), C.into()),
                ConversationKey::Pair(A.into(), B.into()),
            ]
        );
        let ab = &conversations[2];
        assert_eq!((ab.entries.len(), ab.pending), (2, 1));
        assert_eq!(conversations[1].undeliverable, 1);
    }

    #[test]
    fn filters_narrow_to_attention_and_to_the_hovered_agent() {
        let mut v = view();
        v.toggle_pending_only();
        assert_eq!(
            keys(&v),
            [
                ConversationKey::Pair(A.into(), C.into()),
                ConversationKey::Pair(A.into(), B.into()),
            ]
        );
        v.toggle_pending_only();
        v.toggle_agent_filter();
        assert_eq!(v.agent_filter.as_deref(), Some(A));
        assert!(!keys(&v).contains(&ConversationKey::Pair(B.into(), C.into())));
        v.toggle_agent_filter();
        assert_eq!(v.conversations().len(), 3);
    }

    #[test]
    fn counts_mail_addressed_to_the_sessions_agent() {
        let v = view();
        assert_eq!(v.counts_for(10), Some((1, 1)));
        assert_eq!(v.counts_for(20), Some((0, 2)));
        assert_eq!(v.counts_for(99), None);
    }

    #[test]
    fn jump_goes_to_the_other_party_of_the_latest_message() {
        let mut v = view();
        v.selected = 2;
        assert_eq!(
            v.jump_pid(),
            Some(20),
            "latest A↔B message is B → A; A is hovered"
        );
        v.selected = 0;
        assert_eq!(v.jump_pid(), Some(20), "C → B: jump to the recipient");
    }

    #[test]
    fn renders_conversations_badges_and_timeline() {
        let mut v = view();
        v.selected = 2;
        let backend = ratatui::backend::TestBackend::new(140, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &mut v))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("TRI-1 cc 000000a1 ↔ TRI-2 cc 00000"),
            "{rendered}"
        );
        assert!(
            rendered.contains("● 1 2 TRI-1"),
            "badge before names: {rendered}"
        );
        assert!(rendered.contains("✕ 1 1 TRI-1"), "{rendered}");
        assert!(rendered.contains("reply to a"));
    }
}
