use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

const CALLER_SESSION: &str = "01a0dbf1-52ad-7e22-9ecd-54582d4a3001";
const TARGET_SESSION: &str = "01a0dbf1-52ad-7e22-9ecd-54582d4a3002";
const CLEARED_SESSION: &str = "01a0dbf1-52ad-7e22-9ecd-54582d4a3003";

struct Fixture {
    dir: PathBuf,
    target: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.target.kill();
        let _ = self.target.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("triage-mailbox-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".codex/sessions")).unwrap();
        for (session, name) in [(CALLER_SESSION, "caller"), (TARGET_SESSION, "target")] {
            fs::write(
                dir.join(format!(".codex/sessions/rollout-{name}.jsonl")),
                format!(
                    "{{\"timestamp\":\"2000-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\",\"source\":\"cli\"}}}}\n"
                ),
            )
            .unwrap();
        }
        for (name, body) in [
            (
                "tmux",
                r#"
case "$1" in
list-panes)
  echo "fixture|1.0|$CALLER_PID|/dev/null|codex|/tmp|1|%41|caller"
  echo "fixture|2.0|$TARGET_PID|/dev/null|codex|/tmp|1|%42|target" ;;
capture-pane) printf '› \n' ;;
send-keys|load-buffer|paste-buffer) echo "$*" >> "$HOME/keys" ;;
esac
"#,
            ),
            (
                "ps",
                r#"
[ -n "$SANDBOXED" ] && { echo "ps: Operation not permitted" >&2; exit 1; }
case "$*" in
*comm*) echo "$CALLER_PID codex"; echo "$TARGET_PID codex" ;;
*) echo "$CALLER_PID 1"; echo "$TARGET_PID 1"; echo "$PPID $CALLER_PID" ;;
esac
"#,
            ),
            (
                "lsof",
                r#"
case "$3" in
"$CALLER_PID") echo "n$HOME/.codex/sessions/rollout-caller.jsonl" ;;
"$TARGET_PID") echo "n$HOME/.codex/sessions/rollout-target.jsonl" ;;
esac
"#,
            ),
        ] {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let target = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        Self { dir, target }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_triage"))
            .args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("PATH", &self.dir)
            .env("TMUX_PANE", "%other")
            .env("CALLER_PID", std::process::id().to_string())
            .env("TARGET_PID", self.target.id().to_string())
            .output()
            .unwrap()
    }

    fn mail(&self, agent: &str, state: &str) -> Vec<serde_json::Value> {
        let dir = self.dir.join("state/triage/mail").join(agent).join(state);
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
            .collect()
    }

    /// What the target's hooks would have written: a live host record.
    #[cfg(target_os = "macos")]
    fn make_hook_capable(&self) {
        let pid = self.target.id();
        write_json(
            &self.dir.join(format!(
                "state/triage/hosts/{pid}-{}.json",
                process_start(pid)
            )),
            serde_json::json!({
                "v": 1, "provider": "codex", "hook_version": "v1",
                "current_session": TARGET_SESSION, "updated_at_ms": 1
            }),
        );
    }

    fn run_at(&self, now_ms: u64, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_triage"))
            .args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("PATH", &self.dir)
            .env("TMUX_PANE", "%other")
            .env("CALLER_PID", std::process::id().to_string())
            .env("TARGET_PID", self.target.id().to_string())
            .env("TRIAGE_TEST_NOW_MS", now_ms.to_string())
            .output()
            .unwrap()
    }

    fn keys(&self) -> String {
        fs::read_to_string(self.dir.join("keys")).unwrap_or_default()
    }
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> u64 {
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
    assert_eq!(n, size);
    info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
}

fn write_json(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

#[test]
fn agents_json_reports_the_mailbox_agent_id() {
    let fx = Fixture::new("agents_json_reports_the_mailbox_agent_id");
    let rows: serde_json::Value =
        serde_json::from_str(&ok(&fx.run(&["agents", "--json"]))).unwrap();
    let target = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "%42")
        .unwrap();
    assert_eq!(target["agent_id"], TARGET_SESSION);
}

