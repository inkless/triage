//! Discovery for harnesses triage tracks through their tmux pane alone:
//! opencode and pi. A pane whose foreground command is the harness becomes a
//! row; the harness's own session store adds a name and busy/idle state when
//! exactly one such pane sits in the session's cwd. Two panes of the same
//! harness in one cwd cannot be told apart from outside, so neither borrows a
//! session name. Unnamed panes fall back to a user-set tmux window name, then
//! to `provider:pane_id`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use crate::models::{Pane, Provider, Session};

/// What a harness's session store says about the latest session in a cwd.
#[derive(Debug, Clone, PartialEq)]
struct SessionMeta {
    id: String,
    cwd: PathBuf,
    /// The session's own name: pi's `/name`, opencode's renameable title.
    name: Option<String>,
    /// The latest thing the user asked for. Shown as the row's headline:
    /// opencode's title is the session's name, so repeating it there would
    /// carry no new information.
    last_prompt: Option<String>,
    busy: Option<bool>,
    created_ms: u64,
    updated_ms: u64,
    idle_since_ms: Option<u64>,
}

/// `skip` holds panes already owned by a Claude or Codex session: a harness
/// one of those agents runs as a tool is not a pane agent.
pub fn discover(
    panes: &HashMap<u32, Pane>,
    ppid_map: &HashMap<u32, u32>,
    skip: &HashSet<String>,
) -> Vec<Session> {
    let children = children_of(ppid_map);
    let comm = |pid: u32| crate::mailbox::proc_info(pid).ok().map(|info| info.comm);
    let sandboxed = if panes
        .values()
        .any(|pane| pane_runs_container(pane, &children, &comm))
    {
        sandbox_agents(panes)
    } else {
        HashMap::new()
    };
    let mut by_provider: HashMap<Provider, Vec<&Pane>> = HashMap::new();
    let mut agent_dirs: HashMap<String, PathBuf> = HashMap::new();
    let mut started = HashMap::new();
    for pane in panes.values().filter(|p| !skip.contains(&p.pane_id)) {
        let detected = pane_provider(pane, &children, &comm)
            .map(|(provider, harness)| (provider, harness, None))
            .or_else(|| {
                sandboxed.get(&pane.pane_id).map(|agent| {
                    (
                        agent.provider,
                        Some(agent.pid),
                        Some(agent.agent_dir.clone()),
                    )
                })
            });
        let Some((provider, harness, agent_dir)) = detected else {
            continue;
        };
        let run = harness.map(|pid| harness_run(provider, pid));
        if run.as_ref().is_some_and(|run| !run.interactive) {
            continue;
        }
        by_provider.entry(provider).or_default().push(pane);
        if let Some(dir) = agent_dir {
            agent_dirs.insert(pane.pane_id.clone(), dir);
        }
        if let Some(ms) = run.and_then(|run| run.started_ms) {
            started.insert(pane.pane_id.clone(), ms);
        }
    }
    let mut out = Vec::new();
    for (provider, panes) in by_provider {
        match provider {
            Provider::Opencode => {
                let metas = opencode_metas(&opencode_db_path());
                out.extend(sessions_for_panes(provider, &panes, &metas, &started));
            }
            Provider::Pi => {
                let mut by_dir: HashMap<PathBuf, Vec<&Pane>> = HashMap::new();
                for pane in panes {
                    let dir = agent_dirs
                        .get(&pane.pane_id)
                        .cloned()
                        .unwrap_or_else(pi_agent_dir);
                    by_dir.entry(dir).or_default().push(pane);
                }
                for (dir, group) in by_dir {
                    let metas = pi_metas(&dir, &group);
                    out.extend(sessions_for_panes(Provider::Pi, &group, &metas, &started));
                }
            }
            Provider::Claude | Provider::Codex => {}
        }
    }
    out
}

