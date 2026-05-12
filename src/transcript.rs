use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// Window in which a prompt newer than the latest away_summary is considered
/// "current work." away_summary fires ~184s after a turn ends; we add buffer
/// so brief network/hook hiccups don't strand the prompt as the headline forever.
const PROMPT_FRESH_WINDOW: Duration = Duration::from_secs(5 * 60);

use crate::discovery::{encode_cwd, projects_dir};
use crate::models::Session;

#[allow(dead_code)]
#[derive(Clone)]
pub struct TranscriptDigest {
    pub path: PathBuf,
    pub headline: Option<String>,
    pub headline_at: Option<SystemTime>,
    pub last_prompt: Option<String>,
    pub last_prompt_at: Option<SystemTime>,
    pub last_turn_duration_ms: Option<u64>,
    pub last_turn_msg_count: Option<u64>,
    pub last_event_at: Option<SystemTime>,
    pub last_stop_at: Option<SystemTime>,
    pub user_prompt_count: u64,
    pub last_stop_had_errors: bool,
    pub last_tool_use: Option<(String, String)>,
    /// Approximate cumulative session cost in USD, computed from per-message
    /// `usage` × the rate table for the message's `model`. "Approximate"
    /// because (a) rates can change without us updating, (b) we sum all
    /// `cache_creation_input_tokens` at 5m rates even though some may be 1h
    /// (~2× the cost). Cross-check against Claude Code's `/cost` slash
    /// command for the canonical figure.
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub total_tokens_cache_write: u64,
    pub total_tokens_cache_read: u64,
    /// Tokens-in for the most recent assistant API call:
    /// `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`.
    /// Approximates the current context-window occupancy. 0 if no usage seen.
    pub latest_context_tokens: u64,
    /// Max `latest_context_tokens` observed across the session. A peak >200k
    /// is hard evidence the user is on a 1M-context model variant, even when
    /// the message's `model` field doesn't carry the `[1m]` tag.
    pub peak_context_tokens: u64,
    /// Model name from the most recent assistant message. Used to look up the
    /// context-window size for the percentage display.
    pub latest_model: Option<String>,
    /// Most recent assistant text response (joined across content blocks).
    /// For Blocked sessions, this is typically Claude's explanation of *why*
    /// it wants to run the pending tool — usually more useful than the raw
    /// tool input alone.
    pub latest_assistant_text: Option<String>,
}

/// Per-million-token USD rates for each model family. Caller picks via
/// `rates_for_model`; substring match on "opus" / "sonnet" / "haiku" so
/// minor-version bumps don't need a code change. Rates as of 2026-Q2.
struct Rates {
    input: f64,
    output: f64,
    cache_write: f64, // ephemeral 5m
    cache_read: f64,
}

const OPUS_RATES: Rates = Rates {
    input: 15.0,
    output: 75.0,
    cache_write: 18.75,
    cache_read: 1.50,
};
const SONNET_RATES: Rates = Rates {
    input: 3.0,
    output: 15.0,
    cache_write: 3.75,
    cache_read: 0.30,
};
const HAIKU_RATES: Rates = Rates {
    input: 1.0,
    output: 5.0,
    cache_write: 1.25,
    cache_read: 0.10,
};

fn rates_for_model(model: &str) -> &'static Rates {
    let m = model.to_lowercase();
    if m.contains("opus") {
        &OPUS_RATES
    } else if m.contains("haiku") {
        &HAIKU_RATES
    } else {
        // Default to Sonnet rates: most common, and lower than Opus, so
        // unknown models err on the cheap side rather than over-reporting.
        &SONNET_RATES
    }
}

/// (path → (mtime, digest)). Skip re-parsing JSONL when its mtime hasn't
/// advanced since the last read. Most jsonls don't change between refresh
/// ticks; the active session's file does, but only it pays the parse cost.
#[derive(Default)]
pub struct DigestCache {
    entries: HashMap<PathBuf, (SystemTime, TranscriptDigest)>,
}

