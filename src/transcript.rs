use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::discovery::{encode_cwd, projects_dir};
use crate::models::Session;

#[allow(dead_code)]
pub struct TranscriptDigest {
    pub path: PathBuf,
    pub headline: Option<String>,
    pub last_prompt: Option<String>,
    pub last_turn_duration_ms: Option<u64>,
    pub last_turn_msg_count: Option<u64>,
    pub last_event_at: Option<SystemTime>,
    pub last_stop_at: Option<SystemTime>,
    pub user_prompt_count: u64,
    pub last_stop_had_errors: bool,
}

/// For a given session, locate its transcript JSONL.
/// Prefer the file matching `sessionId` (the canonical pointer when fresh);
/// fall back to newest-mtime if the sessionId file is missing or empty
/// (handles the post-`/clear` lag case documented in PLAN.md).
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
    let mut last_turn_duration_ms: Option<u64> = None;
    let mut last_turn_msg_count: Option<u64> = None;
    let mut last_event_at: Option<SystemTime> = None;
    let mut last_stop_at: Option<SystemTime> = None;
    let mut user_prompt_count: u64 = 0;
    let mut last_stop_had_errors = false;

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
                if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str()) {
                    last_prompt = Some(p.to_string());
                    user_prompt_count += 1;
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

    Some(TranscriptDigest {
        path: path.to_path_buf(),
        headline: headline.map(|(_, s)| s),
        last_prompt,
        last_turn_duration_ms,
        last_turn_msg_count,
        last_event_at,
        last_stop_at,
        user_prompt_count,
        last_stop_had_errors,
    })
}

pub fn enrich(session: &mut Session) {
    let Some(path) = locate_transcript(&session.cwd, &session.session_id) else {
        return;
    };
    session.transcript_path = Some(path.clone());
    let Some(d) = digest(&path) else { return };
    session.headline = d.headline.or_else(|| d.last_prompt.clone());
    session.last_prompt = d.last_prompt;
    session.last_turn_duration_ms = d.last_turn_duration_ms;
    session.last_turn_msg_count = d.last_turn_msg_count;
    session.last_event_at = d.last_event_at;
    session.last_stop_at = d.last_stop_at;
    session.user_prompt_count = d.user_prompt_count;
    session.last_stop_had_errors = d.last_stop_had_errors;
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
