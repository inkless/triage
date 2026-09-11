use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde_json::Value;

use crate::models::{Pane, Provider, Session};
use crate::tmux;
use crate::transcript::parse_timestamp;

pub fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".codex/sessions")
}

#[derive(Default)]
pub struct CodexDigestCache {
    entries: HashMap<PathBuf, (SystemTime, Option<CodexDigest>)>,
    thread_titles: Option<(CodexStateStamp, HashMap<String, CodexThreadTitle>)>,
    thread_roots: Option<(CodexStateStamp, HashMap<String, String>)>,
}

impl CodexDigestCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&mut self, path: &Path) -> Option<CodexDigest> {
        let meta = fs::metadata(path).ok()?;
        if meta.len() == 0 {
            return None;
        }
        let mtime = meta.modified().ok()?;
        if let Some((cached_mtime, cached)) = self.entries.get(path)
            && *cached_mtime == mtime
        {
            return cached.clone();
        }
        let text = fs::read_to_string(path).ok()?;
        let digest = parse_rollout(path, &text, mtime);
        self.entries
            .insert(path.to_path_buf(), (mtime, digest.clone()));
        digest
    }

    pub fn evict_missing(&mut self) {
        self.entries.retain(|p, _| p.exists());
    }

    fn thread_titles(&mut self) -> HashMap<String, CodexThreadTitle> {
        let Some(path) = codex_state_path() else {
            return HashMap::new();
        };
        let Some(stamp) = CodexStateStamp::for_path(&path) else {
            self.thread_titles = None;
            return HashMap::new();
        };
        if let Some((cached_stamp, cached)) = &self.thread_titles
            && *cached_stamp == stamp
        {
            return cached.clone();
        }
        let titles = load_thread_titles(&path);
        self.thread_titles = Some((stamp, titles.clone()));
        titles
    }

    fn thread_roots(&mut self) -> HashMap<String, String> {
        let Some(path) = codex_state_path() else {
            return HashMap::new();
        };
        let Some(stamp) = CodexStateStamp::for_path(&path) else {
            self.thread_roots = None;
            return HashMap::new();
        };
        if let Some((cached_stamp, cached)) = &self.thread_roots
            && *cached_stamp == stamp
        {
            return cached.clone();
        }
        let roots = load_thread_roots(&path);
        self.thread_roots = Some((stamp, roots.clone()));
        roots
    }
}

pub fn discover_live_sessions(
    panes: &HashMap<u32, Pane>,
    ppid_map: &HashMap<u32, u32>,
    cache: &mut CodexDigestCache,
) -> Vec<Session> {
    let mut out = Vec::new();
    let thread_titles = cache.thread_titles();
    let thread_roots = cache.thread_roots();
    for pid in codex_pids() {
        let Some((path, digest)) = select_rollout(rollout_paths_for_pid(pid), cache) else {
            continue;
        };
        let cwd = digest
            .cwd
            .clone()
            .or_else(|| panes.values().find(|p| p.pid == pid).map(|p| p.cwd.clone()))
            .unwrap_or_default();
        let started_at_ms = digest.started_at_ms;
        let updated_at_ms = digest.updated_at_ms.max(started_at_ms);
        let name = thread_titles
            .get(&digest.session_id)
            .and_then(CodexThreadTitle::display_label)
            .or_else(|| {
                codex_agent_label(
                    digest.agent_nickname.as_deref(),
                    digest.agent_role.as_deref(),
                )
            });
        let mut session = Session::new(
            Provider::Codex,
            pid,
            digest.session_id.clone(),
            cwd,
            name,
            digest.status(),
            started_at_ms,
            updated_at_ms,
            None,
        );
        session.alias_session_id = thread_roots
            .get(&digest.session_id)
            .cloned()
            .or_else(|| Some(digest.session_id.clone()));
        session.pane = tmux::find_owning_pane(pid, panes, ppid_map, 8);
        session.transcript_path = Some(path);
        session.headline = digest.headline.clone();
        session.last_prompt = digest.last_prompt.clone();
        session.last_prompt_at = digest.last_prompt_at;
        session.last_event_at = digest.last_event_at;
        session.last_stop_at = digest.last_stop_at();
        session.user_prompt_count = digest.user_prompt_count;
        session.last_tool_use = digest.last_tool_use.clone();
        session.approval_prompt_pending = digest.pending_approval_tool;
        session.total_tokens_in = digest.total_tokens_in;
        session.total_tokens_out = digest.total_tokens_out;
        session.total_tokens_cache_read = digest.total_tokens_cache_read;
        session.latest_context_tokens = digest.latest_context_tokens;
        session.context_window = digest.context_window;
        session.peak_context_tokens = digest.peak_context_tokens;
        session.latest_model = digest.latest_model.clone();
        session.latest_assistant_text = digest.latest_assistant_text.clone();
        out.push(session);
    }
    out
}

