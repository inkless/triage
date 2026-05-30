//! Cross-session cost rollup. Walks `~/.claude/projects/<encoded>/*.jsonl`,
//! sums per-message contributions through the shared `transcript::score_message`
//! scorer, and buckets by local-day / cwd / session / model.
//!
//! Two surfaces consume this: the in-TUI `$` overlay (see `ui::draw_cost_overlay`)
//! and the `triage cost` CLI subcommand below. Both call `compute_rollup` —
//! one-shot, no persistence. The corpus only grows; if scan time becomes a
//! problem, add an mtime-keyed cache, but at ~hundreds of sessions on APFS this
//! is well under a second cold and we re-do it ~per-open, not per-tick.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::discovery::projects_dir;
use crate::transcript::{MessageUsage, parse_timestamp, score_message};

/// One day's worth of spend in local time.
#[derive(Clone, Debug)]
pub struct DayBucket {
    pub key: DayKey,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DayKey {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl DayKey {
    pub fn format(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[derive(Clone, Debug)]
pub struct CwdBucket {
    pub cwd: String,
    pub cost_usd: f64,
    pub session_count: u64,
}

#[derive(Clone, Debug)]
pub struct ModelBucket {
    pub model: String,
    pub cost_usd: f64,
}

#[derive(Clone, Debug)]
pub struct SessionBucket {
    #[allow(dead_code)] // exposed for future per-session drill-down; unused today
    pub path: PathBuf,
    pub cwd: String,
    pub session_id: String,
    pub cost_usd: f64,
    #[allow(dead_code)]
    pub last_event: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct Rollup {
    pub days: Vec<DayBucket>,
    pub cwds: Vec<CwdBucket>,
    pub models: Vec<ModelBucket>,
    pub sessions: Vec<SessionBucket>,
    pub total_today: f64,
    pub total_7d: f64,
    pub total_30d: f64,
    pub total_all: f64,
    pub scanned_files: u64,
    pub scan_duration_ms: u128,
    /// `now` as a local day, captured at scan time. Used by renderers to label
    /// "today" without re-querying the clock and getting a different boundary.
    pub today: DayKey,
}

/// Walk every transcript JSONL under `~/.claude/projects/` and aggregate.
pub fn compute_rollup() -> Rollup {
    let started = Instant::now();
    let now = SystemTime::now();
    let today = local_day(now);
    let today_secs = day_start_secs(today);
    let secs_per_day: i64 = 86_400;
    let cutoff_7d = today_secs - 6 * secs_per_day; // "today + previous 6"
    let cutoff_30d = today_secs - 29 * secs_per_day;

    let root = projects_dir();
    let mut days: HashMap<DayKey, DayBucket> = HashMap::new();
    let mut cwds: HashMap<String, CwdBucket> = HashMap::new();
    let mut models: HashMap<String, ModelBucket> = HashMap::new();
    let mut sessions: Vec<SessionBucket> = Vec::new();

    let mut scanned_files: u64 = 0;
    let mut total_today = 0.0;
    let mut total_7d = 0.0;
    let mut total_30d = 0.0;
    let mut total_all = 0.0;

    // Walk one level deep: each subdir is one encoded cwd, each .jsonl is one
    // session. Tolerate read errors silently — a half-rotated dir shouldn't
    // sink the whole rollup.
    let Ok(subdirs) = fs::read_dir(&root) else {
        return Rollup {
            today,
            scan_duration_ms: started.elapsed().as_millis(),
            ..Default::default()
        };
    };

    for sub in subdirs.flatten() {
        let dir = sub.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in files.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            scanned_files += 1;
            let Some(session) = score_session(&path) else {
                continue;
            };
            total_all += session.cost_usd;

            for (day_key, cost, in_tok, out_tok) in session.per_day {
                let day_secs = day_start_secs(day_key);
                if day_key == today {
                    total_today += cost;
                }
                if day_secs >= cutoff_7d {
                    total_7d += cost;
                }
                if day_secs >= cutoff_30d {
                    total_30d += cost;
                }
                let bucket = days.entry(day_key).or_insert(DayBucket {
                    key: day_key,
                    cost_usd: 0.0,
                    tokens_in: 0,
                    tokens_out: 0,
                });
                bucket.cost_usd += cost;
                bucket.tokens_in += in_tok;
                bucket.tokens_out += out_tok;
            }

            for (model, cost) in session.per_model {
                let bucket = models.entry(model.clone()).or_insert(ModelBucket {
                    model,
                    cost_usd: 0.0,
                });
                bucket.cost_usd += cost;
            }

            let cwd_display = session.cwd.clone();
            let cwd_bucket = cwds.entry(cwd_display.clone()).or_insert(CwdBucket {
                cwd: cwd_display,
                cost_usd: 0.0,
                session_count: 0,
            });
            cwd_bucket.cost_usd += session.cost_usd;
            cwd_bucket.session_count += 1;

            sessions.push(SessionBucket {
                path: path.clone(),
                cwd: session.cwd,
                session_id: path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string(),
                cost_usd: session.cost_usd,
                last_event: session.last_event,
            });
        }
    }

    let mut days_vec: Vec<DayBucket> = days.into_values().collect();
    days_vec.sort_by_key(|d| d.key);

    let mut cwds_vec: Vec<CwdBucket> = cwds.into_values().collect();
    cwds_vec.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut models_vec: Vec<ModelBucket> = models.into_values().collect();
    models_vec.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    sessions.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Rollup {
        days: days_vec,
        cwds: cwds_vec,
        models: models_vec,
        sessions,
        total_today,
        total_7d,
        total_30d,
        total_all,
        scanned_files,
        scan_duration_ms: started.elapsed().as_millis(),
        today,
    }
}

struct SessionScore {
    cwd: String,
    cost_usd: f64,
    last_event: Option<SystemTime>,
    per_day: Vec<(DayKey, f64, u64, u64)>,
    per_model: Vec<(String, f64)>,
}

/// Parse one transcript file and return per-day / per-model contributions.
/// Returns `None` if the file has zero cost-bearing events — those don't need
/// to occupy a session-table row.
fn score_session(path: &Path) -> Option<SessionScore> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);

    let mut counted: HashSet<String> = HashSet::new();
    let mut cwd: Option<String> = None;
    let mut total: f64 = 0.0;
    let mut last_event: Option<SystemTime> = None;
    let mut per_day_map: HashMap<DayKey, (f64, u64, u64)> = HashMap::new();
    let mut per_model_map: HashMap<String, f64> = HashMap::new();

    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str())
        {
            cwd = Some(c.to_string());
        }
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp);
        if let Some(t) = ts
            && last_event.is_none_or(|prev| t > prev)
        {
            last_event = Some(t);
        }
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let Some(u) = score_message(msg, &mut counted) else {
            continue;
        };
        let MessageUsage {
            cost_usd,
            model,
            in_tok,
            out_tok,
            ..
        } = u;
        if cost_usd <= 0.0 {
            continue;
        }
        total += cost_usd;
        // Bucket by local-day of the event timestamp. Missing timestamp →
        // bucket under "today" so the cost still surfaces somewhere.
        let day_key = ts
            .map(local_day)
            .unwrap_or_else(|| local_day(SystemTime::now()));
        let entry = per_day_map.entry(day_key).or_insert((0.0, 0, 0));
        entry.0 += cost_usd;
        entry.1 += in_tok;
        entry.2 += out_tok;
        if !model.is_empty() {
            *per_model_map.entry(model_family(&model)).or_insert(0.0) += cost_usd;
        }
    }

    if total <= 0.0 {
        return None;
    }

    let cwd = cwd.unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let per_day: Vec<(DayKey, f64, u64, u64)> = per_day_map
        .into_iter()
        .map(|(k, (c, i, o))| (k, c, i, o))
        .collect();
    let per_model: Vec<(String, f64)> = per_model_map.into_iter().collect();

    Some(SessionScore {
        cwd,
        cost_usd: total,
        last_event,
        per_day,
        per_model,
    })
}

