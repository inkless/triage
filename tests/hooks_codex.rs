use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const S1: &str = "01a0dbf1-0000-7000-8000-000000000001";
const S2: &str = "01a0dbf1-0000-7000-8000-000000000002";
const SENDER: &str = "bbbbbbbb-0000-4000-8000-000000000001";

struct Env {
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The test process stands in for the Codex host (`TRIAGE_TEST_CODEX_HOST`);
/// a fake `codex` on PATH records `codex queue` calls.
impl Env {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("triage-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::create_dir_all(dir.join(".codex")).unwrap();
        let fake = dir.join("bin/codex");
        fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/queue.log\"\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state/triage")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_triage"));
        cmd.args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.dir.join("bin").display()),
            )
            .env("TRIAGE_TEST_CODEX_HOST", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn hook(&self, event: &str, session: &str) -> Output {
        let mut child = self
            .command(&["inbox", "--hook", "codex", event, "--triage-hook=v1"])
            .spawn()
            .unwrap();
        let payload = serde_json::json!({"session_id": session, "hook_event_name": event});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn write_mail(&self, agent: &str, id: &str, body: &str) {
        let path = self.state().join(format!("mail/{agent}/pending/{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let msg = serde_json::json!({
            "v": 1, "id": id, "created_at_ms": 1,
            "from": {"agent": SENDER, "session": SENDER, "provider": "claude", "label": "TRI-148"},
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

    fn queue_calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("queue.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn pointer_marker(&self, agent: &str) -> PathBuf {
        self.state().join(format!("markers/{agent}.pointer"))
    }
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

#[test]
fn session_start_only_records_the_session() {
    let env = Env::new("session-start");
    env.write_mail(S1, "01900000-0000-7000-8000-000000000001", "hi");
    let out = env.hook("session-start", S1);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert_eq!(env.mail(S1, "pending").len(), 1);
    let host = fs::read_dir(env.state().join("hosts"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(host.path()).unwrap()).unwrap();
    assert_eq!(record["provider"], "codex");
    assert_eq!(record["current_session"], S1);
}

#[test]
fn prompt_drain_hands_mail_over_as_hidden_context_and_clears_the_pointer() {
    let env = Env::new("drain");
    env.hook("session-start", S1);
    env.write_mail(S1, "01900000-0000-7000-8000-000000000002", "please rebase");
    fs::create_dir_all(env.pointer_marker(S1).parent().unwrap()).unwrap();
    fs::write(env.pointer_marker(S1), "{}").unwrap();

    let out = env.hook("user-prompt-submit", S1);
    assert_eq!(out.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(
        json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("please rebase")
    );
    let delivered = env.mail(S1, "delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["delivered_via"], "codex-user-prompt-submit");
    assert!(!env.pointer_marker(S1).exists());
}

#[test]
fn helper_queues_one_pointer_per_burst() {
    let env = Env::new("helper");
    env.hook("session-start", S1);
    env.write_mail(S1, "01900000-0000-7000-8000-000000000003", "one");
    env.write_mail(S1, "01900000-0000-7000-8000-000000000004", "two");

    let run_helper = || {
        let out = env.command(&["inbox", "--helper", S1]).output().unwrap();
        assert_eq!(out.status.code(), Some(0));
    };
    run_helper();
    let calls = env.queue_calls();
    assert_eq!(
        calls,
        [format!(
            "queue --thread {S1} --message 📨 triage: 2 peer message(s) from 00000001. If nothing is attached, run: triage inbox"
        )]
    );
    assert!(env.pointer_marker(S1).exists());

    run_helper();
    assert_eq!(env.queue_calls().len(), 1, "a live pointer is not re-sent");
}

#[test]
fn a_prompt_on_a_stale_thread_requeues_the_pointer_to_the_current_one() {
    let env = Env::new("requeue");
    env.hook("session-start", S1);
    env.hook("session-start", S2);
    env.write_mail(
        S1,
        "01900000-0000-7000-8000-000000000005",
        "for the new thread",
    );
    fs::create_dir_all(env.pointer_marker(S1).parent().unwrap()).unwrap();
    fs::write(env.pointer_marker(S1), "{}").unwrap();

    let out = env.hook("user-prompt-submit", S1);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "a stale thread gets nothing");
    assert_eq!(env.mail(S1, "pending").len(), 1);
    assert!(wait_until(Duration::from_secs(10), || env
        .queue_calls()
        .iter()
        .any(|c| c.starts_with(&format!("queue --thread {S2} ")))));
}

#[test]
fn hooks_install_writes_codex_entries_once() {
    let env = Env::new("install");
    let settings = env.dir.join(".codex/hooks.json");
    fs::write(
        &settings,
        r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"user.sh"}]}]}}"#,
    )
    .unwrap();
    let run = |args: &[&str]| env.command(args).output().unwrap();
    assert!(run(&["hooks", "install", "--codex"]).status.success());
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    let ups = v["hooks"]["UserPromptSubmit"].as_array().unwrap();
    assert_eq!(ups[0]["hooks"][0]["command"], "user.sh");
    assert!(
        ups[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" inbox --hook codex user-prompt-submit --triage-hook=v1")
    );
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    let again = run(&["hooks", "install", "--codex"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("already up to date"));
}
