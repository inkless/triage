use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const S1: &str = "019a8f3c-0000-7000-8000-0000000000b1";
const S2: &str = "019a8f3c-0000-7000-8000-0000000000b2";
const SENDER: &str = "bbbbbbbb-0000-4000-8000-000000000001";

struct Env {
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The test process stands in for the pi process (`TRIAGE_TEST_PANE_HARNESS`).
impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("triage-pi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".pi/agent")).unwrap();
        Self { dir }
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state/triage")
    }

    /// pi records a session once it has a message, under the pi process's
    /// directory; the test process is the pi process here.
    fn pi_session(&self, id: &str) {
        let cwd = std::env::current_dir().unwrap();
        let encoded = cwd
            .to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', '\\', ':'], "-");
        let dir = self.dir.join(format!(".pi/agent/sessions/--{encoded}--"));
        fs::create_dir_all(&dir).unwrap();
        let header = serde_json::json!({"type": "session", "id": id, "cwd": cwd});
        fs::write(dir.join(format!("{id}.jsonl")), format!("{header}\n")).unwrap();
    }

    fn host_records(&self) -> Vec<serde_json::Value> {
        let Ok(entries) = fs::read_dir(self.state().join("hosts")) else {
            return Vec::new();
        };
        entries
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
            .collect()
    }