/// Collapse exact model strings (e.g. `claude-sonnet-4-6-20251001`) to the
/// family name used in the rates table. Keeps the per-model summary readable
/// — one row each for Opus / Sonnet / Haiku rather than every dated variant.
fn model_family(model: &str) -> String {
    let m = model.to_lowercase();
    if m.contains("opus") {
        "opus".to_string()
    } else if m.contains("haiku") {
        "haiku".to_string()
    } else if m.contains("sonnet") {
        "sonnet".to_string()
    } else {
        model.to_string()
    }
}

/// Compute the local-time (y, m, d) for a SystemTime.  Uses libc::localtime_r
/// so DST + per-system TZ rules apply per-timestamp; this matters across
/// spring/fall boundaries but is also just the conventional "today" the user
/// reads on their wall clock.
pub fn local_day(t: SystemTime) -> DayKey {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Safety: libc::localtime_r writes into the out parameter; we initialize
    // the struct to zero and pass a valid pointer. Reentrant variant, so no
    // global static-buffer race.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let secs_t: libc::time_t = secs as libc::time_t;
        libc::localtime_r(&secs_t, &mut tm);
        DayKey {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
        }
    }
}

/// Seconds-since-epoch of local midnight on the given day. Used for window
/// cutoffs (7d / 30d) so we compare day_keys via their UTC offset of local
/// midnight. Approximation: we treat all days as having a fixed offset of
/// 86400 from the day before; this is wrong across DST transitions by exactly
/// one hour, but the 7d/30d windows are coarse enough that one hour doesn't
/// move a session between buckets.
fn day_start_secs(d: DayKey) -> i64 {
    // Re-use the civil-day arithmetic from transcript.rs by constructing a
    // UTC timestamp at the local-day's midnight and pretending it's local.
    // Within the same TZ this is internally consistent for window-cutoff
    // comparisons.
    days_from_civil(d.year as i64, d.month, d.day) * 86_400
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

// ---------- CLI ----------

#[derive(Clone, Copy, Debug)]
enum GroupBy {
    Day,
    Cwd,
    Session,
    Model,
}

/// `triage cost` subcommand. Flags:
///   --days N            window for --by day strip (default 14)
///   --by day|cwd|session|model
///   --json              machine-readable
///   (no flags)          two-line glance: today + 7d
pub fn cli_cost(args: &[String]) -> io::Result<()> {
    let mut days: usize = 14;
    let mut group: Option<GroupBy> = None;
    let mut json = false;
    let mut top_n: usize = 10;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                days = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::other("--days needs a positive integer"))?;
                i += 2;
            }
            "--by" => {
                let key = args
                    .get(i + 1)
                    .ok_or_else(|| io::Error::other("--by needs day|cwd|session|model"))?;
                group = Some(match key.as_str() {
                    "day" => GroupBy::Day,
                    "cwd" => GroupBy::Cwd,
                    "session" => GroupBy::Session,
                    "model" => GroupBy::Model,
                    other => {
                        return Err(io::Error::other(format!(
                            "--by must be day|cwd|session|model (got {other})"
                        )));
                    }
                });
                i += 2;
            }
            "--top" => {
                top_n = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::other("--top needs a positive integer"))?;
                i += 2;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(io::Error::other(format!(
                    "unknown arg: {other} (try `triage cost --help`)"
                )));
            }
        }
    }

    let rollup = compute_rollup();
    let mut out = io::stdout().lock();

    if json {
        return print_json(&rollup, &mut out);
    }

    match group {
        None => print_glance(&rollup, &mut out),
        Some(GroupBy::Day) => print_by_day(&rollup, days, &mut out),
        Some(GroupBy::Cwd) => print_by_cwd(&rollup, top_n, &mut out),
        Some(GroupBy::Session) => print_by_session(&rollup, top_n, &mut out),
        Some(GroupBy::Model) => print_by_model(&rollup, &mut out),
    }
}

