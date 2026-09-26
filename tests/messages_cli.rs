use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const A: &str = "aaaaaaaa-0000-4000-8000-0000000000a1";
const B: &str = "bbbbbbbb-0000-4000-8000-0000000000b2";
const C: &str = "cccccccc-0000-4000-8000-0000000000c3";
const UNLINKED: &str = "dddddddd-0000-4000-8000-0000000000d4";
const DAY: u64 = 24 * 60 * 60 * 1000;
const NOW: u64 = 1_800_000_000_000;

struct Env {
    dir: PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Env {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("triage-messages-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let env = Self { dir };
        let mail = [
            (
                "01900000-0000-7000-8000-000000000001",
                A,
                "TRI-1",
                B,
                "delivered",
                NOW - 100 * DAY,
                Some("claude-waiter"),
                "hello b",
            ),
            (
                "01900000-0000-7000-8000-000000000002",
                B,
                "TRI-2",
                A,
                "pending",
                NOW - DAY / 24,
                None,
                "reply to a",
            ),
            (
                "01900000-0000-7000-8000-000000000003",
                A,
                "TRI-1",
                C,
                "notified",
                NOW - 50 * DAY,
                None,
                "long one",
            ),
            (
                "01900000-0000-7000-8000-000000000004",
                C,
                "TRI-3",
                B,
                "pasted",
                NOW - 40 * DAY,
                Some("legacy-paste"),
                "old paste",
            ),
            (
                "01900000-0000-7000-8000-000000000005",
                A,
                "TRI-1",
                UNLINKED,
                "pending",
                NOW - 2 * DAY,
                None,
                "lost",
            ),
            (
                "01900000-0000-7000-8000-000000000006",
                A,
                "TRI-1",
                UNLINKED,
                "pending",
                NOW - DAY / 24,
                None,
                "just sent",
            ),
            (
                "01900000-0000-7000-8000-000000000007",
                B,
                "TRI-2",
                C,
                "delivered",
                NOW - DAY,
                Some("codex-user-prompt-submit"),
                "recent",
            ),
        ];
        for (id, from, label, to, state, created, via, body) in mail {
            let mut msg = serde_json::json!({
                "v": 1, "id": id, "created_at_ms": created,
                "from": {"agent": from, "session": from, "provider": "claude", "label": label},
                "to": {"agent": to, "session_at_send": to},
                "body": body, "attempt": 0, "bounce_of": null
            });
            if let Some(via) = via {
                msg["delivered_via"] = via.into();
                msg["delivered_at_ms"] = (created + 7_000).into();
            }
            let path = env
                .dir
                .join(format!("state/triage/mail/{to}/{state}/{id}.json"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, msg.to_string()).unwrap();
        }
        for linked in [A, B, C] {
            let lineage = env.dir.join("state/triage/lineage");
            fs::create_dir_all(&lineage).unwrap();
            fs::write(lineage.join(linked), linked).unwrap();
        }
        env
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_triage"))
            .args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("TRIAGE_TEST_NOW_MS", NOW.to_string())
            .output()
            .unwrap()
    }

    fn lines(&self, args: &[&str]) -> Vec<String> {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[test]
fn lists_every_message_with_both_labels_and_its_state() {
    let env = Env::new("list");
    let lines = env.lines(&["messages"]);
    assert_eq!(lines.len(), 7, "{lines:?}");
    assert!(
        lines[0].contains(
            "TRI-1 (000000a1) → TRI-2 (000000b2)  [delivered via claude-waiter · 7s]  hello b"
        ),
        "{}",
        lines[0]
    );
    assert!(lines.iter().any(|l| l.contains("[notified]")));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("0000d4 ") || l.contains("→ 000000d4"))
    );
}

#[test]
fn filters_by_agent_thread_and_pending() {
    let env = Env::new("filter");
    assert_eq!(env.lines(&["messages", "--with", "000000c3"]).len(), 3);
    let thread = env.lines(&["messages", "--thread", A, "000000b2"]);
    assert_eq!(thread.len(), 2, "both directions: {thread:?}");
    let pending = env.lines(&["messages", "--pending"]);
    assert_eq!(pending.len(), 4, "{pending:?}");
    assert!(
        pending
            .iter()
            .all(|l| l.contains("[pending") || l.contains("[notified"))
    );
    assert!(
        pending
            .iter()
            .any(|l| l.contains("unlinked 24h+") && l.contains("lost")),
        "{pending:?}"
    );
    assert!(
        pending
            .iter()
            .any(|l| !l.contains("unlinked") && l.contains("just sent")),
        "fresh unlinked mail isn't flagged yet: {pending:?}"
    );
    assert_eq!(
        env.run(&["messages", "--with", "../x"]).status.code(),
        Some(2)
    );
}

#[test]
fn json_carries_state_and_message_fields() {
    let env = Env::new("json");
    let out = env.run(&["messages", "--json", "--thread", A, B]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["state"], "delivered");
    assert_eq!(rows[0]["from"]["label"], "TRI-1");
}

#[test]
fn purge_removes_only_settled_mail_and_old_legacy_lines() {
    let env = Env::new("purge");
    let legacy = env.dir.join(".config/triage/agent-messages.jsonl");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let old_secs = (NOW - 60 * DAY) / 1000;
    let new_secs = (NOW - DAY) / 1000;
    fs::write(
        &legacy,
        format!("{{\"ts\":{old_secs},\"verdict\":\"sent\"}}\n{{\"ts\":{new_secs},\"verdict\":\"sent\"}}\n"),
    )
    .unwrap();
    let out = env.lines(&["messages", "purge", "--older-than", "30"]);
    assert_eq!(
        out,
        ["Removed 2 message(s) and 1 legacy log line(s) older than 30 day(s)."]
    );
    let left = env.lines(&["messages"]);
    assert_eq!(left.len(), 5, "{left:?}");
    assert!(
        left.iter().any(|l| l.contains("recent")),
        "recent settled mail survives"
    );
    assert!(
        left.iter().any(|l| l.contains("[notified]")),
        "notified mail is never purged"
    );
    assert_eq!(fs::read_to_string(legacy).unwrap().lines().count(), 1);
}
