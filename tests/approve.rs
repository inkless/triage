use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

#[test]
fn approve_only_sends_keys_when_the_pane_shows_a_yes_selected_prompt() {
    let dir = std::env::temp_dir().join(format!("triage-approve-{}", std::process::id()));
    fs::create_dir_all(dir.join(".codex/sessions")).unwrap();
    let rollout = dir.join(".codex/sessions/rollout-test.jsonl");
    let caller_rollout = dir.join(".codex/sessions/rollout-caller.jsonl");
    for (path, id) in [(&rollout, "test"), (&caller_rollout, "caller")] {
        fs::write(
            path,
            format!(
                "{{\"timestamp\":\"2000-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"source\":\"cli\"}}}}\n"
            ),
        )
        .unwrap();
    }
    let pending = concat!(
        "{\"timestamp\":\"2000-01-01T00:00:01Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call-1\",",
        "\"arguments\":\"{\\\"cmd\\\":\\\"cargo test\\\",\\\"sandbox_permissions\\\":\\\"require_escalated\\\"}\"}}\n"
    );
    let meta = fs::read_to_string(&rollout).unwrap();
    let set_pending = |on: bool| {
        fs::write(
            &rollout,
            if on {
                format!("{meta}{pending}")
            } else {
                meta.clone()
            },
        )
        .unwrap();
    };
    for (name, body) in [
        (
            "tmux",
            r#"
case "$1" in
list-panes) echo "fixture|1.0|$FIXTURE_PID|/dev/null|codex|/tmp|1|%42|fixture" ;;
capture-pane)
  case "$SCENARIO" in
    capture-failure) exit 1 ;;
    always) printf 'Would you like to run the following command?\n  $ cargo test\n  1. Yes, proceed (y)\n› 2. Yes, and don'"'"'t ask again for commands that start with `cargo test` (p)\n  3. No, and tell Codex what to do differently (esc)\n' ;;
    yes) printf 'Would you like to run the following command?\n  $ cargo test\n› 1. Yes, proceed (y)\n  2. No, and tell Codex what to do differently (esc)\n' ;;
    no) printf 'Would you like to run the following command?\n  $ cargo test\n  1. Yes, proceed (y)\n› 2. No, and tell Codex what to do differently (esc)\n' ;;
    *) printf '› \n' ;;
  esac ;;
send-keys) echo "$*" >> "$HOME/keys" ;;
esac
"#,
        ),
        (
            "ps",
            r#"
case "$*" in
*tty*) echo "$FIXTURE_PID null codex"; echo "999998 ? codex" ;;
*comm*) echo "$FIXTURE_PID codex"; echo "999998 codex" ;;
*) echo "$FIXTURE_PID 1"; echo "999998 1"; echo "$PPID 999998" ;;
esac
exit 0
"#,
        ),
        (
            "lsof",
            r#"
case "$3" in
999998) echo "n$FIXTURE_CALLER_ROLLOUT" ;;
*) echo "n$FIXTURE_ROLLOUT" ;;
esac
"#,
        ),
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
            .env_remove("CODEX_THREAD_ID")
            .env("FIXTURE_PID", std::process::id().to_string())
            .env("FIXTURE_ROLLOUT", &rollout)
            .env("FIXTURE_CALLER_ROLLOUT", &caller_rollout)
            .env("SCENARIO", scenario)
            .output()
            .unwrap()
    };
    let keys = dir.join("keys");
    let stderr = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();

    set_pending(true);
    for (scenario, expected) in [
        ("idle", "no permission prompt visible"),
        ("always", "not a one-time Yes"),
        ("no", "not a one-time Yes"),
        ("capture-failure", "cannot inspect"),
    ] {
        let output = run(scenario, &["approve", "--to", "%42"]);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{scenario}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(expected),
            "{scenario}: {}",
            stderr(&output)
        );
        assert!(!keys.exists(), "{scenario} sent keys");
    }

    // Control: the same prompt text on screen with no pending approval in the
    // rollout (stale scrollback / quoted source) must not receive a key.
    set_pending(false);
    let stale = run("yes", &["approve", "--to", "%42"]);
    assert_eq!(stale.status.code(), Some(3), "{}", stderr(&stale));
    assert!(
        stderr(&stale).contains("no pending approval"),
        "{}",
        stderr(&stale)
    );
    assert!(!keys.exists(), "pane-only prompt sent keys");
    set_pending(true);

    let dry = run("yes", &["approve", "--to", "%42", "--dry-run"]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert!(String::from_utf8_lossy(&dry.stdout).contains("would approve %42"));
    assert!(!keys.exists());

    let approved = run("yes", &["approve", "--to", "%42"]);
    assert!(approved.status.success(), "{}", stderr(&approved));
    assert!(String::from_utf8_lossy(&approved.stdout).contains("cargo test"));
    assert_eq!(
        fs::read_to_string(&keys).unwrap(),
        "send-keys -t %42 Enter\n"
    );

    fs::remove_file(&keys).unwrap();
    let denied = run("no", &["approve", "--to", "%42", "--deny"]);
    assert!(denied.status.success(), "{}", stderr(&denied));
    assert_eq!(
        fs::read_to_string(&keys).unwrap(),
        "send-keys -t %42 Escape\n"
    );

    let audit = fs::read_to_string(dir.join(".config/triage/agent-messages.jsonl")).unwrap();
    for verdict in ["approve-refused", "approved", "denied"] {
        assert!(
            audit.contains(&format!("\"verdict\":\"{verdict}\"")),
            "missing {verdict} in {audit}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