/// A harness inside a sandbox container, joined to its pane by labels the
/// sandbox wrapper sets on the container.
#[derive(Debug)]
struct SandboxAgent {
    provider: Provider,
    pid: u32,
    agent_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct SandboxContainer {
    #[serde(rename = "Pid")]
    pid: u32,
    #[serde(default, rename = "Labels")]
    labels: HashMap<String, String>,
}

fn is_container_runtime(command: &str) -> bool {
    matches!(command, "podman" | "docker" | "nerdctl")
}

/// The pane's foreground command is usually the runtime, but a shell that runs
/// the wrapper without job control leaves the shell as the pane command and
/// the runtime one level down.
fn pane_runs_container(
    pane: &Pane,
    children: &HashMap<u32, Vec<u32>>,
    comm: &dyn Fn(u32) -> Option<String>,
) -> bool {
    if is_container_runtime(&pane.current_command) {
        return true;
    }
    let kids = children.get(&pane.pid).into_iter().flatten();
    let grandkids = kids
        .clone()
        .flat_map(|kid| children.get(kid).into_iter().flatten());
    std::iter::once(&pane.pid)
        .chain(kids)
        .chain(grandkids)
        .any(|&pid| comm(pid).is_some_and(|name| is_container_runtime(&name)))
}

/// Rootless podman reparents `conmon` to init, so the container's harness is
/// never in the pane's process tree. A sandbox that labels its container with
/// the pane id and its state directory is the only thing that joins the two.
fn sandbox_agents(panes: &HashMap<u32, Pane>) -> HashMap<String, SandboxAgent> {
    let Ok(out) = Command::new("podman")
        .args(["ps", "--filter", "label=triage.harness", "--format", "json"])
        .output()
    else {
        return HashMap::new();
    };
    if !out.status.success() {
        return HashMap::new();
    }
    let pane_ids: HashSet<&str> = panes.values().map(|pane| pane.pane_id.as_str()).collect();
    parse_sandbox_agents(&out.stdout, &pane_ids)
}

fn parse_sandbox_agents(json: &[u8], panes: &HashSet<&str>) -> HashMap<String, SandboxAgent> {
    let rows: Vec<SandboxContainer> = serde_json::from_slice(json).unwrap_or_default();
    rows.into_iter()
        .filter_map(|row| {
            let pane_id = row.labels.get("triage.pane")?;
            if !panes.contains(pane_id.as_str()) {
                return None;
            }
            let provider = match row.labels.get("triage.harness").map(String::as_str) {
                Some("pi") => Provider::Pi,
                _ => return None,
            };
            let agent_dir = PathBuf::from(row.labels.get("triage.agent_dir")?);
            Some((
                pane_id.clone(),
                SandboxAgent {
                    provider,
                    pid: row.pid,
                    agent_dir,
                },
            ))
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct HarnessRun {
    started_ms: Option<u64>,
    /// False for a server or one-shot run (`opencode serve`, `pi -p`): no
    /// one reads its pane, so it is not listed or sent to.
    interactive: bool,
}

/// One `ps` per harness pane: start time from elapsed time (portable across
/// Linux and macOS, unlike the kernels' own start fields) and arguments.
fn harness_run(provider: Provider, pid: u32) -> HarnessRun {
    let out = Command::new("ps")
        .args(["-o", "etime=,args=", "-p", &pid.to_string()])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    parse_harness_run(provider, &out, time_to_ms(SystemTime::now()))
}

fn parse_harness_run(provider: Provider, ps_out: &str, now_ms: u64) -> HarnessRun {
    let line = ps_out.trim();
    let (etime, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let started_ms = crate::agent_comm::elapsed_seconds(etime)
        .and_then(|elapsed| now_ms.checked_sub(elapsed * 1000));
    let args: Vec<&str> = args.split_whitespace().skip(1).collect();
    let interactive = match provider {
        Provider::Opencode => !args
            .iter()
            .find(|a| !a.starts_with('-'))
            .is_some_and(|sub| matches!(*sub, "serve" | "run" | "web" | "acp")),
        Provider::Pi => !args.iter().enumerate().any(|(i, a)| {
            matches!(*a, "-p" | "--print" | "--mode=rpc" | "--mode=json")
                || (*a == "--mode"
                    && args
                        .get(i + 1)
                        .is_some_and(|m| matches!(*m, "rpc" | "json")))
        }),
        Provider::Claude | Provider::Codex => true,
    };
    HarnessRun {
        started_ms,
        interactive,
    }
}

fn children_of(ppid_map: &HashMap<u32, u32>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &ppid) in ppid_map {
        children.entry(ppid).or_default().push(pid);
    }
    children
}

/// The pane's foreground command names the harness when it was started from
/// an interactive shell. Launched as `tmux new-window opencode`, the shell
/// runs it without job control and tmux reports the shell instead, so also
/// look at the pane's children and grandchildren. A shell that execs the
/// harness makes it the pane's own process.
/// Returns the harness and, when found, its process id.
fn pane_provider(
    pane: &Pane,
    children: &HashMap<u32, Vec<u32>>,
    comm: &dyn Fn(u32) -> Option<String>,
) -> Option<(Provider, Option<u32>)> {
    let kids = children.get(&pane.pid).into_iter().flatten();
    let grandkids = kids
        .clone()
        .flat_map(|kid| children.get(kid).into_iter().flatten());
    let harness = std::iter::once(&pane.pid)
        .chain(kids)
        .chain(grandkids)
        .find_map(|&pid| Some((provider_for_command(&comm(pid)?)?, pid)));
    match (provider_for_command(&pane.current_command), harness) {
        (Some(provider), Some((found, pid))) if found == provider => Some((provider, Some(pid))),
        (Some(provider), _) => Some((provider, None)),
        (None, Some((provider, pid))) => Some((provider, Some(pid))),
        (None, None) => None,
    }
}

fn provider_for_command(command: &str) -> Option<Provider> {
    match command {
        "opencode" => Some(Provider::Opencode),
        "pi" => Some(Provider::Pi),
        _ => None,
    }
}

/// Slack for clock and `ps` rounding when comparing a session's last write
/// against its harness's start.
const START_SLACK_MS: u64 = 2_000;

/// `started` maps a pane to its harness's start time. A session last written
/// before the harness started belongs to an earlier run in the same cwd, so
/// such a pane shows as a fresh, idle session rather than inheriting it.
fn sessions_for_panes(
    provider: Provider,
    panes: &[&Pane],
    metas: &[SessionMeta],
    started: &HashMap<String, u64>,
) -> Vec<Session> {
    let mut panes_per_cwd: HashMap<&Path, usize> = HashMap::new();
    for pane in panes {
        *panes_per_cwd.entry(pane.cwd.as_path()).or_default() += 1;
    }
    panes
        .iter()
        .map(|pane| {
            if panes_per_cwd[pane.cwd.as_path()] != 1 {
                return pane_session(provider, pane, None);
            }
            let since = started.get(&pane.pane_id).copied();
            match (latest_in(metas, &pane.cwd, since), since) {
                (Some(meta), _) => pane_session(provider, pane, Some(meta)),
                (None, Some(_)) => pane_session(provider, pane, Some(&fresh_meta(pane))),
                (None, None) => pane_session(provider, pane, None),
            }
        })
        .collect()
}

fn latest_in<'a>(
    metas: &'a [SessionMeta],
    cwd: &Path,
    since_ms: Option<u64>,
) -> Option<&'a SessionMeta> {
    metas
        .iter()
        .filter(|m| m.cwd == cwd)
        .filter(|m| since_ms.is_none_or(|since| m.updated_ms + START_SLACK_MS >= since))
        .max_by_key(|m| m.updated_ms)
}

/// A harness that has not written its session yet: idle, no history.
fn fresh_meta(pane: &Pane) -> SessionMeta {
    SessionMeta {
        id: pane.pane_id.clone(),
        cwd: pane.cwd.clone(),
        name: None,
        last_prompt: None,
        busy: Some(false),
        created_ms: 0,
        updated_ms: 0,
        idle_since_ms: None,
    }
}

fn pane_session(provider: Provider, pane: &Pane, meta: Option<&SessionMeta>) -> Session {
    // `pid` is the pane's root process, not the harness: identity for these
    // providers is the pane itself (see agent_comm::resolve_caller).
    let status = match meta.and_then(|m| m.busy) {
        Some(true) => "busy",
        Some(false) => "idle",
        None => "unknown",
    };
    let mut session = Session::new(
        provider,
        pane.pid,
        meta.map_or_else(|| pane.pane_id.clone(), |m| m.id.clone()),
        pane.cwd.clone(),
        Some(
            meta.and_then(|m| m.name.clone())
                .or_else(|| crate::models::useful_window_name(pane))
                .unwrap_or_else(|| format!("{}:{}", provider.name(), pane.pane_id)),
        ),
        status.to_string(),
        meta.map_or(0, |m| m.created_ms),
        meta.map_or(0, |m| m.updated_ms),
        None,
    );
    session.last_stop_at = meta.and_then(|m| m.idle_since_ms).map(ms_to_time);
    session.last_event_at = meta.map(|m| ms_to_time(m.updated_ms));
    session.last_prompt = meta.and_then(|m| m.last_prompt.clone());
    session.pane = Some(pane.clone());
    session
}

fn ms_to_time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn opencode_db_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"));
    data.join("opencode/opencode.db")
}

const OPENCODE_QUERY: &str = "select s.id, s.directory, s.title, s.time_created, \
     s.time_updated, s.time_busy, s.time_idle, \
     (select substr(json_extract(m.data, '$.text'), 1, 300) \
        from session_message m \
       where m.session_id = s.id and m.type = 'user' \
       order by m.seq desc limit 1) as last_user_text \
     from session_v2 s where s.parent_id is null \
     order by s.time_updated desc limit 500";

/// Best effort: a missing database, missing `sqlite3`, or a schema change
/// leaves opencode panes listed but unnamed.
fn opencode_metas(db: &Path) -> Vec<SessionMeta> {
    if !db.exists() {
        return Vec::new();
    }
    let Ok(out) = Command::new("sqlite3")
        .args(["-readonly", "-json", &db.to_string_lossy(), OPENCODE_QUERY])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_opencode_rows(&out.stdout)
}

/// The pid of the shared opencode server, which runs every opencode tool
/// shell, from the `service.json` it writes on start.
pub fn opencode_server_pid() -> Option<u32> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/state"));
    let text = fs::read_to_string(state.join("opencode/service.json")).ok()?;
    let service: Value = serde_json::from_str(&text).ok()?;
    service.get("pid")?.as_u64()?.try_into().ok()
}

