use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

#[test]
fn interrupt_only_sends_escape_after_all_guards_pass() {
    let dir = std::env::temp_dir().join(format!("triage-interrupt-{}", std::process::id()));
    fs::create_dir_all(dir.join(".codex/sessions")).unwrap();
    let rollout = dir.join(".codex/sessions/rollout-test.jsonl");
    let events = concat!(
        "{\"timestamp\":\"2000-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"test\",\"source\":\"cli\"}}\n",
        "{\"timestamp\":\"2000-01-01T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n"
    );
    for (name, body) in [
        (
            "tmux",
            r#"
case "$1" in
list-panes) echo "fixture|1.0|$FIXTURE_PID|/dev/null|codex|/tmp|1|%42|fixture" ;;
capture-pane)
  case "$SCENARIO" in
    capture-failure) exit 1 ;;
    animated) /bin/cat "$FIXTURE_CAPTURE" ;;
    permission) printf 'Would you like to run the following command?\n  $ test\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\n' ;;
    draft) printf '› unsent draft\n' ;;
    wrapped) printf '› \n  wrapped draft\n' ;;
    unknown) printf 'unrecognized screen\n' ;;
    placeholder) printf '› \033[2mAsk Codex to do anything\033[0m\n\n' ;;
    *) printf '› \n' ;;
  esac ;;
send-keys|load-buffer|paste-buffer) echo "$*" >> "$HOME/keys" ;;
esac
"#,
        ),
        (
            "ps",
            r#"
case "$*" in
*stat*)
  [ "$SCENARIO" = process-failure ] && exit 1
  echo "$FIXTURE_PID 1 S"
  [ "$SCENARIO" = child ] && echo "999999 $FIXTURE_PID S"
  [ "$SCENARIO" = changed ] && printf '\n{}\n' >> "$FIXTURE_ROLLOUT"
  ;;
*comm*) echo "$FIXTURE_PID codex" ;;
*) echo "$FIXTURE_PID 1" ;;
esac
exit 0
"#,
        ),
        ("lsof", "echo \"n$FIXTURE_ROLLOUT\""),
    ] {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let run = |scenario: &str, args: &[&str]| -> Output {
        Command::new(env!("CARGO_BIN_EXE_triage"))
            .args(args)
            .env("HOME", &dir)
            .env("PATH", &dir)
            .env("TMUX_PANE", "%other")
            .env("FIXTURE_PID", std::process::id().to_string())
            .env("FIXTURE_ROLLOUT", &rollout)
            .env("SCENARIO", scenario)
            .env(
                "FIXTURE_CAPTURE",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/codex-empty-165.ansi"
                ),
            )
            .output()
            .unwrap()
    };
    fs::write(&rollout, events).unwrap();
    let rows = run("quiet", &["agents", "--json"]);
    assert!(
        rows.status.success(),
        "{}",
        String::from_utf8_lossy(&rows.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&rows.stdout).unwrap();
    assert_eq!(rows[0]["state"], "NoProgress");
    assert!(rows[0]["no_progress_seconds"].as_u64().unwrap() >= 900);
    assert_eq!(rows[0]["can_receive"], true);

    for scenario in [
        "draft",
        "wrapped",
        "unknown",
        "capture-failure",
        "permission",
    ] {
        let rows = run(scenario, &["agents", "--json"]);
        assert!(rows.status.success());
        let rows: serde_json::Value = serde_json::from_slice(&rows.stdout).unwrap();
        assert_eq!(rows[0]["can_receive"], false, "{scenario}");
        let send = run(
            scenario,
            &[
                "send",
                "--to",
                "%42",
                "--from",
                "test",
                "--message",
                "hello",
            ],
        );
        assert_eq!(
            send.status.code(),
            Some(3),
            "{scenario}: {}",
            String::from_utf8_lossy(&send.stderr)
        );
        assert!(
            !dir.join("keys").exists(),
            "{scenario} pasted into the composer"
        );
    }
    for scenario in ["quiet", "placeholder", "animated"] {
        let send = run(
            scenario,
            &[
                "send",
                "--to",
                "%42",
                "--from",
                "test",
                "--message",
                "hello",
                "--dry-run",
            ],
        );
        assert!(
            send.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&send.stderr)
        );
        assert!(!dir.join("keys").exists());
    }

    let dry = run("quiet", &["interrupt", "--to", "%42", "--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert!(!dir.join("keys").exists());

    for (scenario, extra, expected) in [
        (
            "recent",
            "{\"timestamp\":\"2099-01-01T00:00:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"reasoning\"}}",
            "no observable progress",
        ),
        (
            "done",
            "{\"timestamp\":\"2000-01-01T00:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}",
            "active Codex turn",
        ),
        (
            "tool",
            "{\"timestamp\":\"2000-01-01T00:00:02Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"pending\",\"name\":\"exec\"}}",
            "unfinished tool",
        ),
        ("child", "", "live child"),
        ("permission", "", "permission prompt"),
        ("draft", "", "unsent text"),
        ("capture-failure", "", "cannot inspect"),
        ("process-failure", "", "process check failed"),
        ("changed", "", "transcript changed"),
    ] {
        fs::write(&rollout, format!("{events}{extra}\n")).unwrap();
        let output = run(scenario, &["interrupt", "--to", "%42"]);
        assert!(!output.status.success(), "{scenario} was allowed");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!dir.join("keys").exists(), "{scenario} sent keys");
    }
    fs::write(&rollout, events).unwrap();
    let output = run("quiet", &["interrupt", "--to", "%42"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.join("keys")).unwrap(),
        "send-keys -t %42 Escape\n"
    );
    fs::remove_dir_all(dir).unwrap();
}
