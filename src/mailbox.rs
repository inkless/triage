//! File mailbox for peer messages. A message's state is the directory it sits
//! in; processes claim mail by renaming it, so every transition is atomic.
#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::Provider;

pub const DELIVERY_BUDGET_CHARS: usize = 5_000;
const OVERFLOW_HEAD_CHARS: usize = 500;
const MAX_LABEL_CHARS: usize = 32;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn random_bytes(buf: &mut [u8]) {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len());
    }
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0;
        while filled < buf.len() {
            let n = unsafe {
                libc::getrandom(buf[filled..].as_mut_ptr().cast(), buf.len() - filled, 0)
            };
            if n < 0 {
                assert_eq!(
                    io::Error::last_os_error().kind(),
                    io::ErrorKind::Interrupted,
                    "getrandom failed"
                );
                continue;
            }
            filled += n as usize;
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn new_nonce() -> String {
    let mut buf = [0u8; 8];
    random_bytes(&mut buf);
    hex(&buf)
}

/// UUIDv7. The 12-bit `rand_a` field is a counter within the millisecond, so
/// ids minted by one process sort in creation order.
pub fn new_message_id() -> String {
    static LAST: Mutex<(u64, u16)> = Mutex::new((0, 0));
    let mut rand = [0u8; 10];
    random_bytes(&mut rand);
    let (ms, counter) = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        if now > last.0 {
            *last = (now, u16::from_be_bytes([rand[8], rand[9]]) & 0x07ff);
        } else if last.1 >= 0x0fff {
            *last = (last.0 + 1, 0);
        } else {
            last.1 += 1;
        }
        *last
    };
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x70 | (counter >> 8) as u8;
    b[7] = counter as u8;
    b[8..].copy_from_slice(&rand[..8]);
    b[8] = 0x80 | (b[8] & 0x3f);
    let h = hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

/// Every id read from a payload, file name or file contents passes through
/// here before it reaches a path or argv.
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => is_lower_hex(b),
        })
}

pub fn is_nonce(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(is_lower_hex)
}

/// Codex ids are UUIDv7, whose prefixes collide, so agents display by suffix.
pub fn short_id(agent: &str) -> &str {
    &agent[agent.len().saturating_sub(8)..]
}

fn invalid(what: &str, value: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid {what}: {value:?}"),
    )
}