fn print_help() {
    println!("triage cost — daily/weekly Claude spend across all sessions");
    println!();
    println!("USAGE:");
    println!("  triage cost                          today + 7-day total");
    println!("  triage cost --by day [--days N]      per-day strip (default N=14)");
    println!("  triage cost --by cwd  [--top N]      top cwds by spend (default N=10)");
    println!("  triage cost --by session [--top N]   top sessions by spend (default N=10)");
    println!("  triage cost --by model               per-model split");
    println!("  triage cost --json                   machine-readable");
}

fn print_glance(r: &Rollup, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "today  {}", format_usd(r.total_today))?;
    let avg = if r.total_7d > 0.0 {
        r.total_7d / 7.0
    } else {
        0.0
    };
    writeln!(
        out,
        "7-day  {}  (avg {}/day)",
        format_usd(r.total_7d),
        format_usd(avg)
    )?;
    writeln!(out, "30-day {}", format_usd(r.total_30d))?;
    writeln!(out, "all    {}", format_usd(r.total_all))?;
    writeln!(
        out,
        "\n{} session{} scanned in {} ms",
        r.scanned_files,
        if r.scanned_files == 1 { "" } else { "s" },
        r.scan_duration_ms
    )?;
    Ok(())
}

fn print_by_day(r: &Rollup, days: usize, out: &mut impl Write) -> io::Result<()> {
    // Take the most recent N days that have data; pad missing days with $0
    // so the strip reads as a clean N-row table aligned to "today".
    let today_secs = day_start_secs(r.today);
    let mut all_days: HashMap<DayKey, f64> = r.days.iter().map(|d| (d.key, d.cost_usd)).collect();
    let mut rows: Vec<(DayKey, f64)> = Vec::with_capacity(days);
    for back in (0..days).rev() {
        let secs = today_secs - back as i64 * 86_400;
        let key = day_key_for_secs(secs);
        let cost = all_days.remove(&key).unwrap_or(0.0);
        rows.push((key, cost));
    }
    let peak = rows
        .iter()
        .map(|(_, c)| *c)
        .fold(0.0_f64, f64::max)
        .max(0.01);
    let bar_width = 30;
    for (k, c) in &rows {
        let filled = ((c / peak) * bar_width as f64).round() as usize;
        let bar: String = "█".repeat(filled);
        let label = if *k == r.today { " (today)" } else { "" };
        writeln!(
            out,
            "{}  {:>8}  {}{}",
            k.format(),
            format_usd(*c),
            bar,
            label
        )?;
    }
    writeln!(
        out,
        "\n{} session{} scanned in {} ms",
        r.scanned_files,
        if r.scanned_files == 1 { "" } else { "s" },
        r.scan_duration_ms
    )?;
    Ok(())
}