fn codex_state_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".codex/state_5.sqlite"))
}

pub fn load_thread_roots_for_aliases() -> HashMap<String, String> {
    let Some(path) = codex_state_path() else {
        return HashMap::new();
    };
    load_thread_roots(&path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexStateStamp {
    db_mtime: Option<SystemTime>,
    db_len: u64,
    wal_mtime: Option<SystemTime>,
    wal_len: u64,
}

impl CodexStateStamp {
    fn for_path(path: &Path) -> Option<Self> {
        let db = fs::metadata(path).ok()?;
        let wal_path = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        let wal = fs::metadata(wal_path).ok();
        Some(Self {
            db_mtime: db.modified().ok(),
            db_len: db.len(),
            wal_mtime: wal.as_ref().and_then(|m| m.modified().ok()),
            wal_len: wal.map(|m| m.len()).unwrap_or(0),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CodexThreadTitle {
    title: Option<String>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
}

impl CodexThreadTitle {
    fn display_label(&self) -> Option<String> {
        self.title
            .as_deref()
            .and_then(normalize_codex_label)
            .or_else(|| {
                codex_agent_label(self.agent_nickname.as_deref(), self.agent_role.as_deref())
            })
    }
}

fn load_thread_titles(path: &Path) -> HashMap<String, CodexThreadTitle> {
    let Ok(out) = Command::new("sqlite3")
        .args([
            "-readonly",
            "-json",
            &path.to_string_lossy(),
            "select id, title, agent_nickname, agent_role from threads where archived = 0",
        ])
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    parse_thread_titles_json(&out.stdout)
}

fn load_thread_roots(path: &Path) -> HashMap<String, String> {
    let Ok(out) = Command::new("sqlite3")
        .args([
            "-readonly",
            "-json",
            &path.to_string_lossy(),
            "select parent_thread_id, child_thread_id from thread_spawn_edges",
        ])
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    parse_thread_roots_json(&out.stdout)
}

fn parse_thread_roots_json(bytes: &[u8]) -> HashMap<String, String> {
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(bytes) else {
        return HashMap::new();
    };
    let mut parent_by_child = HashMap::new();
    let mut ids = HashSet::new();
    for row in rows {
        let Some(parent) = row.get("parent_thread_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(child) = row.get("child_thread_id").and_then(|v| v.as_str()) else {
            continue;
        };
        parent_by_child.insert(child.to_string(), parent.to_string());
        ids.insert(parent.to_string());
        ids.insert(child.to_string());
    }
    ids.into_iter()
        .map(|id| {
            let root = resolve_thread_root(&id, &parent_by_child);
            (id, root)
        })
        .collect()
}

fn resolve_thread_root(id: &str, parent_by_child: &HashMap<String, String>) -> String {
    let mut cur = id.to_string();
    let mut seen = HashSet::new();
    while let Some(parent) = parent_by_child.get(&cur) {
        if !seen.insert(cur.clone()) {
            break;
        }
        cur = parent.clone();
    }
    cur
}

fn parse_thread_titles_json(bytes: &[u8]) -> HashMap<String, CodexThreadTitle> {
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(bytes) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        out.insert(
            id.to_string(),
            CodexThreadTitle {
                title: row
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                agent_nickname: row
                    .get("agent_nickname")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
                agent_role: row
                    .get("agent_role")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string),
            },
        );
    }
    out
}

fn codex_agent_label(nickname: Option<&str>, role: Option<&str>) -> Option<String> {
    let nickname = nickname.and_then(normalize_codex_label);
    let role = role.and_then(normalize_codex_label);
    match (nickname, role) {
        (Some(nick), Some(role)) => Some(format!("{nick} ({role})")),
        (Some(nick), None) => Some(nick),
        (None, Some(role)) => Some(role),
        (None, None) => None,
    }
}

fn normalize_codex_label(raw: &str) -> Option<String> {
    let label = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        return None;
    }
    Some(crate::approval::truncate(&label, 80))
}

fn codex_pids() -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,comm="]).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        let Some((pid_raw, command)) = split_first_ws(line) else {
            continue;
        };
        let Ok(pid) = pid_raw.parse::<u32>() else {
            continue;
        };
        if command_basename(command) == Some("codex") {
            pids.push(pid);
        }
    }
    pids
}