#[cfg(target_os = "macos")]
#[test]
fn mailbox_send_queues_without_touching_the_pane() {
    let fx = Fixture::new("mailbox_send_queues_without_touching_the_pane");
    fx.make_hook_capable();
    let short = &TARGET_SESSION[TARGET_SESSION.len() - 8..];
    let out = ok(&fx.run(&["send", "--to", short, "--mode", "mailbox", "-m", "hi"]));
    assert!(out.contains("queued"), "{out}");
    let pending = fx.mail(TARGET_SESSION, "pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["body"], "hi");
    assert_eq!(pending[0]["from"]["agent"], CALLER_SESSION);
    assert_eq!(pending[0]["from"]["provider"], "codex");
    assert_eq!(pending[0]["to"]["agent"], TARGET_SESSION);
    assert_eq!(fx.keys(), "");
}

#[test]
fn a_target_without_hooks_gets_the_message_pasted() {
    let fx = Fixture::new("fallback");
    let out = ok(&fx.run(&["send", "--to", "%42", "--mode", "mailbox", "-m", "hi"]));
    assert!(out.contains("pasted"), "{out}");
    assert!(fx.keys().contains("paste-buffer"), "{}", fx.keys());
    assert!(fx.mail(TARGET_SESSION, "pending").is_empty());
    let pasted = fx.mail(TARGET_SESSION, "pasted");
    assert_eq!(pasted.len(), 1);
    assert_eq!(pasted[0]["delivered_via"], "legacy-paste");
}

#[cfg(target_os = "macos")]
#[test]
fn wait_returns_once_delivered_and_exits_5_while_still_queued() {
    let fx = Fixture::new("wait");
    fx.make_hook_capable();
    let queued = fx.run(&[
        "send", "--to", "%42", "--mode", "mailbox", "--wait=1", "-m", "a",
    ]);
    assert_eq!(queued.status.code(), Some(5));
    assert_eq!(fx.mail(TARGET_SESSION, "pending").len(), 1);

    let mail = fx.dir.join("state/triage/mail").join(TARGET_SESSION);
    let deliverer = {
        let mail = mail.clone();
        std::thread::spawn(move || {
            for _ in 0..100 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let pending: Vec<_> = fs::read_dir(mail.join("pending"))
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .collect();
                if pending.len() == 2 {
                    fs::create_dir_all(mail.join("delivered")).unwrap();
                    for path in pending {
                        fs::rename(
                            &path,
                            mail.join("delivered").join(path.file_name().unwrap()),
                        )
                        .unwrap();
                    }
                    return;
                }
            }
        })
    };
    let delivered = ok(&fx.run(&[
        "send",
        "--to",
        "%42",
        "--mode",
        "mailbox",
        "--wait=10",
        "-m",
        "b",
    ]));
    assert!(delivered.starts_with("delivered"), "{delivered}");
    deliverer.join().unwrap();
}

#[test]
fn mail_for_a_host_dead_over_30s_bounces_to_the_sender_once() {
    let fx = Fixture::new("bounce");
    let dead_agent = "01a0dbf1-52ad-7e22-9ecd-54582d4a3009";
    write_json(
        &fx.dir.join("state/triage/hosts/99999999-1.json"),
        serde_json::json!({
            "v": 1, "provider": "claude", "hook_version": "v1",
            "current_session": dead_agent, "updated_at_ms": 1
        }),
    );
    for (id, bounce_of) in [
        (
            "01900000-0000-7000-8000-000000000001",
            serde_json::Value::Null,
        ),
        (
            "01900000-0000-7000-8000-000000000002",
            serde_json::json!("01900000-0000-7000-8000-0000000000aa"),
        ),
    ] {
        write_json(
            &fx.dir
                .join("state/triage/mail")
                .join(dead_agent)
                .join(format!("pending/{id}.json")),
            serde_json::json!({
                "v": 1, "id": id, "created_at_ms": 1,
                "from": {"agent": CALLER_SESSION, "session": CALLER_SESSION, "provider": "codex", "label": "me"},
                "to": {"agent": dead_agent, "session_at_send": dead_agent},
                "body": "secret body", "attempt": 0, "bounce_of": bounce_of
            }),
        );
    }
    let t0 = 1_800_000_000_000;
    ok(&fx.run_at(t0, &["agents", "--json"]));
    assert_eq!(
        fx.mail(dead_agent, "pending").len(),
        2,
        "first sighting only"
    );
    ok(&fx.run_at(t0 + 10_000, &["agents", "--json"]));
    assert_eq!(fx.mail(dead_agent, "pending").len(), 2, "dead under 30s");
    ok(&fx.run_at(t0 + 31_000, &["agents", "--json"]));
    ok(&fx.run_at(t0 + 62_000, &["agents", "--json"]));
    assert!(fx.mail(dead_agent, "pending").is_empty());
    assert_eq!(fx.mail(dead_agent, "undeliverable").len(), 2);
    let bounces = fx.mail(CALLER_SESSION, "pending");
    assert_eq!(
        bounces.len(),
        1,
        "one bounce, and a bounce is never bounced"
    );
    assert_eq!(
        bounces[0]["bounce_of"],
        "01900000-0000-7000-8000-000000000001"
    );
    assert!(!bounces[0]["body"].as_str().unwrap().contains("secret body"));
}