fn print_by_cwd(r: &Rollup, top_n: usize, out: &mut impl Write) -> io::Result<()> {
    let peak = r.cwds.first().map(|c| c.cost_usd).unwrap_or(0.01).max(0.01);
    let bar_width = 30;
    for c in r.cwds.iter().take(top_n) {
        let filled = ((c.cost_usd / peak) * bar_width as f64).round() as usize;
        let bar: String = "█".repeat(filled);
        let short = short_cwd(&c.cwd);
        writeln!(
            out,
            "{:>8}  {:>3} sess  {:<40}  {}",
            format_usd(c.cost_usd),
            c.session_count,
            short,
            bar
        )?;
    }
    Ok(())
}

fn print_by_session(r: &Rollup, top_n: usize, out: &mut impl Write) -> io::Result<()> {
    for s in r.sessions.iter().take(top_n) {
        let short = short_cwd(&s.cwd);
        let id_short: String = s.session_id.chars().take(8).collect();
        writeln!(
            out,
            "{:>8}  {}  {}",
            format_usd(s.cost_usd),
            id_short,
            short
        )?;
    }
    Ok(())
}

fn print_by_model(r: &Rollup, out: &mut impl Write) -> io::Result<()> {
    let total: f64 = r.models.iter().map(|m| m.cost_usd).sum();
    for m in &r.models {
        let pct = if total > 0.0 {
            100.0 * m.cost_usd / total
        } else {
            0.0
        };
        writeln!(
            out,
            "{:<10}  {:>8}  {:>5.1}%",
            m.model,
            format_usd(m.cost_usd),
            pct
        )?;
    }
    Ok(())
}

fn print_json(r: &Rollup, out: &mut impl Write) -> io::Result<()> {
    // Hand-rolled JSON: serde_json::to_string is overkill for ~8 known fields
    // and pulls in derive overhead we don't otherwise need here.
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = write!(
        s,
        "{{\"today\":\"{}\",\"total_today\":{:.6},\"total_7d\":{:.6},\"total_30d\":{:.6},\"total_all\":{:.6},\"scanned_files\":{},\"scan_duration_ms\":{},\"days\":[",
        r.today.format(),
        r.total_today,
        r.total_7d,
        r.total_30d,
        r.total_all,
        r.scanned_files,
        r.scan_duration_ms
    );
    for (i, d) in r.days.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"date\":\"{}\",\"cost_usd\":{:.6},\"tokens_in\":{},\"tokens_out\":{}}}",
            d.key.format(),
            d.cost_usd,
            d.tokens_in,
            d.tokens_out
        );
    }
    s.push_str("],\"cwds\":[");
    for (i, c) in r.cwds.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"cwd\":{},\"cost_usd\":{:.6},\"sessions\":{}}}",
            json_str(&c.cwd),
            c.cost_usd,
            c.session_count
        );
    }
    s.push_str("],\"models\":[");
    for (i, m) in r.models.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"model\":{},\"cost_usd\":{:.6}}}",
            json_str(&m.model),
            m.cost_usd
        );
    }
    s.push_str("]}");
    writeln!(out, "{s}")?;
    Ok(())
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Reverse-lookup: given an integer seconds-since-epoch (treated as the
/// "midnight" of a local day), produce the DayKey it belongs to. Used to walk
/// backwards from today and fill missing days with zero rows.
fn day_key_for_secs(secs: i64) -> DayKey {
    // 1 second past midnight to avoid landing exactly on a DST shift boundary
    // and getting yesterday's date back.
    local_day(UNIX_EPOCH + std::time::Duration::from_secs((secs + 1).max(0) as u64))
}

/// Trim a long absolute cwd to its last two path components so it fits in a
/// table column. `/Users/g/workspace/triage` → `workspace/triage`.
pub fn short_cwd(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= 2 {
        return cwd.to_string();
    }
    parts[parts.len() - 2..].join("/")
}

/// Format USD with sub-cent precision when small, dollars/cents otherwise.
pub fn format_usd(v: f64) -> String {
    if v == 0.0 {
        "$0.00".to_string()
    } else if v < 0.01 {
        format!("${v:.4}")
    } else if v < 100.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.0}")
    }
}