    fn extension(&self) -> PathBuf {
        self.dir.join(".pi/agent/extensions/triage-mail.ts")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_triage"));
        cmd.args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env_remove("PI_CODING_AGENT_DIR")
            .env("TRIAGE_TEST_PANE_HARNESS", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn spawn_hook(&self, event: &str, session: &str) -> Child {
        let mut child = self
            .command(&["inbox", "--hook", "pi", event, "--triage-hook=v1"])
            .spawn()
            .unwrap();
        let payload = serde_json::json!({ "session_id": session });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child
    }

    fn hook(&self, event: &str, session: &str) -> Output {
        self.spawn_hook(event, session).wait_with_output().unwrap()
    }

    fn ack(&self, session: &str, ack: &serde_json::Value) -> Output {
        let mut child = self
            .command(&["inbox", "--hook", "pi", "ack", "--triage-hook=v1"])
            .spawn()
            .unwrap();
        let payload = serde_json::json!({ "session_id": session, "ack": ack });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn hooks(&self, args: &[&str]) -> Output {
        self.command(&[&["hooks"][..], args].concat())
            .output()
            .unwrap()
    }

    fn write_mail(&self, agent: &str, id: &str, body: &str) {
        let path = self.state().join(format!("mail/{agent}/pending/{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let msg = serde_json::json!({
            "v": 1, "id": id, "created_at_ms": 1,
            "from": {"agent": SENDER, "session": SENDER, "provider": "claude", "label": "lead"},
            "to": {"agent": agent, "session_at_send": agent},
            "body": body, "attempt": 0, "bounce_of": null
        });
        fs::write(path, msg.to_string()).unwrap();
    }

    fn mail(&self, agent: &str, state: &str) -> Vec<serde_json::Value> {
        let Ok(entries) = fs::read_dir(self.state().join(format!("mail/{agent}/{state}"))) else {
            return Vec::new();
        };
        entries
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect()
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn finish_within(mut child: Child, limit: Duration) -> Output {
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > limit {
            child.kill().unwrap();
            panic!("hook still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn session_start_makes_the_pi_process_the_host_of_its_session() {
    let env = Env::new("start");
    env.pi_session(S1);
    env.pi_session(S2);
    let out = env.hook("session-start", S1);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    let hosts: Vec<_> = fs::read_dir(env.state().join("hosts"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(hosts.len(), 1, "{hosts:?}");
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&hosts[0]).unwrap()).unwrap();
    assert_eq!(record["provider"], "pi");
    assert_eq!(record["current_session"], S1);

    // A new session in the same pi process replaces the old one.
    env.hook("session-start", S2);
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&hosts[0]).unwrap()).unwrap();
    assert_eq!(record["current_session"], S2);
}

#[test]
fn the_waiter_prints_mail_as_it_arrives_and_the_extension_confirms_it() {
    let env = Env::new("wait");
    env.pi_session(S1);
    env.hook("session-start", S1);
    let waiter = env.spawn_hook("wait", S1);
    std::thread::sleep(Duration::from_millis(300));
    env.write_mail(S1, "01900000-0000-7000-8000-0000000000d1", "please rebase");

    let out = finish_within(waiter, Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(0));
    let printed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = printed["text"].as_str().unwrap();
    assert!(text.starts_with("📨 Peer message from lead"), "{text}");
    assert!(text.contains("\nplease rebase\n"), "{text}");
    assert!(env.mail(S1, "pending").is_empty());
    // Printed is not delivered: the extension may never inject it.
    assert!(env.mail(S1, "delivered").is_empty());

    // An ack for another claim settles nothing.
    let mut wrong = printed["ack"].clone();
    wrong[0]["nonce"] = "0000000000000000".into();
    assert!(env.ack(S1, &wrong).status.success());
    assert!(env.mail(S1, "delivered").is_empty());

    assert!(env.ack(S1, &printed["ack"]).status.success());
    let delivered = env.mail(S1, "delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["delivered_via"], "pi-extension");
}

#[test]
fn a_waiter_for_a_session_the_process_left_exits_without_mail() {
    let env = Env::new("stale");
    env.pi_session(S1);
    env.pi_session(S2);
    env.hook("session-start", S1);
    env.hook("session-start", S2);
    env.write_mail(
        S1,
        "01900000-0000-7000-8000-0000000000d2",
        "for the old session",
    );
    let out = finish_within(env.spawn_hook("wait", S1), Duration::from_secs(5));
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert_eq!(env.mail(S1, "pending").len(), 1);
}

#[test]
fn install_writes_the_extension_once_and_uninstall_removes_only_ours() {
    let env = Env::new("install");
    let out = env.hooks(&["install", "--pi"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let source = fs::read_to_string(env.extension()).unwrap();
    let binary = serde_json::to_string(env!("CARGO_BIN_EXE_triage")).unwrap();
    assert!(
        source.contains(&format!("const TRIAGE = {binary};")),
        "{source}"
    );
    assert!(!source.contains("__TRIAGE__"));

    let again = env.hooks(&["install", "--pi"]);
    assert!(
        stdout(&again).contains("already up to date"),
        "{}",
        stdout(&again)
    );
    assert_eq!(fs::read_to_string(env.extension()).unwrap(), source);
    let status = env.hooks(&["status", "--pi"]);
    assert!(stdout(&status).contains("pi mail extension"));
    assert!(
        stdout(&status).contains(" installed"),
        "{}",
        stdout(&status)
    );

    let out = env.hooks(&["uninstall", "--pi"]);
    assert!(out.status.success());
    assert!(!env.extension().exists());

    fs::write(env.extension(), "export default function () {}\n").unwrap();
    let out = env.hooks(&["install", "--pi"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not written by triage"));
    env.hooks(&["uninstall", "--pi"]);
    assert_eq!(
        fs::read_to_string(env.extension()).unwrap(),
        "export default function () {}\n"
    );
}

#[test]
fn install_skips_pi_when_it_is_not_installed() {
    let env = Env::new("nopi");
    fs::remove_dir_all(env.dir.join(".pi")).unwrap();
    let out = env.hooks(&["install", "--pi"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("not found; skipping"),
        "{}",
        stdout(&out)
    );
    assert!(!env.extension().exists());
}

#[test]
fn session_start_for_a_session_pi_has_not_recorded_hosts_nothing() {
    let env = Env::new("unknown");
    env.pi_session(S1);
    env.hook("session-start", S1);
    assert_eq!(env.host_records()[0]["current_session"], S1);

    // A session with no file in the pi process's directory (forged, or new
    // and not yet written) replaces nothing: the process hosts nothing.
    let out = env.hook("session-start", S2);
    assert_eq!(out.status.code(), Some(0));
    assert!(env.host_records().is_empty(), "{:?}", env.host_records());
    env.write_mail(S2, "01900000-0000-7000-8000-0000000000d3", "not yet");
    let out = finish_within(env.spawn_hook("wait", S2), Duration::from_secs(5));
    assert!(out.stdout.is_empty());

    // Once pi has written the session, the waiter registers it and delivers.
    env.pi_session(S2);
    let out = finish_within(env.spawn_hook("wait", S2), Duration::from_secs(10));
    assert!(stdout(&out).contains("not yet"), "{}", stdout(&out));
    assert_eq!(env.host_records()[0]["current_session"], S2);
}

#[cfg(target_os = "linux")]
#[test]
fn session_start_for_a_session_another_pi_process_holds_is_refused() {
    let env = Env::new("foreign");
    env.pi_session(S1);
    env.pi_session(S2);
    env.hook("session-start", S1);
    // Another live process hosts S2.
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    let stat = fs::read_to_string(format!("/proc/{}/stat", other.id())).unwrap();
    let start = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    let record = serde_json::json!({"v": 1, "provider": "pi", "hook_version": "v1",
        "current_session": S2, "updated_at_ms": 1});
    fs::write(
        env.state()
            .join(format!("hosts/{}-{start}.json", other.id())),
        record.to_string(),
    )
    .unwrap();

    env.hook("session-start", S2);
    let records = env.host_records();
    let _ = other.kill();
    let _ = other.wait();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["current_session"], S2);
    assert!(
        !fs::read_dir(env.state().join("hosts")).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{}-", std::process::id()))),
        "the test process still hosts a session"
    );
}
