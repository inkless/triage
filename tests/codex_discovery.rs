use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn shared_daemon_descended_from_a_client_preserves_each_threads_pane_and_status() {
    let dir = std::env::temp_dir().join(format!("triage-codex-discovery-{}", std::process::id()));
    fs::create_dir_all(dir.join(".codex/sessions")).unwrap();
    for (id, cwd, status, timestamp) in [
        ("triage-thread", "/work/triage", "task_started", "00:00:01"),
        ("ux-thread", "/work/ux", "task_complete", "00:00:02"),
    ] {
        fs::write(
            dir.join(format!(".codex/sessions/rollout-{id}.jsonl")),
            format!(
                "{{\"timestamp\":\"2000-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"source\":\"cli\"}}}}\n{{\"timestamp\":\"2000-01-01T{timestamp}Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"{status}\"}}}}\n"
            ),
        )
        .unwrap();
    }
    for (name, script) in [
        (
            "ps",
            r#"
case "$*" in
*tty*) printf '8633 ?? /opt/codex/bin/codex\n9284 ?? /opt/codex/bin/codex\n9170 ttys031 codex\n24666 ttys038 codex\n' ;;
*) printf '8633 9284\n9284 9170\n9170 9028\n24666 21893\n' ;;
esac
"#,
        ),
        (
            "lsof",
            r#"
if [ "$1" = -Fn ]; then
  if [ "$3" = 8633 ]; then
    echo "n$HOME/.codex/sessions/rollout-triage-thread.jsonl"
    echo "n$HOME/.codex/sessions/rollout-ux-thread.jsonl"
  fi
else
  printf 'p8633\nfcwd\ntDIR\nn/work/triage\nf11\ntunix\nd0xdaemon\np9170\nfcwd\ntDIR\nn/work/triage\nf18\ntunix\nd0xclient1\nn->0xdaemon\np24666\nfcwd\ntDIR\nn/work/ux\nf38\ntunix\nd0xclient2\nn->0xdaemon\n'
fi
"#,
        ),
        (
            "tmux",
            r#"
case "$1" in
list-panes)
  echo 'triage|1.0|9028|/dev/ttys031|codex|/work/triage|1|%30|triage-dev'
  echo 'mobile-misc|1.0|21893|/dev/ttys038|codex|/work/ux|1|%159|ux-review' ;;
capture-pane) printf '› \n' ;;
esac
"#,
        ),
    ] {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_triage"))
        .args(["agents", "--json"])
        .env("HOME", &dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("PATH", &dir)
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (id, name, pane, state) in [
        ("triage-thread", "triage-dev", "%30", "NoProgress"),
        ("ux-thread", "ux-review", "%159", "Stale"),
    ] {
        let row = rows.iter().find(|row| row["session_id"] == id).unwrap();
        assert_eq!(row["name"], name);
        assert_eq!(row["pane_id"], pane);
        assert_eq!(row["state"], state);
    }
    fs::remove_dir_all(dir).unwrap();
}