fn check_uuid(what: &str, value: &str) -> io::Result<()> {
    if is_uuid(value) {
        Ok(())
    } else {
        Err(invalid(what, value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub v: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub from: Sender,
    pub to: Recipient,
    pub body: String,
    pub attempt: u32,
    pub bounce_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_via: Option<DeliveredVia>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub agent: String,
    pub session: String,
    pub provider: Provider,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub agent: String,
    pub session_at_send: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeliveredVia {
    ClaudeWaiter,
    ClaudePostToolUse,
    CodexUserPromptSubmit,
    CodexPostToolUse,
    LegacyPaste,
    Inbox,
    InboxShow,
}

impl Message {
    fn validate(&self) -> io::Result<()> {
        check_uuid("message id", &self.id)?;
        check_uuid("sender agent", &self.from.agent)?;
        check_uuid("sender session", &self.from.session)?;
        check_uuid("recipient agent", &self.to.agent)?;
        check_uuid("recipient session", &self.to.session_at_send)?;
        if let Some(bounced) = &self.bounce_of {
            check_uuid("bounced message id", bounced)?;
        }
        Ok(())
    }
}

/// Sidecar written next to an inflight message. `nonce` is the header nonce
/// the waiter looks for in the transcript to confirm delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub nonce: String,
    pub claimed_at_ms: u64,
    pub waiter_pid: u32,
    pub waiter_start: u64,
    #[serde(default)]
    pub transcript_path: String,
    #[serde(default)]
    pub transcript_offset: u64,
    pub printed_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Inflight,
    Delivered,
    Notified,
    Read,
    Undeliverable,
    Pasted,
}

impl State {
    fn dir(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Inflight => "inflight",
            State::Delivered => "delivered",
            State::Notified => "notified",
            State::Read => "read",
            State::Undeliverable => "undeliverable",
            State::Pasted => "pasted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostId {
    pub pid: u32,
    pub start: u64,
}

impl HostId {
    fn key(self) -> String {
        format!("{}-{}", self.pid, self.start)
    }

    fn parse(key: &str) -> Option<Self> {
        let (pid, start) = key.split_once('-')?;
        Some(Self {
            pid: pid.parse().ok()?,
            start: start.parse().ok()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostRecord {
    pub v: u32,
    pub provider: Provider,
    pub hook_version: String,
    pub current_session: String,
    pub updated_at_ms: u64,
}

/// A claimed message: which session and host process claimed it is encoded
/// in the file name so reconcile can judge the claim without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub id: String,
    pub session: String,
    pub host: HostId,
}

impl Inflight {
    fn file_name(&self) -> String {
        format!(
            "{}@{}@{}@{}.json",
            self.id, self.session, self.host.pid, self.host.start
        )
    }

    fn parse(file_name: &str) -> Option<Self> {
        let stem = file_name.strip_suffix(".json")?;
        let mut parts = stem.split('@');
        let (id, session, pid, start) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || !is_uuid(id) || !is_uuid(session) {
            return None;
        }
        Some(Self {
            id: id.to_string(),
            session: session.to_string(),
            host: HostId {
                pid: pid.parse().ok()?,
                start: start.parse().ok()?,
            },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockKind {
    Waiter,
    Wake,
    Helper,
}

/// Held until dropped; the kernel releases it if the holder dies.
pub struct FileLock {
    _file: File,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn default_root() -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            });
        base.join("triage")
    }

    pub fn open_default() -> Self {
        Self::at(Self::default_root())
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mailbox(&self, agent: &str) -> io::Result<Mailbox> {
        check_uuid("agent id", agent)?;
        Ok(Mailbox {
            dir: self.root.join("mail").join(agent),
        })
    }

    pub fn link_lineage(&self, session: &str, root: &str) -> io::Result<String> {
        check_uuid("session id", session)?;
        check_uuid("root agent id", root)?;
        let dir = self.root.join("lineage");
        ensure_dir(&dir)?;
        let path = dir.join(session);
        if let Some(existing) = self.lineage_root(session) {
            return Ok(existing);
        }
        let temp = dir.join(format!(".{session}.{}", new_nonce()));
        write_new(&temp, root.as_bytes())?;
        let linked = fs::hard_link(&temp, &path);
        let _ = fs::remove_file(&temp);
        match linked {
            Ok(()) => Ok(root.to_string()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self
                .lineage_root(session)
                .ok_or_else(|| invalid("lineage record", session)),
            Err(e) => Err(e),
        }
    }

    pub fn lineage_root(&self, session: &str) -> Option<String> {
        if !is_uuid(session) {
            return None;
        }
        let text = fs::read_to_string(self.root.join("lineage").join(session)).ok()?;
        let root = text.trim();
        is_uuid(root).then(|| root.to_string())
    }

    pub fn write_host(&self, host: HostId, record: &HostRecord) -> io::Result<()> {
        check_uuid("current session", &record.current_session)?;
        let dir = self.root.join("hosts");
        ensure_dir(&dir)?;
        let bytes = serde_json::to_vec(record)?;
        replace_file(&dir, &format!("{}.json", host.key()), &bytes)
    }

    pub fn read_host(&self, host: HostId) -> Option<HostRecord> {
        let path = self.root.join("hosts").join(format!("{}.json", host.key()));
        let record: HostRecord = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        is_uuid(&record.current_session).then_some(record)
    }

    pub fn remove_host(&self, host: HostId) -> io::Result<()> {
        let path = self.root.join("hosts").join(format!("{}.json", host.key()));
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn hosts(&self) -> Vec<(HostId, HostRecord)> {
        let mut hosts: Vec<_> = list_names(&self.root.join("hosts"))
            .into_iter()
            .filter_map(|name| {
                let host = HostId::parse(name.strip_suffix(".json")?)?;
                Some((host, self.read_host(host)?))
            })
            .collect();
        hosts.sort_by_key(|(host, _)| (host.pid, host.start));
        hosts
    }

    pub fn try_lock(&self, kind: LockKind, agent: &str) -> io::Result<Option<FileLock>> {
        check_uuid("agent id", agent)?;
        let dir = self.root.join("locks");
        ensure_dir(&dir)?;
        let suffix = match kind {
            LockKind::Waiter => "waiter",
            LockKind::Wake => "wake",
            LockKind::Helper => "helper",
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join(format!("{agent}.{suffix}")))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(FileLock { _file: file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Mailbox {
    dir: PathBuf,
}

impl Mailbox {
    fn state_dir(&self, state: State) -> PathBuf {
        self.dir.join(state.dir())
    }

    fn path(&self, state: State, id: &str) -> PathBuf {
        self.state_dir(state).join(format!("{id}.json"))
    }

    fn inflight_path(&self, inflight: &Inflight) -> PathBuf {
        self.state_dir(State::Inflight).join(inflight.file_name())
    }

    fn claim_path(&self, inflight: &Inflight) -> PathBuf {
        self.state_dir(State::Inflight)
            .join(format!("{}.claim", inflight.file_name()))
    }

    /// Writes the message into `state` via `tmp/`, so readers never see a
    /// half-written file.
    fn publish(&self, state: State, msg: &Message) -> io::Result<()> {
        msg.validate()?;
        let tmp = self.dir.join("tmp");
        ensure_dir(&tmp)?;
        ensure_dir(&self.state_dir(state))?;
        let staged = tmp.join(format!("{}.{}.json", msg.id, new_nonce()));
        write_new(&staged, &serde_json::to_vec(msg)?)?;
        fs::rename(&staged, self.path(state, &msg.id))
    }

    pub fn enqueue(&self, msg: &Message) -> io::Result<()> {
        self.publish(State::Pending, msg)
    }

    pub fn record_pasted(&self, msg: &Message, now: u64) -> io::Result<()> {
        let mut msg = msg.clone();
        msg.delivered_at_ms = Some(now);
        msg.delivered_via = Some(DeliveredVia::LegacyPaste);
        self.publish(State::Pasted, &msg)
    }

    /// Message ids in `state`, oldest first. Not meaningful for `Inflight`.
    pub fn ids(&self, state: State) -> Vec<String> {
        let mut ids: Vec<String> = list_names(&self.state_dir(state))
            .into_iter()
            .filter_map(|name| {
                let id = name.strip_suffix(".json")?;
                is_uuid(id).then(|| id.to_string())
            })
            .collect();
        ids.sort();
        ids
    }

    pub fn read(&self, state: State, id: &str) -> io::Result<Message> {
        check_uuid("message id", id)?;
        read_message(&self.path(state, id))
    }

    pub fn inflight(&self) -> Vec<Inflight> {
        let mut claims: Vec<Inflight> = list_names(&self.state_dir(State::Inflight))
            .iter()
            .filter_map(|name| Inflight::parse(name))
            .collect();
        claims.sort_by(|a, b| a.id.cmp(&b.id));
        claims
    }

    pub fn read_inflight(&self, inflight: &Inflight) -> io::Result<Message> {
        read_message(&self.inflight_path(inflight))
    }

    pub fn read_claim(&self, inflight: &Inflight) -> Option<Claim> {
        serde_json::from_slice(&fs::read(self.claim_path(inflight)).ok()?).ok()
    }

    pub fn write_claim(&self, inflight: &Inflight, claim: &Claim) -> io::Result<()> {
        let bytes = serde_json::to_vec(claim)?;
        replace_file(
            &self.state_dir(State::Inflight),
            &format!("{}.claim", inflight.file_name()),
            &bytes,
        )
    }

    /// `Ok(None)` means another process claimed the message first.
    pub fn claim(
        &self,
        id: &str,
        session: &str,
        host: HostId,
        claim: &Claim,
    ) -> io::Result<Option<Inflight>> {
        check_uuid("message id", id)?;
        check_uuid("session id", session)?;
        ensure_dir(&self.state_dir(State::Inflight))?;
        let inflight = Inflight {
            id: id.to_string(),
            session: session.to_string(),
            host,
        };
        match fs::rename(self.path(State::Pending, id), self.inflight_path(&inflight)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
        self.write_claim(&inflight, claim)?;
        Ok(Some(inflight))
    }

    /// The destination is written before the claim is removed: a crash in
    /// between leaves a duplicate, never a loss.
    pub fn commit(
        &self,
        inflight: &Inflight,
        to: State,
        via: DeliveredVia,
        now: u64,
    ) -> io::Result<()> {
        let mut msg = self.read_inflight(inflight)?;
        msg.delivered_at_ms = Some(now);
        msg.delivered_via = Some(via);
        self.publish(to, &msg)?;
        self.drop_claim(inflight)
    }

    pub fn revert(&self, inflight: &Inflight) -> io::Result<()> {
        let mut msg = self.read_inflight(inflight)?;
        msg.attempt += 1;
        self.publish(State::Pending, &msg)?;
        self.drop_claim(inflight)
    }

    /// `Ok(false)` means the message wasn't in `from`, typically because
    /// another process moved it first.
    pub fn transition(
        &self,
        id: &str,
        from: State,
        to: State,
        delivered: Option<(DeliveredVia, u64)>,
    ) -> io::Result<bool> {
        check_uuid("message id", id)?;
        let mut msg = match read_message(&self.path(from, id)) {
            Ok(msg) => msg,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if let Some((via, at)) = delivered {
            msg.delivered_via = Some(via);
            msg.delivered_at_ms = Some(at);
        }
        self.publish(to, &msg)?;
        match fs::remove_file(self.path(from, id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(true),
        }
    }

    fn drop_claim(&self, inflight: &Inflight) -> io::Result<()> {
        for path in [self.inflight_path(inflight), self.claim_path(inflight)] {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

fn read_message(path: &Path) -> io::Result<Message> {
    let msg: Message = serde_json::from_slice(&fs::read(path)?)?;
    msg.validate()?;
    Ok(msg)
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn replace_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let staged = dir.join(format!(".{name}.{}", new_nonce()));
    write_new(&staged, bytes)?;
    fs::rename(&staged, dir.join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })
}

fn list_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    /// Opaque; only compared for equality to detect pid reuse.
    pub start: u64,
    pub comm: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Unknown,
}

#[cfg(target_os = "macos")]
pub fn proc_info(pid: u32) -> Result<ProcInfo, Liveness> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if n != size {
        return Err(
            if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                Liveness::Dead
            } else {
                Liveness::Unknown
            },
        );
    }
    let name = if info.pbi_name[0] != 0 {
        &info.pbi_name[..]
    } else {
        &info.pbi_comm[..]
    };
    let bytes: Vec<u8> = name
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Ok(ProcInfo {
        ppid: info.pbi_ppid,
        start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        comm: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(target_os = "linux")]
pub fn proc_info(pid: u32) -> Result<ProcInfo, Liveness> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Liveness::Dead),
        Err(_) => return Err(Liveness::Unknown),
    };
    parse_proc_stat(&stat).ok_or(Liveness::Unknown)
}

/// `/proc/<pid>/stat`: the command name is parenthesised and may itself
/// contain spaces or parentheses, so fields are counted from the last `)`.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(stat: &str) -> Option<ProcInfo> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    Some(ProcInfo {
        ppid: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
        comm,
    })
}

pub fn liveness(host: HostId) -> Liveness {
    match proc_info(host.pid) {
        Ok(info) if info.start == host.start => Liveness::Alive,
        Ok(_) => Liveness::Dead,
        Err(state) => state,
    }
}

/// One message as it will be shown to the recipient.
pub struct RenderItem<'a> {
    pub msg: &'a Message,
    /// The claim's header nonce; absent for legacy fallback paste.
    pub nonce: Option<&'a str>,
    pub sent_before_clear: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Full,
    /// Only the head went out; the message moves to `notified/`.
    Head,
    /// Didn't fit this delivery; stays pending.
    Deferred,
}

pub fn render_batch(items: &[RenderItem], budget: usize) -> (String, Vec<Fit>) {
    render_batch_with(items, budget, &mut new_nonce)
}

fn render_batch_with(
    items: &[RenderItem],
    budget: usize,
    fence_nonce: &mut dyn FnMut() -> String,
) -> (String, Vec<Fit>) {
    const SEPARATOR: &str = "\n\n";
    let mut out = String::new();
    let mut used = 0;
    let mut fits = Vec::with_capacity(items.len());
    for item in items {
        if fits.contains(&Fit::Deferred) {
            fits.push(Fit::Deferred);
            continue;
        }
        let sep = if out.is_empty() { 0 } else { SEPARATOR.len() };
        let nonce = fence_nonce();
        let full = render_one(item, &nonce, false);
        let head = || render_one(item, &nonce, true);
        let (text, fit) = if used + sep + full.chars().count() <= budget {
            (full, Fit::Full)
        } else {
            let head = head();
            if out.is_empty() || used + sep + head.chars().count() <= budget {
                (head, Fit::Head)
            } else {
                fits.push(Fit::Deferred);
                continue;
            }
        };
        if sep > 0 {
            out.push_str(SEPARATOR);
        }
        used += sep + text.chars().count();
        out.push_str(&text);
        fits.push(fit);
    }
    (out, fits)
}

fn render_one(item: &RenderItem, fence: &str, head_only: bool) -> String {
    let msg = item.msg;
    let from = short_id(&msg.from.agent);
    let provider = match msg.from.provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let mut marker = format!("id={}", msg.id);
    if let Some(nonce) = item.nonce {
        marker.push_str(&format!(" n={nonce}"));
    }
    if msg.attempt > 0 {
        marker.push_str(&format!(
            "; re-delivery attempt {}, ignore if already handled",
            msg.attempt
        ));
    }
    if item.sent_before_clear {
        marker.push_str("; sent before this session was cleared");
    }
    let total_chars = msg.body.chars().count();
    let body = if head_only {
        escape_body(
            &msg.body
                .chars()
                .take(OVERFLOW_HEAD_CHARS)
                .collect::<String>(),
        )
    } else {
        escape_body(&msg.body)
    };
    let footer = if head_only {
        format!("… ({total_chars} chars) run: triage inbox show {}", msg.id)
    } else {
        format!("Reply: triage send --to {from} --message \"…\"")
    };
    format!(
        "{prefix}{label} ({provider} · agent {from}), delivered by triage. [{marker}]\n\
         <<peer-msg nonce={fence}>>\n{body}\n<</peer-msg nonce={fence}>>\n{footer}",
        prefix = crate::agent_comm::PEER_MESSAGE_PREFIX,
        label = display_label(&msg.from.label),
    )
}

pub fn display_label(label: &str) -> &str {
    let valid = !label.is_empty()
        && label.chars().count() <= MAX_LABEL_CHARS
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid { label } else { "agent" }
}

const FENCE_TAGS: [&str; 4] = [
    "<<peer-msg",
    "<</peer-msg",
    "<system-reminder",
    "</system-reminder",
];
const QUOTED_LINE_STARTS: [&str; 2] = ["📨 peer message", "reply: triage send"];

/// Neutralises anything in a body that could pass for renderer structure or a
/// harness tag, matching case- and whitespace-insensitively.
pub fn escape_body(body: &str) -> String {
    let stripped: String = body
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect();
    let mut chars: Vec<char> = stripped.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some(end) = FENCE_TAGS
                .iter()
                .find_map(|tag| match_loose(&chars, i, tag))
        {
            for c in &mut chars[i..end] {
                *c = match *c {
                    '<' => '‹',
                    '>' => '›',
                    other => other,
                };
            }
            i = end;
        } else {
            i += 1;
        }
    }
    let neutralised: String = chars.into_iter().collect();
    neutralised
        .split('\n')
        .map(|line| {
            let normalised = line
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            if QUOTED_LINE_STARTS.iter().any(|s| normalised.starts_with(s)) {
                format!("(quoted) {line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// End index of `pattern` matched at `start`, ignoring case and allowing
/// whitespace between any two pattern characters.
fn match_loose(chars: &[char], start: usize, pattern: &str) -> Option<usize> {
    let mut i = start;
    for (n, p) in pattern.chars().enumerate() {
        if n > 0 {
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
        }
        if !chars.get(i)?.to_lowercase().eq(p.to_lowercase()) {
            return None;
        }
        i += 1;
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const ALICE: &str = "00000000-0000-4000-8000-00000000a11c";
    const BOB: &str = "00000000-0000-4000-8000-000000000b0b";
    const SESSION: &str = "00000000-0000-4000-8000-00000000c1a0";

    fn temp_store(name: &str) -> Store {
        let root = std::env::temp_dir().join(format!(
            "triage-mailbox-{name}-{}-{}",
            std::process::id(),
            new_nonce()
        ));
        Store::at(root)
    }

    fn message(id: &str, body: &str) -> Message {
        Message {
            v: 1,
            id: id.to_string(),
            created_at_ms: 1_790_000_000_000,
            from: Sender {
                agent: ALICE.to_string(),
                session: ALICE.to_string(),
                provider: Provider::Claude,
                label: "TRI-148".to_string(),
            },
            to: Recipient {
                agent: BOB.to_string(),
                session_at_send: BOB.to_string(),
            },
            body: body.to_string(),
            attempt: 0,
            bounce_of: None,
            delivered_at_ms: None,
            delivered_via: None,
        }
    }

    fn claim_record() -> Claim {
        Claim {
            nonce: "0123456789abcdef".to_string(),
            claimed_at_ms: 1,
            waiter_pid: 1,
            waiter_start: 1,
            transcript_path: String::new(),
            transcript_offset: 0,
            printed_at_ms: None,
        }
    }

    const HOST: HostId = HostId { pid: 42, start: 7 };

    #[test]
    fn message_ids_are_v7_and_sort_in_creation_order() {
        let ids: Vec<String> = (0..20_000).map(|_| new_message_id()).collect();
        assert!(
            ids.iter()
                .all(|id| is_uuid(id) && id.as_bytes()[14] == b'7')
        );
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, ids);
    }

    #[test]
    fn id_validation_rejects_anything_that_could_escape_a_path() {
        for bad in [
            "../../../../../../../../../etc/passwd",
            "00000000-0000-4000-8000-00000000A11C",
            "00000000-0000-4000-8000-00000000a11c/",
            "00000000/0000-4000-8000-00000000a11c",
            "00000000-0000-4000-8000-00000000a11",
        ] {
            assert!(!is_uuid(bad), "{bad}");
        }
        assert!(is_uuid(ALICE));
        assert!(Store::at("/nonexistent").mailbox("../../etc").is_err());
    }

    #[test]
    fn concurrent_claims_have_exactly_one_winner() {
        let store = temp_store("claims");
        let mailbox = store.mailbox(BOB).unwrap();
        let id = new_message_id();
        mailbox.enqueue(&message(&id, "hi")).unwrap();
        let barrier = Arc::new(Barrier::new(16));
        let winners: Vec<bool> = (0..16)
            .map(|n| {
                let (mailbox, id, barrier) = (mailbox.clone(), id.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let host = HostId { pid: n, start: 1 };
                    mailbox
                        .claim(&id, SESSION, host, &claim_record())
                        .unwrap()
                        .is_some()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(winners.iter().filter(|&&w| w).count(), 1);
        assert!(mailbox.ids(State::Pending).is_empty());
        assert_eq!(mailbox.inflight().len(), 1);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn concurrent_lineage_links_agree_on_one_root() {
        let store = temp_store("lineage");
        let roots: Vec<String> = (0..16).map(|_| new_message_id()).collect();
        let barrier = Arc::new(Barrier::new(roots.len()));
        let results: Vec<String> = roots
            .iter()
            .cloned()
            .map(|root| {
                let (store, barrier) = (store.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    store.link_lineage(SESSION, &root).unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        let stored = store.lineage_root(SESSION).unwrap();
        assert!(results.iter().all(|r| *r == stored));
        assert!(roots.contains(&stored));
        assert_eq!(list_names(&store.root().join("lineage")), vec![SESSION]);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn commit_records_delivery_and_clears_the_claim() {
        let store = temp_store("commit");
        let mailbox = store.mailbox(BOB).unwrap();
        let id = new_message_id();
        mailbox.enqueue(&message(&id, "hi")).unwrap();
        let inflight = mailbox
            .claim(&id, SESSION, HOST, &claim_record())
            .unwrap()
            .unwrap();
        assert_eq!(mailbox.read_claim(&inflight), Some(claim_record()));
        mailbox
            .commit(&inflight, State::Delivered, DeliveredVia::ClaudeWaiter, 99)
            .unwrap();
        let delivered = mailbox.read(State::Delivered, &id).unwrap();
        assert_eq!(delivered.delivered_via, Some(DeliveredVia::ClaudeWaiter));
        assert_eq!(delivered.delivered_at_ms, Some(99));
        assert!(list_names(&mailbox.state_dir(State::Inflight)).is_empty());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn revert_returns_mail_to_pending_as_a_labelled_redelivery() {
        let store = temp_store("revert");
        let mailbox = store.mailbox(BOB).unwrap();
        let id = new_message_id();
        mailbox.enqueue(&message(&id, "hi")).unwrap();
        let inflight = mailbox
            .claim(&id, SESSION, HOST, &claim_record())
            .unwrap()
            .unwrap();
        mailbox.revert(&inflight).unwrap();
        assert_eq!(mailbox.ids(State::Pending), vec![id.clone()]);
        assert_eq!(mailbox.read(State::Pending, &id).unwrap().attempt, 1);
        assert!(list_names(&mailbox.state_dir(State::Inflight)).is_empty());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn inflight_names_round_trip_and_reject_unsafe_ids() {
        let inflight = Inflight {
            id: new_message_id(),
            session: SESSION.to_string(),
            host: HOST,
        };
        assert_eq!(Inflight::parse(&inflight.file_name()), Some(inflight));
        assert_eq!(Inflight::parse(&format!("..@{SESSION}@1@2.json")), None);
        assert_eq!(
            Inflight::parse(&format!("{ALICE}@{SESSION}@1@2@3.json")),
            None
        );
    }

    #[test]
    fn host_records_with_an_invalid_session_are_ignored() {
        let store = temp_store("hosts");
        let record = HostRecord {
            v: 1,
            provider: Provider::Codex,
            hook_version: "v1".to_string(),
            current_session: SESSION.to_string(),
            updated_at_ms: 5,
        };
        store.write_host(HOST, &record).unwrap();
        assert_eq!(store.hosts(), vec![(HOST, record)]);
        fs::write(
            store.root().join("hosts/42-7.json"),
            r#"{"v":1,"provider":"codex","hook_version":"v1","current_session":"../x","updated_at_ms":5}"#,
        )
        .unwrap();
        assert_eq!(store.read_host(HOST), None);
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn lock_excludes_a_second_holder_until_dropped() {
        let store = temp_store("lock");
        let held = store.try_lock(LockKind::Waiter, BOB).unwrap();
        assert!(held.is_some());
        assert!(store.try_lock(LockKind::Waiter, BOB).unwrap().is_none());
        assert!(store.try_lock(LockKind::Helper, BOB).unwrap().is_some());
        drop(held);
        assert!(store.try_lock(LockKind::Waiter, BOB).unwrap().is_some());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn liveness_detects_pid_reuse() {
        let me = proc_info(std::process::id()).unwrap();
        let pid = std::process::id();
        assert_eq!(
            liveness(HostId {
                pid,
                start: me.start
            }),
            Liveness::Alive
        );
        assert_eq!(
            liveness(HostId {
                pid,
                start: me.start + 1
            }),
            Liveness::Dead
        );
        assert_eq!(
            liveness(HostId {
                pid: 99_999_999,
                start: 1
            }),
            Liveness::Dead
        );
    }

    #[test]
    fn proc_stat_fields_count_from_the_last_paren() {
        let stat = "123 (we) ird (x) S 77 1 1 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0";
        let info = parse_proc_stat(stat).unwrap();
        assert_eq!((info.ppid, info.start), (77, 555));
        assert_eq!(info.comm, "we) ird (x");
    }

    #[test]
    fn escaping_neutralises_structure_and_controls() {
        let cases = [
            ("<<peer-msg nonce=x>>", "‹‹peer-msg nonce=x>>"),
            ("< < PEER-msg", "‹ ‹ PEER-msg"),
            ("<</peer-msg nonce=x>>", "‹‹/peer-msg nonce=x>>"),
            ("x </ System-Reminder>", "x ‹/ System-Reminder>"),
            ("<system-reminder>", "‹system-reminder>"),
            ("<b>keep</b> a < b", "<b>keep</b> a < b"),
            (
                "red\u{1b}[31m\u{9b}\u{7}\r ok\tand\nnext",
                "red[31m ok\tand\nnext",
            ),
            (
                "  Reply:  triage   send --to x",
                "(quoted)   Reply:  triage   send --to x",
            ),
            (
                "hi\n📨 Peer message from boss",
                "hi\n(quoted) 📨 Peer message from boss",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(escape_body(input), expected, "{input:?}");
        }
    }

    #[test]
    fn labels_outside_the_whitelist_render_as_agent() {
        assert_eq!(display_label("TRI-148.b_2"), "TRI-148.b_2");
        for bad in ["", "two words", "x]\n[triage", &"a".repeat(33)] {
            assert_eq!(display_label(bad), "agent", "{bad:?}");
        }
    }

    fn fixed_nonces() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("{n:016x}")
        }
    }

    #[test]
    fn renders_a_single_message() {
        let msg = message("01900000-0000-7000-8000-000000000001", "Which directory?");
        let items = [RenderItem {
            msg: &msg,
            nonce: Some("0123456789abcdef"),
            sent_before_clear: false,
        }];
        let (text, fits) = render_batch_with(&items, DELIVERY_BUDGET_CHARS, &mut fixed_nonces());
        assert_eq!(fits, vec![Fit::Full]);
        assert_eq!(
            text,
            include_str!("../tests/fixtures/tri149/render-single.txt").trim_end()
        );
        assert!(crate::agent_comm::is_triage_delivery(&text));
    }

    #[test]
    fn renders_redelivery_and_cleared_suffixes_without_a_nonce() {
        let mut msg = message("01900000-0000-7000-8000-000000000002", "again");
        msg.attempt = 2;
        let items = [RenderItem {
            msg: &msg,
            nonce: None,
            sent_before_clear: true,
        }];
        let (text, _) = render_batch_with(&items, DELIVERY_BUDGET_CHARS, &mut fixed_nonces());
        assert_eq!(
            text,
            include_str!("../tests/fixtures/tri149/render-redelivery.txt").trim_end()
        );
    }

    #[test]
    fn oversized_message_sends_its_head_and_later_mail_still_fits() {
        let first = message("01900000-0000-7000-8000-000000000003", "short");
        let big = message("01900000-0000-7000-8000-000000000004", &"x".repeat(6_000));
        let last = message("01900000-0000-7000-8000-000000000005", "after");
        let items: Vec<RenderItem> = [&first, &big, &last]
            .into_iter()
            .map(|msg| RenderItem {
                msg,
                nonce: Some("0123456789abcdef"),
                sent_before_clear: false,
            })
            .collect();
        let (text, fits) = render_batch_with(&items, DELIVERY_BUDGET_CHARS, &mut fixed_nonces());
        assert_eq!(fits, vec![Fit::Full, Fit::Head, Fit::Full]);
        assert!(text.chars().count() <= DELIVERY_BUDGET_CHARS);
        assert_eq!(
            text,
            include_str!("../tests/fixtures/tri149/render-overflow.txt").trim_end()
        );
    }

    #[test]
    fn once_a_message_is_deferred_everything_after_it_waits() {
        let msgs: Vec<Message> = (0..8)
            .map(|n| {
                message(
                    &format!("01900000-0000-7000-8000-00000000001{n}"),
                    &"y".repeat(900),
                )
            })
            .collect();
        let mut items: Vec<RenderItem> = msgs
            .iter()
            .map(|msg| RenderItem {
                msg,
                nonce: Some("0123456789abcdef"),
                sent_before_clear: false,
            })
            .collect();
        let (text, fits) = render_batch_with(&items, DELIVERY_BUDGET_CHARS, &mut fixed_nonces());
        let first_deferred = fits.iter().position(|f| *f == Fit::Deferred).unwrap();
        assert!(first_deferred > 0);
        assert!(fits[first_deferred..].iter().all(|f| *f == Fit::Deferred));
        assert!(text.chars().count() <= DELIVERY_BUDGET_CHARS);

        items.truncate(1);
        let (_, fits) = render_batch_with(&items, 10, &mut fixed_nonces());
        assert_eq!(fits, vec![Fit::Head]);
    }
}