impl DigestCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&mut self, path: &Path) -> Option<TranscriptDigest> {
        let mtime = fs::metadata(path).and_then(|m| m.modified()).ok()?;
        if let Some((cached_mtime, d)) = self.entries.get(path)
            && *cached_mtime == mtime
        {
            return Some(d.clone());
        }
        let d = digest(path)?;
        self.entries.insert(path.to_path_buf(), (mtime, d.clone()));
        Some(d)
    }

    /// Drop entries for files that no longer exist. Called once per refresh
    /// so the cache doesn't grow unboundedly across renamed/deleted jsonls.
    pub fn evict_missing(&mut self) {
        self.entries.retain(|p, _| p.exists());
    }
}

/// Pair every live session in the same cwd to a transcript .jsonl. We can't
/// trust the sessionId recorded in `~/.claude/sessions/<pid>.json`: after
/// `/clear`, Claude writes the new conversation to a freshly-named .jsonl but
/// often doesn't rewrite the sessions JSON, leaving it pointing at a stale
/// file. We also can't trust file mtime alone, because away_summary writes can
/// touch an idle session's jsonl after the user has moved focus elsewhere.
///
/// The most reliable signal for "which jsonl is the user actively typing in"
/// is the latest user-text event timestamp inside each .jsonl. Pair the
/// currently-focused tmux pane's pid with the jsonl whose latest user-text is
/// newest; pair the rest greedily by mtime against updatedAt.
pub fn assign_transcripts(sessions: &mut [Session], cache: &mut DigestCache) {
    let mut by_cwd: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        by_cwd.entry(s.cwd.clone()).or_default().push(i);
    }

    for (cwd, mut idxs) in by_cwd {
        let dir = projects_dir().join(encode_cwd(&cwd));
        let Ok(read) = fs::read_dir(&dir) else { continue };

        // (mtime, last_user_text_at, path). last_user_text_at falls back to
        // mtime when the file has no qualifying user-text event, so files with
        // only system/assistant noise still sort somewhere reasonable.
        let mut jsonls: Vec<(SystemTime, SystemTime, PathBuf)> = Vec::new();
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 {
                continue;
            }
            let Ok(mtime) = meta.modified() else { continue };
            // Use the cached digest's last_prompt_at as the user-text
            // timestamp. Falls back to mtime when no qualifying user-text
            // exists. The cache makes this free on unchanged files.
            let user_ts = cache
                .get(&path)
                .and_then(|d| d.last_prompt_at)
                .unwrap_or(mtime);
            jsonls.push((mtime, user_ts, path));
        }

        // PASS 1 — sessionId direct match (TRI-83). The canonical pointer
        // from session → transcript lives in `Session.session_id`. When two
        // sessions share a cwd, this is the only reliable signal; the
        // greedy step below uses sessions-JSON `updatedAt` which lags 30+
        // min on idle Claude processes and can mis-pair freshly-typed
        // sessions to the wrong row. We do this before the active-pane
        // step so that an active pane whose sessionId still points at its
        // own (slightly older) transcript doesn't accidentally steal a
        // peer's freshest jsonl.
        let mut matched: Vec<usize> = Vec::new();
        for (pos, &si) in idxs.iter().enumerate() {
            let sid = &sessions[si].session_id;
            if sid.is_empty() {
                continue;
            }
            let target_name = format!("{sid}.jsonl");
            if let Some(j) = jsonls
                .iter()
                .position(|(_, _, p)| p.file_name().map(|f| f == target_name.as_str()).unwrap_or(false))
            {
                let (_, _, path) = jsonls.swap_remove(j);
                sessions[si].transcript_path = Some(path);
                matched.push(pos);
            }
        }
        // Drop matched indices from `idxs` (descending so swap_remove is
        // safe).
        matched.sort_unstable();
        for pos in matched.into_iter().rev() {
            idxs.swap_remove(pos);
        }

        // PASS 2 — active pane gets the jsonl with newest last-user-text.
        // Only runs for sessions still unmatched after pass 1.
        let active_idx = idxs
            .iter()
            .position(|&i| sessions[i].pane.as_ref().is_some_and(|p| p.active));

        if let (Some(pos), false) = (active_idx, jsonls.is_empty()) {
            let pick = jsonls
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, uts, _))| *uts)
                .map(|(i, _)| i);
            if let Some(j) = pick {
                let (_, _, path) = jsonls.swap_remove(j);
                let session_idx = idxs.swap_remove(pos);
                sessions[session_idx].transcript_path = Some(path);
            }
        }

        // PASS 3 — greedy fallback for any remaining sessions whose
        // sessionId was stale (post-`/clear` is the documented case).
        // Pair by updatedAt DESC against mtime DESC.
        jsonls.sort_by(|a, b| b.0.cmp(&a.0));
        idxs.sort_by_key(|&i| std::cmp::Reverse(sessions[i].updated_at_ms));
        for (k, &si) in idxs.iter().enumerate() {
            if k >= jsonls.len() {
                break;
            }
            sessions[si].transcript_path = Some(jsonls[k].2.clone());
        }
    }
}