fn rollout_paths_for_pid(pid: u32) -> Vec<PathBuf> {
    let Ok(out) = Command::new("lsof")
        .args(["-Fn", "-p", &pid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter(|path| is_codex_rollout(path))
        .map(PathBuf::from)
        .collect()
}

fn select_rollout(
    paths: Vec<PathBuf>,
    cache: &mut CodexDigestCache,
) -> Option<(PathBuf, CodexDigest)> {
    paths
        .into_iter()
        .filter_map(|path| cache.get(&path).map(|digest| (path, digest)))
        .max_by_key(|(_, digest)| (digest.is_user_thread, digest.updated_at_ms))
}

fn is_codex_rollout(path: &str) -> bool {
    path.contains("/.codex/sessions/")
        && path.ends_with(".jsonl")
        && Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-"))
}

#[derive(Clone)]
struct CodexDigest {
    is_user_thread: bool,
    session_id: String,
    cwd: Option<PathBuf>,
    started_at_ms: u64,
    updated_at_ms: u64,
    headline: Option<String>,
    last_prompt: Option<String>,
    last_prompt_at: Option<SystemTime>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    last_event_at: Option<SystemTime>,
    latest_assistant_at: Option<SystemTime>,
    user_prompt_count: u64,
    last_tool_use: Option<(String, String)>,
    pending_tool: bool,
    pending_approval_tool: bool,
    total_tokens_in: u64,
    total_tokens_out: u64,
    total_tokens_cache_read: u64,
    latest_context_tokens: u64,
    context_window: Option<u64>,
    peak_context_tokens: u64,
    latest_model: Option<String>,
    latest_assistant_text: Option<String>,
    latest_kind: LatestKind,
    turn_active: Option<bool>,
}

impl CodexDigest {
    fn status(&self) -> String {
        if let Some(active) = self.turn_active {
            return if active { "busy" } else { "idle" }.to_string();
        }
        if self.pending_tool
            || matches!(
                self.latest_kind,
                LatestKind::User
                    | LatestKind::TaskStarted
                    | LatestKind::FunctionCall
                    | LatestKind::FunctionOutput
                    | LatestKind::Commentary
                    | LatestKind::Reasoning
            )
        {
            "busy".to_string()
        } else {
            "idle".to_string()
        }
    }

    fn last_stop_at(&self) -> Option<SystemTime> {
        (self.status() == "idle")
            .then_some(self.latest_assistant_at)
            .flatten()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LatestKind {
    None,
    User,
    TaskStarted,
    Assistant,
    Commentary,
    Reasoning,
    FunctionCall,
    FunctionOutput,
}

fn parse_rollout(path: &Path, text: &str, mtime: SystemTime) -> Option<CodexDigest> {
    let mut is_user_thread = false;
    let mut session_id = String::new();
    let mut cwd: Option<PathBuf> = None;
    let mut started_at: Option<SystemTime> = None;
    let mut last_event_at: Option<SystemTime> = None;
    let mut last_prompt: Option<String> = None;
    let mut last_prompt_at: Option<SystemTime> = None;
    let mut agent_nickname: Option<String> = None;
    let mut agent_role: Option<String> = None;
    let mut user_prompt_count = 0;
    let mut latest_assistant: Option<(SystemTime, String)> = None;
    let mut latest_model: Option<String> = None;
    let mut total_tokens_in = 0;
    let mut total_tokens_out = 0;
    let mut total_tokens_cache_read = 0;
    let mut latest_context_tokens = 0;
    let mut peak_context_tokens = 0;
    let mut context_window = None;
    let mut function_calls: Vec<CodexFunctionCall> = Vec::new();
    let mut completed_calls: HashSet<String> = HashSet::new();
    let mut turn_active = None;
    let mut latest_kind = LatestKind::None;
    let mut latest_kind_at: Option<SystemTime> = None;

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp);
        if let Some(t) = ts {
            if started_at.is_none() {
                started_at = Some(t);
            }
            if last_event_at.is_none_or(|prev| t > prev) {
                last_event_at = Some(t);
            }
        }
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "session_meta" => {
                if let Some(payload) = v.get("payload") {
                    if payload.get("thread_source").and_then(Value::as_str)
                        == Some("guardian_review")
                        || payload
                            .pointer("/source/subagent/other")
                            .and_then(Value::as_str)
                            == Some("guardian")
                    {
                        return None;
                    }
                    is_user_thread = payload.get("thread_source").and_then(Value::as_str)
                        == Some("user")
                        || matches!(
                            payload.get("source").and_then(Value::as_str),
                            Some("cli" | "vscode")
                        );
                    if let Some(id) = payload.get("id").and_then(|id| id.as_str()) {
                        session_id = id.to_string();
                    }
                    if let Some(c) = payload.get("cwd").and_then(|c| c.as_str()) {
                        cwd = Some(PathBuf::from(c));
                    }
                    if let Some(n) = payload.get("agent_nickname").and_then(|n| n.as_str()) {
                        agent_nickname = Some(n.to_string());
                    }
                    if let Some(r) = payload.get("agent_role").and_then(|r| r.as_str()) {
                        agent_role = Some(r.to_string());
                    }
                    if let Some(t) = payload
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(parse_timestamp)
                    {
                        started_at = Some(t);
                    }
                }
            }
            "turn_context" => {
                if let Some(payload) = v.get("payload") {
                    if let Some(c) = payload.get("cwd").and_then(|c| c.as_str()) {
                        cwd = Some(PathBuf::from(c));
                    }
                    if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
                        latest_model = Some(model.to_string());
                    }
                }
            }
            "event_msg" => {
                if let Some(payload) = v.get("payload") {
                    match payload.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "task_complete" | "turn_aborted" => {
                            turn_active = Some(false);
                            function_calls.clear();
                            completed_calls.clear();
                        }
                        "task_started" => {
                            turn_active = Some(true);
                            function_calls.clear();
                            completed_calls.clear();
                            if let Some(n) =
                                payload.get("model_context_window").and_then(|n| n.as_u64())
                            {
                                context_window = Some(n);
                            }
                            mark_latest(
                                &mut latest_kind,
                                &mut latest_kind_at,
                                LatestKind::TaskStarted,
                                ts,
                            );
                        }
                        "user_message" => {
                            if turn_active.is_some() {
                                turn_active = Some(true);
                            }
                            if let Some(message) = payload.get("message").and_then(|m| m.as_str()) {
                                last_prompt = Some(message.to_string());
                                last_prompt_at = ts;
                                user_prompt_count += 1;
                                mark_latest(
                                    &mut latest_kind,
                                    &mut latest_kind_at,
                                    LatestKind::User,
                                    ts,
                                );
                            }
                        }
                        "agent_message" => {
                            if let Some(message) = payload.get("message").and_then(|m| m.as_str())
                                && let Some(t) = ts
                            {
                                let kind = assistant_kind(payload, &mut turn_active);
                                update_assistant(&mut latest_assistant, t, message.to_string());
                                mark_latest(&mut latest_kind, &mut latest_kind_at, kind, ts);
                            }
                        }
                        "token_count" => {
                            if let Some(info) = payload.get("info") {
                                read_token_count(
                                    info,
                                    &mut total_tokens_in,
                                    &mut total_tokens_out,
                                    &mut total_tokens_cache_read,
                                    &mut latest_context_tokens,
                                    &mut peak_context_tokens,
                                    &mut context_window,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = v.get("payload") {
                    match payload.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "message" => {
                            let role = payload.get("role").and_then(|r| r.as_str());
                            if role == Some("assistant")
                                && let Some(content) = payload.get("content")
                                && let Some(text) = content_text(content, "output_text")
                                && let Some(t) = ts
                            {
                                let kind = assistant_kind(payload, &mut turn_active);
                                update_assistant(&mut latest_assistant, t, text);
                                mark_latest(&mut latest_kind, &mut latest_kind_at, kind, ts);
                            }
                        }
                        "reasoning" => {
                            mark_latest(
                                &mut latest_kind,
                                &mut latest_kind_at,
                                LatestKind::Reasoning,
                                ts,
                            );
                        }
                        "function_call" | "custom_tool_call" => {
                            let call_id = payload
                                .get("call_id")
                                .and_then(|id| id.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = payload
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("?")
                                .to_string();
                            let arguments = payload
                                .get("arguments")
                                .or_else(|| payload.get("input"))
                                .and_then(|a| a.as_str())
                                .unwrap_or_default();
                            let brief = brief_codex_arguments(arguments);
                            let requires_approval = codex_arguments_require_approval(arguments);
                            function_calls.push(CodexFunctionCall {
                                call_id,
                                name,
                                brief,
                                requires_approval,
                            });
                            mark_latest(
                                &mut latest_kind,
                                &mut latest_kind_at,
                                LatestKind::FunctionCall,
                                ts,
                            );
                        }
                        "function_call_output" | "custom_tool_call_output" => {
                            if let Some(call_id) = payload.get("call_id").and_then(|id| id.as_str())
                            {
                                completed_calls.insert(call_id.to_string());
                            }
                            mark_latest(
                                &mut latest_kind,
                                &mut latest_kind_at,
                                LatestKind::FunctionOutput,
                                ts,
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if session_id.is_empty() {
        session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("codex")
            .to_string();
    }

    let pending_call = function_calls
        .iter()
        .rev()
        .find(|call| call.call_id.is_empty() || !completed_calls.contains(&call.call_id));
    let last_tool_use = pending_call.map(|call| (call.name.clone(), call.brief.clone()));
    let pending_tool = pending_call.is_some();
    let pending_approval_tool = pending_call.is_some_and(|call| call.requires_approval);
    let started_at_ms = started_at
        .or(last_event_at)
        .map(system_time_ms)
        .unwrap_or(0);
    let updated_at_ms = last_event_at
        .map(system_time_ms)
        .unwrap_or_else(|| system_time_ms(mtime));
    let latest_assistant_text = latest_assistant.as_ref().map(|(_, text)| text.clone());
    let prompt_is_newer = last_prompt_at.is_some_and(|prompt_at| {
        latest_assistant
            .as_ref()
            .is_none_or(|(assistant_at, _)| prompt_at > *assistant_at)
    });
    let headline = if prompt_is_newer {
        last_prompt.as_ref().map(|prompt| format!("→ {prompt}"))
    } else {
        latest_assistant_text
            .clone()
            .or_else(|| last_prompt.clone())
    };
    let headline = headline.and_then(|text| {
        let visible = text
            .split("<oai-mem-citation>")
            .next()
            .unwrap_or_default()
            .trim();
        (!visible.is_empty()).then(|| visible.to_string())
    });
    let latest_assistant_at = latest_assistant.map(|(t, _)| t);

    Some(CodexDigest {
        is_user_thread,
        session_id,
        cwd,
        started_at_ms,
        updated_at_ms,
        headline,
        last_prompt,
        last_prompt_at,
        agent_nickname,
        agent_role,
        last_event_at,
        latest_assistant_at,
        user_prompt_count,
        last_tool_use,
        pending_tool,
        pending_approval_tool,
        total_tokens_in,
        total_tokens_out,
        total_tokens_cache_read,
        latest_context_tokens,
        context_window,
        peak_context_tokens,
        latest_model,
        latest_assistant_text,
        latest_kind,
        turn_active,
    })
}

fn assistant_kind(payload: &Value, turn_active: &mut Option<bool>) -> LatestKind {
    match payload.get("phase").and_then(Value::as_str) {
        Some("commentary") => LatestKind::Commentary,
        Some("final_answer") => {
            *turn_active = Some(false);
            LatestKind::Assistant
        }
        _ => LatestKind::Assistant,
    }
}

struct CodexFunctionCall {
    call_id: String,
    name: String,
    brief: String,
    requires_approval: bool,
}

fn mark_latest(
    latest_kind: &mut LatestKind,
    latest_kind_at: &mut Option<SystemTime>,
    kind: LatestKind,
    ts: Option<SystemTime>,
) {
    let Some(t) = ts else {
        return;
    };
    if latest_kind_at.is_none_or(|prev| t >= prev) {
        *latest_kind = kind;
        *latest_kind_at = Some(t);
    }
}

fn update_assistant(latest: &mut Option<(SystemTime, String)>, ts: SystemTime, text: String) {
    if text.trim().is_empty() {
        return;
    }
    if latest.as_ref().is_none_or(|(prev, _)| ts >= *prev) {
        *latest = Some((ts, text));
    }
}

fn read_token_count(
    info: &Value,
    total_tokens_in: &mut u64,
    total_tokens_out: &mut u64,
    total_tokens_cache_read: &mut u64,
    latest_context_tokens: &mut u64,
    peak_context_tokens: &mut u64,
    context_window: &mut Option<u64>,
) {
    if let Some(total) = info.get("total_token_usage") {
        *total_tokens_in = total
            .get("input_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(*total_tokens_in);
        let output = total
            .get("output_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let reasoning = total
            .get("reasoning_output_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        *total_tokens_out = output + reasoning;
        *total_tokens_cache_read = total
            .get("cached_input_tokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(*total_tokens_cache_read);
    }
    if let Some(last) = info.get("last_token_usage")
        && let Some(input) = last.get("input_tokens").and_then(|n| n.as_u64())
    {
        *latest_context_tokens = input;
        if input > *peak_context_tokens {
            *peak_context_tokens = input;
        }
    }
    if let Some(n) = info.get("model_context_window").and_then(|n| n.as_u64()) {
        *context_window = Some(n);
    }
}

fn content_text(content: &Value, block_type: &str) -> Option<String> {
    if let Some(s) = content.as_str()
        && !s.trim().is_empty()
    {
        return Some(s.to_string());
    }
    let parts: Vec<String> = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(|t| t.as_str()) == Some(block_type))
        .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
        .filter(|text| !text.trim().is_empty())
        .map(ToString::to_string)
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn brief_codex_arguments(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => {
            if let Some(cmd) = value.get("cmd").and_then(|cmd| cmd.as_str()) {
                return crate::approval::truncate(cmd, 400);
            }
            crate::approval::brief_tool_input(Some(&value))
        }
        Err(_) => crate::approval::truncate(arguments, 200),
    }
}

fn codex_arguments_require_approval(arguments: &str) -> bool {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .as_ref()
        .is_some_and(value_requires_approval)
}

fn value_requires_approval(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map
                .get("sandbox_permissions")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v == "require_escalated")
            {
                return true;
            }
            map.values().any(value_requires_approval)
        }
        Value::Array(values) => values.iter().any(value_requires_approval),
        _ => false,
    }
}

fn split_first_ws(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(char::is_whitespace)?;
    let (first, rest) = s.split_at(idx);
    Some((first, rest.trim()))
}

fn command_basename(command: &str) -> Option<&str> {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .or_else(|| command.split_whitespace().next())
}

fn system_time_ms(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_turn_stays_busy_through_commentary_and_code_mode_tools() {
        let events = [
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Checking it"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"code-1","name":"exec","input":"await tools.exec_command({cmd: 'cargo test'})"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"code-1","output":"ok"}}"#,
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Still investigating"}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning"}}"#,
        ];
        let mut text = String::new();
        for (index, event) in events.iter().enumerate() {
            let mut event: Value = serde_json::from_str(event).unwrap();
            event["timestamp"] = format!("2026-09-10T16:00:{index:02}Z").into();
            text.push_str(&format!("{event}\n"));
            let digest =
                parse_rollout(Path::new("rollout.jsonl"), &text, SystemTime::UNIX_EPOCH).unwrap();
            assert_eq!(digest.status(), "busy", "event {index}");
            assert_eq!(digest.last_stop_at(), None);
            assert_eq!(digest.pending_tool, index == 2);
            if index == 2 {
                assert_eq!(digest.last_tool_use.as_ref().unwrap().0, "exec");
                assert!(
                    digest
                        .last_tool_use
                        .as_ref()
                        .unwrap()
                        .1
                        .contains("cargo test")
                );
            }
        }
        for terminal in [
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Finished"}]}}"#,
        ] {
            let mut event: Value = serde_json::from_str(terminal).unwrap();
            event["timestamp"] = "2026-09-10T16:00:10Z".into();
            let ended = format!("{text}{event}\n");
            assert_eq!(
                parse_rollout(Path::new("rollout.jsonl"), &ended, SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .status(),
                "idle"
            );
            let restarted = format!(
                "{ended}{{\"timestamp\":\"2026-09-10T16:00:11Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n"
            );
            assert_eq!(
                parse_rollout(
                    Path::new("rollout.jsonl"),
                    &restarted,
                    SystemTime::UNIX_EPOCH
                )
                .unwrap()
                .status(),
                "busy"
            );
        }
    }

    #[test]
    fn commentary_without_lifecycle_events_is_busy() {
        for envelope in ["response_item", "event_msg"] {
            let text = serde_json::json!({
                "timestamp": "2026-09-10T16:00:00Z",
                "type": envelope,
                "payload": {
                    "type": if envelope == "response_item" { "message" } else { "agent_message" },
                    "role": "assistant", "phase": "commentary", "message": "Checking it",
                    "content": [{"type": "output_text", "text": "Checking it"}]
                }
            })
            .to_string();
            assert_eq!(
                parse_rollout(Path::new("rollout.jsonl"), &text, SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .status(),
                "busy"
            );
        }
    }

    #[test]
    fn aborted_turn_discards_unanswered_tools() {
        let text = r#"{"timestamp":"2026-09-10T16:00:00Z","type":"event_msg","payload":{"type":"task_started"}}
{"timestamp":"2026-09-10T16:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"old","name":"exec_command","arguments":"{\"sandbox_permissions\":\"require_escalated\"}"}}
{"timestamp":"2026-09-10T16:00:02Z","type":"event_msg","payload":{"type":"turn_aborted"}}"#;
        let digest =
            parse_rollout(Path::new("rollout.jsonl"), text, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(digest.status(), "idle");
        assert!(!digest.pending_tool);
        assert!(!digest.pending_approval_tool);
    }

    #[test]
    fn selects_user_rollout_regardless_of_open_file_order() {
        let dir = std::env::temp_dir().join(format!("triage-rollouts-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.jsonl");
        let guardian = dir.join("guardian.jsonl");
        let child = dir.join("child.jsonl");
        fs::write(&user, r#"{"type":"session_meta","payload":{"id":"user","source":"cli","thread_source":"user"}}
{"timestamp":"2026-09-10T16:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":"Checking headlines"}}"#).unwrap();
        fs::write(&guardian, r#"{"type":"session_meta","payload":{"id":"guardian","thread_source":"guardian_review"}}
{"timestamp":"2026-09-10T16:00:02Z","type":"event_msg","payload":{"type":"agent_message","message":"approval JSON"}}"#).unwrap();
        fs::write(&child, r#"{"type":"session_meta","payload":{"id":"child","source":{"subagent":{"spawn":{}}}}}
{"timestamp":"2026-09-10T16:00:01Z","type":"event_msg","payload":{"type":"agent_message","message":"Child result"}}"#).unwrap();
        let mut cache = CodexDigestCache::new();
        for paths in [
            vec![guardian.clone(), child.clone(), user.clone()],
            vec![user.clone(), child.clone(), guardian.clone()],
        ] {
            let (path, digest) = select_rollout(paths, &mut cache).unwrap();
            assert_eq!(path, user);
            assert_eq!(digest.headline.as_deref(), Some("Checking headlines"));
        }
        assert!(select_rollout(vec![guardian], &mut cache).is_none());
        assert_eq!(
            select_rollout(vec![child], &mut cache)
                .unwrap()
                .1
                .session_id,
            "child"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_guardian_source_without_thread_source() {
        assert!(
            parse_rollout(
                Path::new("rollout.jsonl"),
                r#"{"type":"session_meta","payload":{"source":{"subagent":{"other":"guardian"}}}}"#,
                SystemTime::UNIX_EPOCH
            )
            .is_none()
        );
    }

    #[test]
    fn new_request_replaces_old_answer_until_assistant_responds() {
        let mut text = String::from(
            r#"{"timestamp":"2026-09-10T16:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":"Previous task done"}}
{"timestamp":"2026-09-10T16:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"Check the headlines"}}"#,
        );
        let parse = |text: &str| {
            parse_rollout(Path::new("rollout.jsonl"), text, SystemTime::UNIX_EPOCH).unwrap()
        };
        assert_eq!(
            parse(&text).headline.as_deref(),
            Some("→ Check the headlines")
        );
        text.push_str(r#"
{"timestamp":"2026-09-10T16:00:02Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"Investigating transcript selection"}]}}"#);
        assert_eq!(
            parse(&text).headline.as_deref(),
            Some("Investigating transcript selection")
        );
        text.push_str(r#"
{"timestamp":"2026-09-10T16:00:03Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"Fixed transcript selection"}]}}"#);
        assert_eq!(
            parse(&text).headline.as_deref(),
            Some("Fixed transcript selection")
        );
    }

    #[test]
    fn headline_omits_memory_metadata_but_detail_preserves_it() {
        let text = r#"{"timestamp":"2026-09-10T16:00:00Z","type":"event_msg","payload":{"type":"agent_message","message":"Ready for review.\n<oai-mem-citation>internal references</oai-mem-citation>"}}"#;
        let digest =
            parse_rollout(Path::new("rollout.jsonl"), text, SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(digest.headline.as_deref(), Some("Ready for review."));
        assert!(
            digest
                .latest_assistant_text
                .unwrap()
                .contains("<oai-mem-citation>")
        );
    }

    #[test]
    fn parses_codex_rollout_core_fields() {
        let text = r#"{"timestamp":"2026-05-30T16:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","timestamp":"2026-05-30T15:59:59.000Z","cwd":"/tmp/project","originator":"codex-tui","cli_version":"0.135.0"}}
{"timestamp":"2026-05-30T16:00:01.000Z","type":"turn_context","payload":{"cwd":"/tmp/project","model":"gpt-5.5"}}
{"timestamp":"2026-05-30T16:00:02.000Z","type":"event_msg","payload":{"type":"user_message","message":"please inspect it"}}
{"timestamp":"2026-05-30T16:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"git status\"}"}}
{"timestamp":"2026-05-30T16:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}
{"timestamp":"2026-05-30T16:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}
{"timestamp":"2026-05-30T16:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":300,"output_tokens":100,"reasoning_output_tokens":50},"last_token_usage":{"input_tokens":900},"model_context_window":258400}}}"#;
        let digest = parse_rollout(
            Path::new("/tmp/rollout-2026-05-30T09-00-00-thread-1.jsonl"),
            text,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(digest.session_id, "thread-1");
        assert_eq!(digest.cwd, Some(PathBuf::from("/tmp/project")));
        assert_eq!(digest.status(), "idle");
        assert_eq!(digest.last_prompt.as_deref(), Some("please inspect it"));
        assert_eq!(digest.headline.as_deref(), Some("done"));
        assert_eq!(digest.latest_model.as_deref(), Some("gpt-5.5"));
        assert_eq!(digest.total_tokens_in, 1000);
        assert_eq!(digest.total_tokens_out, 150);
        assert_eq!(digest.total_tokens_cache_read, 300);
        assert_eq!(digest.latest_context_tokens, 900);
        assert_eq!(digest.context_window, Some(258400));
    }

    #[test]
    fn pending_function_call_marks_session_busy() {
        let text = r#"{"timestamp":"2026-05-30T16:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","cwd":"/tmp/project"}}
{"timestamp":"2026-05-30T16:00:01.000Z","type":"event_msg","payload":{"type":"user_message","message":"run tests"}}
{"timestamp":"2026-05-30T16:00:02.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"cargo test\"}"}}"#;
        let digest = parse_rollout(
            Path::new("/tmp/rollout-2026-05-30T09-00-00-thread-1.jsonl"),
            text,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(digest.status(), "busy");
        assert_eq!(
            digest.last_tool_use,
            Some(("exec_command".to_string(), "cargo test".to_string()))
        );
        assert!(!digest.pending_approval_tool);
    }

    #[test]
    fn pending_escalated_function_call_marks_approval_prompt_pending() {
        let text = r#"{"timestamp":"2026-05-30T16:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","cwd":"/tmp/project"}}
{"timestamp":"2026-05-30T16:00:01.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"cargo install --path .\",\"sandbox_permissions\":\"require_escalated\",\"justification\":\"install the current build\"}"}}"#;
        let digest = parse_rollout(
            Path::new("/tmp/rollout-2026-05-30T09-00-00-thread-1.jsonl"),
            text,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(digest.status(), "busy");
        assert!(digest.pending_approval_tool);
        assert_eq!(
            digest.last_tool_use,
            Some((
                "exec_command".to_string(),
                "cargo install --path .".to_string()
            ))
        );
    }

    #[test]
    fn completed_escalated_function_call_is_not_pending_approval() {
        let text = r#"{"timestamp":"2026-05-30T16:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","cwd":"/tmp/project"}}
{"timestamp":"2026-05-30T16:00:01.000Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"call-1","arguments":"{\"cmd\":\"triage --probe\",\"sandbox_permissions\":\"require_escalated\"}"}}
{"timestamp":"2026-05-30T16:00:02.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#;
        let digest = parse_rollout(
            Path::new("/tmp/rollout-2026-05-30T09-00-00-thread-1.jsonl"),
            text,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert!(!digest.pending_approval_tool);
    }

    #[test]
    fn nested_parallel_tool_approval_is_detected() {
        let args = r#"{"tool_uses":[{"recipient_name":"functions.exec_command","parameters":{"cmd":"gh pr checks","sandbox_permissions":"require_escalated"}}]}"#;
        assert!(codex_arguments_require_approval(args));
    }

    #[test]
    fn parses_thread_roots_from_spawn_edges() {
        let json = br#"[
            {"parent_thread_id":"parent","child_thread_id":"child-a"},
            {"parent_thread_id":"child-a","child_thread_id":"grandchild"},
            {"parent_thread_id":"parent","child_thread_id":"child-b"}
        ]"#;
        let roots = parse_thread_roots_json(json);
        assert_eq!(roots.get("parent").map(String::as_str), Some("parent"));
        assert_eq!(roots.get("child-a").map(String::as_str), Some("parent"));
        assert_eq!(roots.get("child-b").map(String::as_str), Some("parent"));
        assert_eq!(roots.get("grandchild").map(String::as_str), Some("parent"));
    }

    #[test]
    fn parses_thread_title_json_and_normalizes_labels() {
        let json = br#"[{"id":"thread-1","title":"$mb-work triage\n\nextra","agent_nickname":null,"agent_role":null},{"id":"thread-2","title":"","agent_nickname":"reviewer","agent_role":"correctness"}]"#;
        let titles = parse_thread_titles_json(json);
        assert_eq!(
            titles
                .get("thread-1")
                .and_then(CodexThreadTitle::display_label),
            Some("$mb-work triage extra".to_string())
        );
        assert_eq!(
            titles
                .get("thread-2")
                .and_then(CodexThreadTitle::display_label),
            Some("reviewer (correctness)".to_string())
        );
    }

    #[test]
    fn session_meta_agent_label_is_parsed_as_fallback() {
        let text = r#"{"timestamp":"2026-05-30T16:00:00.000Z","type":"session_meta","payload":{"id":"thread-1","cwd":"/tmp/project","agent_nickname":"reviewer","agent_role":"workflow"}}
{"timestamp":"2026-05-30T16:00:01.000Z","type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#;
        let digest = parse_rollout(
            Path::new("/tmp/rollout-2026-05-30T09-00-00-thread-1.jsonl"),
            text,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            codex_agent_label(
                digest.agent_nickname.as_deref(),
                digest.agent_role.as_deref()
            ),
            Some("reviewer (workflow)".to_string())
        );
    }
}