/// The directory of one opencode session, for checking a caller's
/// `OPENCODE_SESSION_ID` against the pane it claims.
pub fn opencode_session_dir(id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let db = opencode_db_path();
    let query = format!("select directory from session_v2 where id = '{id}'");
    let out = Command::new("sqlite3")
        .args(["-readonly", "-json", &db.to_string_lossy(), &query])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let rows: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
    rows.first()?.get("directory")?.as_str().map(PathBuf::from)
}

#[derive(Deserialize)]
struct OpencodeRow {
    id: String,
    directory: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    time_created: Option<u64>,
    #[serde(default)]
    time_updated: Option<u64>,
    #[serde(default)]
    time_busy: Option<u64>,
    #[serde(default)]
    time_idle: Option<u64>,
    #[serde(default)]
    last_user_text: Option<String>,
}

fn parse_opencode_rows(json: &[u8]) -> Vec<SessionMeta> {
    let rows: Vec<OpencodeRow> = serde_json::from_slice(json).unwrap_or_default();
    rows.into_iter()
        .map(|row| {
            // Both stamps mark the latest transition; whichever is newer is
            // the current state.
            let busy = match (row.time_busy, row.time_idle) {
                (Some(b), Some(i)) => Some(b > i),
                (Some(_), None) => Some(true),
                (None, Some(_)) => Some(false),
                (None, None) => None,
            };
            let idle_since_ms = (busy == Some(false)).then_some(row.time_idle).flatten();
            SessionMeta {
                id: row.id,
                cwd: PathBuf::from(row.directory),
                name: row
                    .title
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
                last_prompt: row
                    .last_user_text
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty()),
                busy,
                created_ms: row.time_created.unwrap_or(0),
                updated_ms: row.time_updated.unwrap_or(0),
                idle_since_ms,
            }
        })
        .collect()
}

