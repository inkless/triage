use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const S1: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const S2: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const SENDER: &str = "bbbbbbbb-0000-4000-8000-000000000001";

struct Env {
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Env {
    /// The test process stands in for the Claude host: hooks find it as the
    /// ancestor with a `~/.claude/sessions/<pid>.json` record.
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("triage-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".claude/sessions")).unwrap();
        fs::write(
            dir.join(format!(".claude/sessions/{}.json", std::process::id())),
            "{}",
        )
        .unwrap();
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
            .env("PATH", self.dir.join("bin"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn spawn_hook(&self, event: &str, wait: bool, session: &str) -> Child {
        let mut args = vec!["inbox", "--hook", "claude", event];
        if wait {
            args.push("--wait");
        }
        args.push("--triage-hook=v1");
        let mut child = self.command(&args).spawn().unwrap();
        let payload = serde_json::json!({
            "session_id": session,
            "transcript_path": self.transcript(session),
            "hook_event_name": event,
        });
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child
    }

    fn hook(&self, event: &str, wait: bool, session: &str) -> Output {
        finish(
            self.spawn_hook(event, wait, session),
            Duration::from_secs(10),
        )
    }

    fn transcript(&self, session: &str) -> PathBuf {
        self.dir.join(format!("{session}.jsonl"))
    }

    fn write_mail(&self, agent: &str, id: &str, body: &str) {
        let path = self.state().join(format!("mail/{agent}/pending/{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let msg = serde_json::json!({
            "v": 1, "id": id, "created_at_ms": 1,
            "from": {"agent": SENDER, "session": SENDER, "provider": "codex", "label": "TRI-148"},
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
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| serde_json::from_slice(&fs::read(p).unwrap()).unwrap())
            .collect()
    }
}

fn finish(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("hook did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

fn rewake_record(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": format!(
            "<task-notification>\n<summary>Stop hook feedback</summary>\n</task-notification>\n<system-reminder>\nStop hook blocking error from command \"Stop\": {text}\n</system-reminder>"
        )},
    })
    .to_string()
        + "\n"
}

fn append(path: &Path, text: &str) {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn waiter_wakes_with_rendered_mail_and_commits_once_the_transcript_shows_it() {
    let env = Env::new("waiter");
    let id = "01900000-0000-7000-8000-000000000001";
    env.write_mail(S1, id, "please rebase");

    let woke = env.hook("stop", true, S1);
    assert_eq!(woke.status.code(), Some(2));
    let text = String::from_utf8(woke.stderr).unwrap();
    assert!(text.starts_with("📨 Peer message from TRI-148"), "{text}");
    assert!(text.contains("please rebase"), "{text}");
    let nonce = text
        .split(&format!("[id={id} n="))
        .nth(1)
        .and_then(|rest| rest.get(..16))
        .expect("header carries the claim nonce")
        .to_string();
    assert!(woke.stdout.is_empty());
    assert_eq!(env.mail(S1, "inflight").len(), 1);

    append(
        &env.transcript(S1),
        &rewake_record(&text.replace(&nonce, "ffffffffffffffff")),
    );
    assert_eq!(env.hook("stop", false, S1).status.code(), Some(0));
    assert_eq!(
        env.mail(S1, "inflight").len(),
        1,
        "a wrong nonce must not confirm"
    );

    append(&env.transcript(S1), &rewake_record(&text));
    assert_eq!(env.hook("stop", false, S1).status.code(), Some(0));
    let delivered = env.mail(S1, "delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["delivered_via"], "claude-waiter");
    assert!(env.mail(S1, "inflight").is_empty());
}

#[test]
fn one_waiter_per_agent_and_a_cleared_session_hands_the_lock_over() {
    let env = Env::new("handoff");
    let first = env.spawn_hook("stop", true, S1);
    std::thread::sleep(Duration::from_millis(1500));

    let second = env.hook("stop", true, S1);
    assert_eq!(
        second.status.code(),
        Some(0),
        "the current session already has a waiter"
    );

    let cleared = env.spawn_hook("session-start", true, S2);
    let first = finish(first, Duration::from_secs(5));
    assert_eq!(first.status.code(), Some(0));
    assert!(first.stderr.is_empty());

    std::thread::sleep(Duration::from_millis(1500));
    env.write_mail(S1, "01900000-0000-7000-8000-000000000002", "after clear");
    let woke = finish(cleared, Duration::from_secs(10));
    assert_eq!(woke.status.code(), Some(2));
    let text = String::from_utf8(woke.stderr).unwrap();
    assert!(text.contains("after clear"), "{text}");
    assert!(
        text.contains("sent before this session was cleared"),
        "{text}"
    );
}

#[test]
fn drain_answers_only_for_the_current_session() {
    let env = Env::new("drain");
    assert_eq!(env.hook("session-start", false, S1).status.code(), Some(0));
    env.write_mail(S1, "01900000-0000-7000-8000-000000000003", "mid-turn note");

    let stale = env.hook("post-tool-use", false, S2);
    assert_eq!(stale.status.code(), Some(0));
    assert!(stale.stdout.is_empty());
    assert_eq!(env.mail(S1, "pending").len(), 1);

    let drained = env.hook("post-tool-use", false, S1);
    assert_eq!(drained.status.code(), Some(0));
    assert!(drained.stderr.is_empty());
    let output: serde_json::Value = serde_json::from_slice(&drained.stdout).unwrap();
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    assert!(
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("mid-turn note")
    );
    let delivered = env.mail(S1, "delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["delivered_via"], "claude-post-tool-use");
}

#[test]
fn a_bad_payload_exits_silently_and_is_logged() {
    let env = Env::new("badpayload");
    let mut child = env
        .command(&[
            "inbox",
            "--hook",
            "claude",
            "stop",
            "--wait",
            "--triage-hook=v1",
        ])
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"../../etc"}"#)
        .unwrap();
    let out = finish(child, Duration::from_secs(5));
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty() && out.stdout.is_empty());
    assert!(
        fs::read_to_string(env.state().join("hook.log"))
            .unwrap()
            .contains("session_id")
    );
}

#[test]
fn hooks_install_uses_the_path_binary_and_is_idempotent() {
    let env = Env::new("install");
    let bin = env.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_triage"), bin.join("triage")).unwrap();
    let settings = env.dir.join(".claude/settings.json");
    fs::write(
        &settings,
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"navi-stop.sh"}]}]}}"#,
    )
    .unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
    let run = |args: &[&str]| finish(env.command(args).spawn().unwrap(), Duration::from_secs(10));

    let first = run(&["hooks", "install"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "navi-stop.sh");
    assert_eq!(
        v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        format!(
            "{} inbox --hook claude session-start --wait --triage-hook=v1",
            bin.join("triage").display()
        )
    );

    let again = run(&["hooks", "install"]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("already up to date"));
    let status = String::from_utf8(run(&["hooks", "status"]).stdout).unwrap();
    assert!(status.contains("Claude Stop mail waiter"), "{status}");
    assert!(
        !status.contains("missing") && !status.contains("stale"),
        "{status}"
    );

    assert!(run(&["hooks", "uninstall"]).status.success());
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"navi-stop.sh"}]}]}})
    );
    assert_eq!(
        fs::metadata(&settings).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