#[test]
fn legacy_send_pastes_and_records_the_message() {
    let fx = Fixture::new("legacy_send_pastes_and_records_the_message");
    ok(&fx.run(&["send", "--to", "%42", "-m", "hello"]));
    assert!(fx.keys().contains("paste-buffer"), "{}", fx.keys());
    let pasted = fx.mail(TARGET_SESSION, "pasted");
    assert_eq!(pasted.len(), 1);
    assert_eq!(pasted[0]["body"], "hello");
    assert_eq!(pasted[0]["delivered_via"], "legacy-paste");
    assert!(fx.mail(TARGET_SESSION, "pending").is_empty());
}

#[test]
fn a_sandboxed_send_says_to_run_outside_the_sandbox() {
    let fx = Fixture::new("sandboxed");
    let out = Command::new(env!("CARGO_BIN_EXE_triage"))
        .args(["send", "--to", "%42", "--mode", "mailbox", "-m", "hi"])
        .env("HOME", &fx.dir)
        .env("XDG_STATE_HOME", fx.dir.join("state"))
        .env("PATH", &fx.dir)
        .env("TMUX_PANE", "%other")
        .env("CALLER_PID", std::process::id().to_string())
        .env("TARGET_PID", fx.target.id().to_string())
        .env("SANDBOXED", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("request escalated permissions"), "{stderr}");
}

#[test]
fn sending_to_yourself_is_a_usage_error() {
    let fx = Fixture::new("sending_to_yourself_is_a_usage_error");
    for mode in ["legacy", "mailbox"] {
        let out = fx.run(&["send", "--to", "%41", "--mode", mode, "-m", "x"]);
        assert_eq!(out.status.code(), Some(2), "{mode}");
    }
    assert_eq!(fx.keys(), "");
}

#[cfg(target_os = "macos")]
#[test]
fn inbox_commits_only_from_the_agents_current_session() {
    let fx = Fixture::new("inbox_commits_only_from_the_agents_current_session");
    let id = "01900000-0000-7000-8000-000000000001";
    write_json(
        &fx.dir
            .join("state/triage/mail")
            .join(CALLER_SESSION)
            .join(format!("pending/{id}.json")),
        serde_json::json!({
            "v": 1, "id": id, "created_at_ms": 1,
            "from": {"agent": TARGET_SESSION, "session": TARGET_SESSION, "provider": "claude", "label": "TRI-148"},
            "to": {"agent": CALLER_SESSION, "session_at_send": CALLER_SESSION},
            "body": "rebase please", "attempt": 0, "bounce_of": null
        }),
    );
    let host = fx.dir.join(format!(
        "state/triage/hosts/{}-{}.json",
        std::process::id(),
        process_start(std::process::id())
    ));
    write_json(
        &host,
        serde_json::json!({
            "v": 1, "provider": "codex", "hook_version": "v1",
            "current_session": CLEARED_SESSION, "updated_at_ms": 1
        }),
    );
    fs::create_dir_all(fx.dir.join("state/triage/lineage")).unwrap();
    fs::write(
        fx.dir.join("state/triage/lineage").join(CLEARED_SESSION),
        CALLER_SESSION,
    )
    .unwrap();

    let stale = ok(&fx.run(&["inbox"]));
    assert!(stale.contains("rebase please"), "{stale}");
    assert!(stale.contains("read-only"), "{stale}");
    assert_eq!(fx.mail(CALLER_SESSION, "pending").len(), 1);

    fs::remove_file(&host).unwrap();
    let current = ok(&fx.run(&["inbox"]));
    assert!(current.contains("rebase please"), "{current}");
    assert!(fx.mail(CALLER_SESSION, "pending").is_empty());
    let delivered = fx.mail(CALLER_SESSION, "delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["delivered_via"], "inbox");

    assert!(ok(&fx.run(&["inbox", "show", id])).contains("rebase please"));
    assert_eq!(fx.run(&["inbox", "show", "../x"]).status.code(), Some(2));
}