/// For a given session, locate its transcript JSONL.
/// Prefer the file matching `sessionId` (the canonical pointer when fresh);
/// fall back to newest-mtime if the sessionId file is missing or empty
/// (handles the post-`/clear` lag case documented in PLAN.md).
/// Used as a single-session fallback when assign_transcripts hasn't run.
pub fn locate_transcript(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let dir = projects_dir().join(encode_cwd(cwd));

    // Try sessionId first.
    let by_id = dir.join(format!("{session_id}.jsonl"));
    if let Ok(meta) = fs::metadata(&by_id)
        && meta.len() > 0 {
            return Some(by_id);
        }

    // Fallback: newest .jsonl in the dir.
    let entries = fs::read_dir(&dir).ok()?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Read the full JSONL and pull out the fields we care about.
/// JSONL files are append-only and bounded in practice (largest sampled was 5989 events,
/// ~few MB). For v1, full-read is fine; tail-read can come later if it matters.
pub fn digest(path: &Path) -> Option<TranscriptDigest> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);

    let mut headline: Option<(SystemTime, String)> = None;
    let mut last_prompt: Option<String> = None;
    let mut last_prompt_at: Option<SystemTime> = None;
    let mut last_turn_duration_ms: Option<u64> = None;
    let mut last_turn_msg_count: Option<u64> = None;
    let mut last_event_at: Option<SystemTime> = None;
    let mut last_stop_at: Option<SystemTime> = None;
    let mut user_prompt_count: u64 = 0;
    let mut last_stop_had_errors = false;
    // Every tool_use in transcript order, plus the set of ids that have a
    // matching tool_result (i.e. completed). The pending tool_use — the one
    // Claude is currently asking permission for — is the latest entry whose
    // id is NOT in `completed`. Last-tool-use-wins is wrong because Claude
    // can auto-run later tool calls (e.g. Edit when defaultMode=acceptEdits)
    // while still blocked on an earlier Bash.
    let mut tool_uses: Vec<(String, String, String)> = Vec::new();
    let mut completed_tool_ids: HashSet<String> = HashSet::new();
    // Cost accumulators. We multiply per assistant message because the
    // model can change mid-session (e.g. Opus → Sonnet on `/model`).
    //
    // Dedupe by `message.id`: one Anthropic API call emits multiple JSONL
    // events (one per content block — text, tool_use, etc.), all sharing the
    // same `message.id` and the same `usage` payload. Counting usage on each
    // event triple- or quadruple-counts every turn's cost. We sum once per
    // unique msg_id.
    let mut total_cost_usd: f64 = 0.0;
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;
    let mut total_tokens_cache_write: u64 = 0;
    let mut total_tokens_cache_read: u64 = 0;
    let mut counted_msg_ids: HashSet<String> = HashSet::new();
    let mut latest_context_tokens: u64 = 0;
    let mut peak_context_tokens: u64 = 0;
    let mut latest_model: Option<String> = None;
    // Track the latest assistant text alongside its event timestamp so a
    // late-arriving older event (rare but possible) doesn't overwrite a
    // newer one. None until we see an assistant message with text content.
    let mut latest_assistant_text: Option<(SystemTime, String)> = None;

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_ts);
        if let Some(t) = ts
            && last_event_at.is_none_or(|prev| t > prev) {
                last_event_at = Some(t);
            }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ty {
            "last-prompt" => {
                // last-prompt events have no timestamp, but they carry the canonical
                // prompt text. Use as a fallback source for last_prompt; timestamp
                // comes from the user-text events below.
                if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str()) {
                    last_prompt = Some(p.to_string());
                }
            }
            "user" => {
                // The transcript records both real user-typed messages and tool_result
                // echoes under type=user. Discriminate by content shape: text blocks
                // are prompts, tool_result blocks complete a prior tool_use.
                if let Some(content) = v.get("message").and_then(|m| m.get("content")) {
                    if let Some(text) = extract_user_text(content)
                        && let Some(t) = ts
                    {
                        last_prompt = Some(text);
                        last_prompt_at = Some(t);
                        user_prompt_count += 1;
                    }
                    if let Some(arr) = content.as_array() {
                        for block in arr {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                                && let Some(id) =
                                    block.get("tool_use_id").and_then(|i| i.as_str())
                            {
                                completed_tool_ids.insert(id.to_string());
                            }
                        }
                    }
                }
            }
            "assistant" => {
                if let Some(content) = v.get("message").and_then(|m| m.get("content"))
                    && let Some(arr) = content.as_array()
                {
                    let mut text_parts: Vec<String> = Vec::new();
                    for block in arr {
                        let block_type = block.get("type").and_then(|t| t.as_str());
                        if block_type == Some("tool_use") {
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("?")
                                .to_string();
                            let brief =
                                crate::approval::brief_tool_input(block.get("input"));
                            tool_uses.push((id, name, brief));
                        } else if block_type == Some("text")
                            && let Some(t) = block.get("text").and_then(|t| t.as_str())
                            && !t.trim().is_empty()
                        {
                            text_parts.push(t.to_string());
                        }
                    }
                    if !text_parts.is_empty()
                        && let Some(t) = ts
                        && latest_assistant_text
                            .as_ref()
                            .is_none_or(|(prev, _)| t >= *prev)
                    {
                        latest_assistant_text = Some((t, text_parts.join("\n\n")));
                    }
                }
                // Per-message cost. Each assistant API call has its own
                // `usage` object and `model` field; sum across the whole
                // session using the per-model rate table. Dedupe by
                // `message.id` (see counted_msg_ids comment above) — one
                // API call typically appears as several JSONL events all
                // sharing the same id, and each carries the same usage.
                // Cache_creation is billed at the 5m rate; we ignore the
                // rare 1h split.
                if let Some(msg) = v.get("message")
                    && let Some(usage) = msg.get("usage")
                {
                    let msg_id = msg
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    // Empty id (older transcript schemas) → can't dedupe;
                    // count it. Real msg_ids → only count first occurrence.
                    let should_count =
                        msg_id.is_empty() || counted_msg_ids.insert(msg_id);
                    if should_count {
                        let model =
                            msg.get("model").and_then(|m| m.as_str()).unwrap_or("");
                        let r = rates_for_model(model);
                        let in_tok = usage
                            .get("input_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                        let out_tok = usage
                            .get("output_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                        let cw_tok = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                        let cr_tok = usage
                            .get("cache_read_input_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0);
                        let cost = (in_tok as f64 * r.input
                            + out_tok as f64 * r.output
                            + cw_tok as f64 * r.cache_write
                            + cr_tok as f64 * r.cache_read)
                            / 1_000_000.0;
                        total_cost_usd += cost;
                        total_tokens_in += in_tok;
                        total_tokens_out += out_tok;
                        total_tokens_cache_write += cw_tok;
                        total_tokens_cache_read += cr_tok;
                        // Track the latest call's input total for the
                        // context-window indicator. Always update with the
                        // most recent observation; transcripts are append-
                        // only, so the last counted message wins.
                        let ctx = in_tok + cw_tok + cr_tok;
                        latest_context_tokens = ctx;
                        if ctx > peak_context_tokens {
                            peak_context_tokens = ctx;
                        }
                        if !model.is_empty() {
                            latest_model = Some(model.to_string());
                        }
                    }
                }
            }
            "system" => {
                let sub = v.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
                match sub {
                    "away_summary" => {
                        if let (Some(c), Some(t)) =
                            (v.get("content").and_then(|c| c.as_str()), ts)
                            && headline.as_ref().is_none_or(|(prev, _)| t > *prev) {
                                headline = Some((t, c.to_string()));
                            }
                    }
                    "turn_duration" => {
                        if let Some(ms) = v.get("durationMs").and_then(|d| d.as_u64()) {
                            last_turn_duration_ms = Some(ms);
                        }
                        if let Some(mc) = v.get("messageCount").and_then(|m| m.as_u64()) {
                            last_turn_msg_count = Some(mc);
                        }
                    }
                    "stop_hook_summary" => {
                        if let Some(t) = ts {
                            last_stop_at = Some(t);
                        }
                        let errs = v
                            .get("hookErrors")
                            .and_then(|e| e.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        last_stop_had_errors = errs;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let (headline_at, headline_text) = match headline {
        Some((t, s)) => (Some(t), Some(s)),
        None => (None, None),
    };

    // The pending tool_use is the latest emitted whose id isn't in the
    // completed set. Empty id (older transcript schemas) → treat as not
    // completable, so we still surface the latest one.
    let last_tool_use = tool_uses
        .into_iter()
        .rev()
        .find(|(id, _, _)| id.is_empty() || !completed_tool_ids.contains(id))
        .map(|(_, name, brief)| (name, brief));

    Some(TranscriptDigest {
        path: path.to_path_buf(),
        headline: headline_text,
        headline_at,
        last_prompt,
        last_prompt_at,
        last_turn_duration_ms,
        last_turn_msg_count,
        last_event_at,
        last_stop_at,
        user_prompt_count,
        last_stop_had_errors,
        last_tool_use,
        total_cost_usd,
        total_tokens_in,
        total_tokens_out,
        total_tokens_cache_write,
        total_tokens_cache_read,
        latest_context_tokens,
        peak_context_tokens,
        latest_model,
        latest_assistant_text: latest_assistant_text.map(|(_, t)| t),
    })
}

pub fn enrich(session: &mut Session, now: SystemTime, cache: &mut DigestCache) {
    let path = match &session.transcript_path {
        Some(p) => p.clone(),
        None => match locate_transcript(&session.cwd, &session.session_id) {
            Some(p) => p,
            None => return,
        },
    };
    session.transcript_path = Some(path.clone());
    let Some(d) = cache.get(&path) else { return };
    // Prompt supersedes the recap when the user started new work AND the session
    // is genuinely still active. We gate on last_event_at (latest transcript
    // event) rather than session.status, because the sessions JSON status lags
    // and can stay "busy" long after activity has stopped. If the transcript
    // hasn't seen any event in PROMPT_FRESH_WINDOW, the away_summary should have
    // caught up; if it didn't, the prompt is stale and the recap is preferable.
    let session_is_active = d
        .last_event_at
        .and_then(|t| now.duration_since(t).ok())
        .map(|age| age <= PROMPT_FRESH_WINDOW)
        .unwrap_or(false);
    let prompt_supersedes = match (d.headline_at, d.last_prompt_at) {
        (Some(h), Some(p)) => p > h && session_is_active,
        (None, Some(_)) => session_is_active,
        _ => false,
    };
    session.headline = if prompt_supersedes {
        d.last_prompt.clone().map(|p| format!("→ {p}"))
    } else {
        d.headline.clone()
    }
    .or_else(|| d.headline.clone())
    .or_else(|| d.last_prompt.clone());
    session.last_prompt = d.last_prompt;
    session.last_prompt_at = d.last_prompt_at;
    session.last_turn_duration_ms = d.last_turn_duration_ms;
    session.last_turn_msg_count = d.last_turn_msg_count;
    session.last_event_at = d.last_event_at;
    session.last_stop_at = d.last_stop_at;
    session.user_prompt_count = d.user_prompt_count;
    session.last_stop_had_errors = d.last_stop_had_errors;
    session.last_tool_use = d.last_tool_use;
    session.total_cost_usd = d.total_cost_usd;
    session.total_tokens_in = d.total_tokens_in;
    session.total_tokens_out = d.total_tokens_out;
    session.total_tokens_cache_write = d.total_tokens_cache_write;
    session.total_tokens_cache_read = d.total_tokens_cache_read;
    session.latest_context_tokens = d.latest_context_tokens;
    session.peak_context_tokens = d.peak_context_tokens;
    session.latest_model = d.latest_model;
    session.latest_assistant_text = d.latest_assistant_text;
}

/// Extract user-typed text from a message.content value.
/// Content is either a string (rare) or an array of blocks; we only want
/// real prompt text. Returns None if the content is purely tool results,
/// auto-attached image-source markers, or interrupt sentinels.
fn extract_user_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return clean_prompt_text(s);
    }
    let arr = content.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(t) = block.get("text").and_then(|t| t.as_str())
            && let Some(cleaned) = clean_prompt_text(t)
        {
            parts.push(cleaned);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" "))
}

/// Drop auto-generated noise that arrives in user events: image-attachment
/// metadata, slash-command sentinels, and interrupt markers. Returns None if
/// the trimmed text is empty or pure noise.
fn clean_prompt_text(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("[Image: source:") {
        return None;
    }
    if trimmed == "[Request interrupted by user]" {
        return None;
    }
    // Slash-command invocations are emitted into the transcript as XML-tagged
    // synthetic messages (`<command-name>/clear</command-name>`,
    // `<local-command-stdout>...`, `<local-command-caveat>...` etc.). Skip them
    // so they don't masquerade as prompts.
    if trimmed.starts_with("<command-") || trimmed.starts_with("<local-command-") {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parse RFC3339 timestamp "2026-05-04T22:34:00.000Z" → SystemTime.
fn parse_ts(s: &str) -> Option<SystemTime> {
    // Hand-rolled to avoid pulling in chrono. Strict format: YYYY-MM-DDTHH:MM:SS[.fff]Z
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[10] != b'T' || *bytes.last().unwrap() != b'Z' {
        return None;
    }
    let year: i64 = std::str::from_utf8(&bytes[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&bytes[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&bytes[8..10]).ok()?.parse().ok()?;
    let hour: u32 = std::str::from_utf8(&bytes[11..13]).ok()?.parse().ok()?;
    let min: u32 = std::str::from_utf8(&bytes[14..16]).ok()?.parse().ok()?;
    let sec: u32 = std::str::from_utf8(&bytes[17..19]).ok()?.parse().ok()?;

    let secs = days_from_civil(year, month, day) * 86400
        + hour as i64 * 3600
        + min as i64 * 60
        + sec as i64;
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// Howard Hinnant's days_from_civil — number of days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy =
        (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}