fn pi_agent_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".pi/agent"))
}

/// pi's per-cwd session directory name: `/a/b` → `--a-b--`.
fn pi_session_dir_name(cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(&cwd);
    let safe: String = trimmed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{safe}--")
}

fn pi_metas(agent_dir: &Path, panes: &[&Pane]) -> Vec<SessionMeta> {
    let mut cwds: Vec<&Path> = panes.iter().map(|p| p.cwd.as_path()).collect();
    cwds.sort();
    cwds.dedup();
    cwds.into_iter()
        .filter_map(|cwd| {
            let dir = agent_dir.join("sessions").join(pi_session_dir_name(cwd));
            let path = latest_jsonl(&dir)?;
            let text = fs::read_to_string(&path).ok()?;
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
            parse_pi_session(&text, time_to_ms(modified))
        })
        .collect()
}

fn latest_jsonl(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .filter_map(|p| {
            let modified = fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((modified, p))
        })
        .max()
        .map(|(_, p)| p)
}

fn time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn timestamp_ms(entry: &Value) -> Option<u64> {
    entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(crate::transcript::parse_timestamp)
        .map(time_to_ms)
}

/// A pi session file is JSONL: a `session` header (id, cwd), then entries.
/// The latest `session_info` carries the user-set name; the last `message`
/// tells busy from idle — an assistant message that stopped for any reason
/// other than a tool call ends the turn.
fn parse_pi_session(text: &str, modified_ms: u64) -> Option<SessionMeta> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Value = serde_json::from_str(lines.next()?).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    let id = header.get("id")?.as_str()?.to_string();
    let cwd = PathBuf::from(header.get("cwd")?.as_str()?);
    let created_ms = timestamp_ms(&header).unwrap_or(0);

    let mut name = None;
    let mut last_prompt = None;
    let mut last_message: Option<Value> = None;
    for line in lines {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("session_info") => {
                name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
            }
            Some("message") => {
                if let Some(prompt) = pi_user_text(&entry) {
                    last_prompt = Some(prompt);
                }
                last_message = Some(entry);
            }
            _ => {}
        }
    }

    let (busy, idle_since_ms) = match &last_message {
        None => (false, None),
        Some(entry) => {
            let message = entry.get("message");
            let role = message.and_then(|m| m.get("role")).and_then(Value::as_str);
            let stop = message
                .and_then(|m| m.get("stopReason"))
                .and_then(Value::as_str);
            let turn_over = role == Some("assistant") && stop.is_some_and(|s| s != "toolUse");
            (!turn_over, turn_over.then(|| timestamp_ms(entry)).flatten())
        }
    };
    Some(SessionMeta {
        id,
        cwd,
        name,
        last_prompt,
        busy: Some(busy),
        created_ms,
        updated_ms: modified_ms,
        idle_since_ms,
    })
}

