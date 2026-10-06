use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

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
        self.write_mail_at(agent, id, body, 1);
    }

    fn write_mail_at(&self, agent: &str, id: &str, body: &str, created_at_ms: u128) {
        let path = self.state().join(format!("mail/{agent}/pending/{id}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let msg = serde_json::json!({
            "v": 1, "id": id, "created_at_ms": created_at_ms,
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
fn mail_arriving_during_the_grace_period_is_counted_in_the_pointer() {
    let env = Env::new("grace");
    env.hook("session-start", S1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    env.write_mail_at(S1, "01900000-0000-7000-8000-000000000006", "first", now);
    let helper = env.command(&["inbox", "--helper", S1]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    env.write_mail_at(
        S1,
        "01900000-0000-7000-8000-000000000007",
        "second",
        now + 1000,
    );
    assert!(helper.wait_with_output().unwrap().status.success());
    let calls = env.queue_calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("2 peer message(s)"), "{calls:?}");
}

#[test]
fn threads_on_one_host_drain_only_their_own_mail() {
    let env = Env::new("shared-host");
    fs::create_dir_all(env.state().join("lineage")).unwrap();
    fs::write(env.state().join(format!("lineage/{S2}")), S1).unwrap();
    env.hook("session-start", S1);
    env.hook("session-start", S2);
    assert_eq!(
        fs::read_to_string(env.state().join(format!("lineage/{S2}"))).unwrap(),
        S2
    );
    env.write_mail(
        S1,
        "01900000-0000-7000-8000-000000000005",
        "first thread only",
    );
    env.write_mail(
        S2,
        "01900000-0000-7000-8000-000000000008",
        "second thread only",
    );
    let out = env.hook("user-prompt-submit", S1);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("first thread only"));
    assert!(!text.contains("second thread only"));
    assert!(env.mail(S1, "pending").is_empty());
    assert_eq!(env.mail(S2, "pending").len(), 1);
    let helper = env.command(&["inbox", "--helper", S2]).output().unwrap();
    assert!(helper.status.success());
    assert_eq!(env.queue_calls().len(), 1);
    assert!(env.queue_calls()[0].starts_with(&format!("queue --thread {S2} ")));
    let out = env.hook("post-tool-use", S2);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("second thread only"));
    assert!(!text.contains("first thread only"));
    assert!(env.mail(S2, "pending").is_empty());
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

#[test]
fn codex_hook_uses_the_environment_thread_only_when_payload_has_no_session() {
    let env = Env::new("environment-thread");
    let hook = |session: Option<&str>, thread: &str| {
        let mut child = env
            .command(&[
                "inbox",
                "--hook",
                "codex",
                "post-tool-use",
                "--triage-hook=v1",
            ])
            .env("CODEX_THREAD_ID", thread)
            .spawn()
            .unwrap();
        let payload = session.map_or_else(
            || serde_json::json!({}),
            |session| serde_json::json!({"session_id": session}),
        );
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    env.write_mail(
        S1,
        "01900000-0000-7000-8000-000000000009",
        "environment thread",
    );
    let output = hook(None, S1);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("environment thread"));
    env.write_mail(S2, "01900000-0000-7000-8000-00000000000a", "payload thread");
    let output = hook(Some(S2), S1);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("payload thread"));
    assert_eq!(env.mail(S1, "delivered").len(), 1);
    assert_eq!(env.mail(S2, "delivered").len(), 1);
}

#[test]
fn uninstall_removes_scoped_codex_hosts_and_keeps_claude_hosts() {
    let env = Env::new("uninstall-hosts");
    assert!(
        env.command(&["hooks", "install", "--codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    env.hook("session-start", S1);
    env.hook("session-start", S2);
    let hosts = env.state().join("hosts");
    let scoped = fs::read_dir(&hosts)
        .unwrap()
        .find_map(|e| {
            let e = e.unwrap();
            e.file_name()
                .to_string_lossy()
                .ends_with(&format!("@{S1}.json"))
                .then_some(e.path())
        })
        .unwrap();
    let key = scoped
        .file_name()
        .unwrap()
        .to_string_lossy()
        .split('@')
        .next()
        .unwrap()
        .to_string();
    fs::write(hosts.join(format!("{key}.json")), fs::read(scoped).unwrap()).unwrap();
    let claude = hosts.join("99999999-1.json");
    fs::write(&claude, serde_json::json!({"v": 1, "provider": "claude", "hook_version": "v1", "current_session": SENDER, "updated_at_ms": 1}).to_string()).unwrap();
    assert!(
        env.command(&["hooks", "uninstall", "--codex"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let remaining: Vec<_> = fs::read_dir(hosts)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(remaining, [claude]);
    let status = env.command(&["hooks", "status"]).output().unwrap();
    assert!(status.status.success());
    assert!(!String::from_utf8_lossy(&status.stdout).contains("sync hooks"));
}