/// The text of a pi user message. Content is either a string or an array of
/// typed parts; only `text` parts carry the prompt. Capped so a pasted file
/// cannot ride into every redraw.
fn pi_user_text(entry: &Value) -> Option<String> {
    let message = entry.get("message")?;
    if message.get("role")?.as_str()? != "user" {
        return None;
    }
    let text = match message.get("content")? {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pid: u32, pane_id: &str, cwd: &str, command: &str) -> Pane {
        Pane {
            target: format!("s:{pid}.0"),
            tmux_session: "s".to_string(),
            window_name: command.to_string(),
            pane_id: pane_id.to_string(),
            pid,
            tty: String::new(),
            current_command: command.to_string(),
            cwd: PathBuf::from(cwd),
            active: true,
        }
    }

    fn meta(id: &str, cwd: &str, updated_ms: u64) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            cwd: PathBuf::from(cwd),
            name: None,
            last_prompt: Some(id.to_string()),
            busy: Some(false),
            created_ms: 1,
            updated_ms,
            idle_since_ms: Some(updated_ms),
        }
    }

    #[test]
    fn only_harness_commands_are_pane_agents() {
        assert_eq!(provider_for_command("opencode"), Some(Provider::Opencode));
        assert_eq!(provider_for_command("pi"), Some(Provider::Pi));
        for other in ["fish", "node", "claude", "codex", "2.1.139", "pip"] {
            assert_eq!(provider_for_command(other), None, "{other}");
        }
    }

    #[test]
    fn a_container_runtime_is_detected_under_the_pane_shell() {
        let children = children_of(&HashMap::from([(20, 10), (30, 20)]));
        let comm = |pid: u32| match pid {
            20 => Some("fish".to_string()),
            30 => Some("podman".to_string()),
            _ => None,
        };
        let shell = pane(10, "%1", "/w", "fish");
        assert!(pane_runs_container(&shell, &children, &comm));
        let foreground = pane(10, "%1", "/w", "podman");
        assert!(pane_runs_container(&foreground, &HashMap::new(), &|_| None));
        let plain = pane(11, "%2", "/w", "fish");
        assert!(!pane_runs_container(&plain, &HashMap::new(), &|_| None));
    }

    #[test]
    fn parses_only_labeled_containers_for_known_panes() {
        let json = br#"[
            {"Pid":42,"Labels":{"triage.harness":"pi","triage.pane":"%23","triage.agent_dir":"/s"}},
            {"Pid":43,"Labels":{"triage.harness":"pi","triage.pane":"%99","triage.agent_dir":"/s"}},
            {"Pid":44,"Labels":{"triage.harness":"claude","triage.pane":"%23","triage.agent_dir":"/s"}},
            {"Pid":45,"Labels":{"triage.harness":"pi","triage.pane":"%23"}}
        ]"#;
        let panes: HashSet<&str> = ["%23"].into_iter().collect();
        let agents = parse_sandbox_agents(json, &panes);
        assert_eq!(agents.len(), 1);
        let agent = &agents["%23"];
        assert_eq!(agent.provider, Provider::Pi);
        assert_eq!(agent.pid, 42);
        assert_eq!(agent.agent_dir, PathBuf::from("/s"));
        assert!(parse_sandbox_agents(b"not json", &panes).is_empty());
    }

    #[test]
    fn finds_a_harness_the_shell_runs_without_job_control() {
        let children = children_of(&HashMap::from([(20, 10), (30, 20), (40, 11)]));
        let comm = |pid: u32| {
            let name = match pid {
                20 => "sh",
                30 => "opencode",
                40 => "pi",
                _ => "other",
            };
            Some(name.to_string())
        };
        let grandchild = pane(10, "%1", "/w", "fish");
        assert_eq!(
            pane_provider(&grandchild, &children, &comm),
            Some((Provider::Opencode, Some(30)))
        );
        let child = pane(11, "%2", "/w", "fish");
        assert_eq!(
            pane_provider(&child, &children, &comm),
            Some((Provider::Pi, Some(40)))
        );
        let foreground = pane(11, "%4", "/w", "pi");
        assert_eq!(
            pane_provider(&foreground, &children, &comm),
            Some((Provider::Pi, Some(40)))
        );
        let plain = pane(12, "%3", "/w", "fish");
        assert_eq!(pane_provider(&plain, &children, &comm), None);
    }

    #[test]
    fn finds_a_harness_that_is_the_pane_process() {
        let comm = |pid: u32| (pid == 13).then(|| "pi".to_string());
        let exec = pane(13, "%5", "/w", "pi");
        assert_eq!(
            pane_provider(&exec, &HashMap::new(), &comm),
            Some((Provider::Pi, Some(13)))
        );
    }

    #[test]
    fn server_and_one_shot_harness_runs_are_not_interactive() {
        let interactive = |provider, out| parse_harness_run(provider, out, 100_000).interactive;
        for out in [
            "01:02 opencode serve --port 4096",
            "01:02 /usr/bin/opencode run fix it",
            "01:02 opencode --print-logs web",
        ] {
            assert!(!interactive(Provider::Opencode, out), "{out}");
        }
        for out in ["01:02 opencode", "01:02 opencode -c", "01:02 opencode /w/a"] {
            assert!(interactive(Provider::Opencode, out), "{out}");
        }
        for out in [
            "01:02 pi -p hello",
            "01:02 pi --print hello",
            "01:02 pi --mode rpc",
            "01:02 pi --mode=json",
        ] {
            assert!(!interactive(Provider::Pi, out), "{out}");
        }
        for out in ["01:02 pi", "01:02 pi --mode text", "01:02 pi --continue"] {
            assert!(interactive(Provider::Pi, out), "{out}");
        }
        assert_eq!(
            parse_harness_run(Provider::Pi, "  01:02 pi\n", 100_000).started_ms,
            Some(38_000)
        );
        assert!(interactive(Provider::Pi, ""));
    }

    #[test]
    fn parses_opencode_session_rows() {
        let json = br#"[
            {"id":"ses_busy","directory":"/w/a","title":"Fix parser","parent_id":null,
             "time_created":10,"time_updated":50,"time_busy":40,"time_idle":30,
             "last_user_text":"please fix the parser"},
            {"id":"ses_idle","directory":"/w/b","title":"  ","time_created":5,
             "time_updated":20,"time_busy":12,"time_idle":18},
            {"id":"ses_new","directory":"/w/c","time_created":1,"time_updated":1}
        ]"#;
        let metas = parse_opencode_rows(json);
        assert_eq!(metas.len(), 3);
        assert_eq!(metas[0].busy, Some(true));
        assert_eq!(metas[0].name.as_deref(), Some("Fix parser"));
        assert_eq!(
            metas[0].last_prompt.as_deref(),
            Some("please fix the parser")
        );
        assert_eq!(metas[0].idle_since_ms, None);
        assert_eq!(metas[1].busy, Some(false));
        assert_eq!(metas[1].name, None);
        assert_eq!(metas[1].last_prompt, None);
        assert_eq!(metas[1].idle_since_ms, Some(18));
        assert_eq!(metas[2].busy, None);
        assert!(parse_opencode_rows(b"not json").is_empty());
    }

    #[test]
    fn pi_session_dir_matches_pi_encoding() {
        assert_eq!(
            pi_session_dir_name(Path::new("/home/inkless/workspace/bazhen")),
            "--home-inkless-workspace-bazhen--"
        );
    }

    #[test]
    fn parses_pi_session_name_and_state() {
        let header = r#"{"type":"session","version":3,"id":"01a1","timestamp":"2026-10-14T18:49:13.644Z","cwd":"/w/a"}"#;
        let named = r#"{"type":"session_info","id":"i1","timestamp":"2026-10-14T18:49:14Z","name":"pi-worker"}"#;
        let user = r#"{"type":"message","id":"m1","timestamp":"2026-10-14T18:49:15Z","message":{"role":"user","content":[{"type":"text","text":"fix the parser"}]}}"#;
        let tool = r#"{"type":"message","id":"m2","timestamp":"2026-10-14T18:49:16Z","message":{"role":"assistant","stopReason":"toolUse"}}"#;
        let done = r#"{"type":"message","id":"m3","timestamp":"2026-10-14T18:49:17Z","message":{"role":"assistant","stopReason":"stop"}}"#;

        let fresh = parse_pi_session(&format!("{header}\n"), 99).unwrap();
        assert_eq!(fresh.id, "01a1");
        assert_eq!(fresh.cwd, PathBuf::from("/w/a"));
        assert_eq!(fresh.busy, Some(false));
        assert_eq!(fresh.updated_ms, 99);

        let working =
            parse_pi_session(&format!("{header}\n{named}\n{user}\n{tool}\n"), 99).unwrap();
        assert_eq!(working.name.as_deref(), Some("pi-worker"));
        assert_eq!(working.last_prompt.as_deref(), Some("fix the parser"));
        assert_eq!(working.busy, Some(true));

        let idle = parse_pi_session(&format!("{header}\n{user}\n{done}\n"), 99).unwrap();
        assert_eq!(idle.busy, Some(false));
        assert!(idle.idle_since_ms.is_some());

        assert!(parse_pi_session(user, 99).is_none());
    }

    #[test]
    fn names_a_pane_only_when_it_is_alone_in_its_cwd() {
        let solo = pane(10, "%30", "/w/a", "opencode");
        let twin_a = pane(11, "%31", "/w/b", "opencode");
        let twin_b = pane(12, "%32", "/w/b", "opencode");
        let metas = vec![
            meta("ses_old", "/w/a", 5),
            meta("ses_new", "/w/a", 9),
            meta("ses_b", "/w/b", 9),
        ];
        let sessions = sessions_for_panes(
            Provider::Opencode,
            &[&solo, &twin_a, &twin_b],
            &metas,
            &HashMap::new(),
        );

        let solo = sessions.iter().find(|s| s.pid == 10).unwrap();
        assert_eq!(solo.session_id, "ses_new");
        assert_eq!(solo.name.as_deref(), Some("opencode:%30"));
        assert_eq!(solo.headline, None);
        assert_eq!(solo.last_prompt.as_deref(), Some("ses_new"));
        assert_eq!(solo.status, "idle");
        assert_eq!(solo.pane.as_ref().unwrap().pane_id, "%30");

        for pid in [11, 12] {
            let twin = sessions.iter().find(|s| s.pid == pid).unwrap();
            assert_eq!(twin.session_id, twin.pane.as_ref().unwrap().pane_id);
            let pane_id = &twin.pane.as_ref().unwrap().pane_id;
            assert_eq!(twin.name, Some(format!("opencode:{pane_id}")));
            assert_eq!(twin.headline, None);
            assert_eq!(twin.last_prompt, None);
            assert_eq!(twin.status, "unknown");
        }
    }

    #[test]
    fn a_pane_is_named_by_its_session_then_its_window() {
        let mut named = pane(10, "%10", "/w/a", "opencode");
        named.window_name = "scratch".to_string();
        let mut with_name = meta("ses_a", "/w/a", 9);
        with_name.name = Some("triage-dev".to_string());
        let sessions =
            sessions_for_panes(Provider::Opencode, &[&named], &[with_name], &HashMap::new());
        assert_eq!(sessions[0].name.as_deref(), Some("triage-dev"));

        let mut windowed = pane(11, "%11", "/w/b", "opencode");
        windowed.window_name = "scratch".to_string();
        let sessions = sessions_for_panes(Provider::Opencode, &[&windowed], &[], &HashMap::new());
        assert_eq!(sessions[0].name.as_deref(), Some("scratch"));

        let mut auto_named = pane(12, "%12", "/w/c", "opencode");
        auto_named.window_name = "opencode".to_string();
        let sessions = sessions_for_panes(Provider::Opencode, &[&auto_named], &[], &HashMap::new());
        assert_eq!(sessions[0].name.as_deref(), Some("opencode:%12"));
    }

    #[test]
    fn a_session_from_an_earlier_run_does_not_describe_a_new_harness() {
        let pane = pane(10, "%48", "/w/a", "pi");
        let metas = vec![meta("old", "/w/a", 1_000_000)];

        let restarted = HashMap::from([("%48".to_string(), 2_000_000)]);
        let fresh = &sessions_for_panes(Provider::Pi, &[&pane], &metas, &restarted)[0];
        assert_eq!(fresh.session_id, "%48");
        assert_eq!(fresh.status, "idle");
        assert_eq!(fresh.headline, None);
        assert_eq!(
            crate::classifier::classify(fresh, SystemTime::now()),
            crate::models::AttentionState::Fresh
        );

        let earlier = HashMap::from([("%48".to_string(), 999_000)]);
        let current = &sessions_for_panes(Provider::Pi, &[&pane], &metas, &earlier)[0];
        assert_eq!(current.session_id, "old");
    }
}
