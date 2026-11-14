use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PI_SESSION: &str = "019a8f3c-0000-7000-8000-0000000000a1";

struct Fixture {
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Fixture {
    fn new(name: &str, panes: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("triage-pane-agents-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("data/opencode")).unwrap();
        fs::write(dir.join("data/opencode/opencode.db"), "").unwrap();
        fs::create_dir_all(dir.join(".pi/agent/sessions/--work-pi--")).unwrap();
        fs::write(
            dir.join(".pi/agent/sessions/--work-pi--/2026-10-14T18-49-13-644Z_01a1.jsonl"),
            concat!(
                r#"{"type":"session","version":3,"id":"019a8f3c-0000-7000-8000-0000000000a1","timestamp":"2026-10-14T18:49:13.644Z","cwd":"/work/pi"}"#,
                "\n",
                r#"{"type":"session_info","id":"i1","timestamp":"2026-10-14T18:49:14Z","name":"pi-worker"}"#,
                "\n",
                r#"{"type":"message","id":"m1","timestamp":"2026-10-14T18:49:15Z","message":{"role":"user"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let tmux = format!(
            r#"
case "$1" in
list-panes)
{panes} ;;
capture-pane) printf '%s\n' "$CAPTURE" ;;
load-buffer) /bin/cat "$4" >> "$HOME/pasted"; echo "$*" >> "$HOME/keys" ;;
send-keys|paste-buffer) echo "$*" >> "$HOME/keys" ;;
esac
"#
        );
        for (name, body) in [
            ("tmux", tmux.as_str()),
            (
                "sqlite3",
                r#"
case "$*" in
*"where id = 'ses_oc'"*|*"where id = 'ses_twin'"*) printf '[{"directory":"/work/oc"}]' ;;
*"time_updated >="*) printf '[{"id":"ses_oc","directory":"/work/oc","time_updated":2000000000000}%s]' "$EXTRA_OC_ROWS" ;;
*"where id ="*) printf '[{"directory":"/work/other"}]' ;;
*session_v2*) printf '[{"id":"ses_oc","directory":"/work/oc","title":"Fix the parser","time_created":1,"time_updated":2000000000000,"time_busy":%s,"time_idle":20}]' "${OC_BUSY_AT:-10}" ;;
esac
"#,
            ),
            (
                "ps",
                r#"
case "$*" in
*etime*) [ -n "$HARNESS_ARGS" ] && echo "  00:05 $HARNESS_ARGS" ;;
*tty*|*comm*) ;;
*) echo "$PPID 777001"; echo "777001 777000"; echo "777000 1"; if [ -n "$EXTRA_PS" ]; then echo "$EXTRA_PS"; fi ;;
esac
"#,
            ),
            ("lsof", ""),
        ] {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_triage"));
        command
            .args(args)
            .env("HOME", &self.dir)
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("PATH", &self.dir)
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("TRIAGE_TEST_PANE_HARNESS")
            .env_remove("OPENCODE_SESSION_ID")
            .env_remove("PI_SESSION_ID")
            .env_remove("PI_SESSION_FILE")
            .env_remove("TMUX_PANE")
            .env("CAPTURE", "")
            .env_remove("HARNESS_ARGS")
            .env_remove("EXTRA_OC_ROWS")
            .env_remove("CODEX_THREAD_ID");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn agents(&self) -> Vec<serde_json::Value> {
        let output = self.run(&["agents", "--json"]);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn row<'a>(rows: &'a [serde_json::Value], pane: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["pane_id"] == pane)
        .unwrap_or_else(|| panic!("no row for {pane}: {rows:?}"))
}

#[test]
fn lists_opencode_and_pi_panes_with_session_names_and_state() {
    let fx = Fixture::new(
        "list",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'pi|1.0|502|/dev/null|pi|/work/pi|1|%31|pi'
  echo 'sh|1.0|503|/dev/null|fish|/work/oc|1|%32|fish'"#,
    );
    let rows = fx.agents();
    assert_eq!(rows.len(), 2, "{rows:?}");

    let oc = row(&rows, "%30");
    assert_eq!(oc["provider"], "oc");
    assert_eq!(oc["name"], "opencode:%30");
    assert_eq!(oc["headline"], "Fix the parser");
    assert_eq!(oc["cwd"], "/work/oc");
    assert_eq!(oc["session_id"], "ses_oc");
    assert_eq!(oc["agent_id"], "ses_oc");
    assert_ne!(oc["state"], "Working");

    let pi = row(&rows, "%31");
    assert_eq!(pi["provider"], "pi");
    assert_eq!(pi["name"], "pi-worker");
    assert_eq!(pi["cwd"], "/work/pi");
    assert_eq!(pi["session_id"], PI_SESSION);
    assert_eq!(pi["state"], "Working");

    let only_pi = fx.run(&["agents", "--json", "--provider", "pi"]);
    let only_pi: Vec<serde_json::Value> = serde_json::from_slice(&only_pi.stdout).unwrap();
    assert_eq!(only_pi.len(), 1);
    assert_eq!(only_pi[0]["pane_id"], "%31");
}

#[test]
fn panes_sharing_a_cwd_are_listed_but_not_named_after_a_guess() {
    let fx = Fixture::new(
        "twins",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'oc|2.0|502|/dev/null|opencode|/work/oc|1|%31|opencode'"#,
    );
    let rows = fx.agents();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for pane in ["%30", "%31"] {
        let r = row(&rows, pane);
        assert_eq!(r["session_id"], pane);
        assert_eq!(r["state"], "Unknown");
        assert_eq!(r["name"], format!("opencode:{pane}"));
        assert_eq!(r["headline"], serde_json::Value::Null);
    }
}

#[test]
fn a_missing_session_store_still_lists_the_pane() {
    let fx = Fixture::new(
        "nostore",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/elsewhere|1|%30|opencode'"#,
    );
    fs::remove_file(fx.dir.join("data/opencode/opencode.db")).unwrap();
    let rows = fx.agents();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(row(&rows, "%30")["cwd"], "/work/elsewhere");
    assert!(Path::new(&fx.dir).exists());
}

#[test]
fn a_tmux_window_name_never_names_a_pane_agent() {
    // pi wrote no `session_info` name; the window called "random" must not
    // leak in as the agent's name.
    let fx = Fixture::new(
        "window",
        r#"  echo 'pi|1.0|502|/dev/null|pi|/work/unnamed|1|%48|random'"#,
    );
    let rows = fx.agents();
    assert_eq!(row(&rows, "%48")["name"], "pi:%48");
}

#[test]
fn a_rename_names_the_row_and_survives_the_session_match() {
    let fx = Fixture::new(
        "rename",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'pi|1.0|502|/dev/null|pi|/work/pi|1|%31|pi'"#,
    );
    // An alias set by session id (oc) and one set by pane id before a
    // session was matched (pi).
    fs::create_dir_all(fx.dir.join(".config/triage")).unwrap();
    fs::write(
        fx.dir.join(".config/triage/state.json"),
        r#"{"aliases":[{"provider":"oc","session_id":"ses_oc","alias":"bazhen-oc"},
                       {"provider":"pi","session_id":"%31","alias":"pi-scout"}]}"#,
    )
    .unwrap();
    let rows = fx.agents();
    assert_eq!(row(&rows, "%30")["name"], "bazhen-oc");
    assert_eq!(row(&rows, "%31")["name"], "pi-scout");
}

/// The caller runs in a pi pane: triage → pi harness (777001) → pane root
/// (777000), as the fake `ps` reports.
const PI_CALLER_AND_OC_TARGET: &str = r#"  echo 'pi|1.0|777000|/dev/null|pi|/work/pi|1|%31|pi'
  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'"#;

fn audit(fx: &Fixture) -> Vec<serde_json::Value> {
    fs::read_to_string(fx.dir.join(".config/triage/agent-messages.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_pi_pane_sends_to_an_opencode_pane_under_its_pane_name() {
    let fx = Fixture::new("send", PI_CALLER_AND_OC_TARGET);
    let output = fx
        .command(&["send", "--to", "%30", "--message", "hello oc"])
        .env("TRIAGE_TEST_PANE_HARNESS", "777001")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));

    let pasted = fs::read_to_string(fx.dir.join("pasted")).unwrap();
    assert_eq!(pasted, "[triage message from pi:%31] hello oc");
    let keys = fs::read_to_string(fx.dir.join("keys")).unwrap();
    assert!(keys.contains("%30"), "{keys}");

    let audit = audit(&fx);
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["from"], "pi:%31");
    assert_eq!(audit[0]["verdict"], "sent");
    assert_eq!(audit[0]["from_session"], serde_json::Value::Null);
    assert_eq!(audit[0]["target_id"], "%30");
    assert_eq!(audit[0]["target_provider"], "oc");

    let whoami = fx
        .command(&["agents", "whoami", "--json"])
        .env("TRIAGE_TEST_PANE_HARNESS", "777001")
        .output()
        .unwrap();
    assert!(whoami.status.success(), "{}", stderr(&whoami));
    let me: serde_json::Value = serde_json::from_slice(&whoami.stdout).unwrap();
    assert_eq!(me["pane_id"], "%31");
    assert_eq!(me["provider"], "pi");

    let inbox = fx
        .command(&["inbox"])
        .env("TRIAGE_TEST_PANE_HARNESS", "777001")
        .output()
        .unwrap();
    assert!(!inbox.status.success());
    assert!(
        stderr(&inbox).contains("cannot tell which pi session is calling"),
        "{}",
        stderr(&inbox)
    );
}

#[test]
fn a_caller_not_under_the_harness_is_refused() {
    let fx = Fixture::new("noharness", PI_CALLER_AND_OC_TARGET);
    let output = fx.run(&["send", "--to", "%30", "--message", "hello"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("not under its pi process"));
    assert!(!fx.dir.join("pasted").exists());
}

#[test]
fn a_visible_permission_prompt_still_blocks_a_pane_only_target() {
    let fx = Fixture::new("prompt", PI_CALLER_AND_OC_TARGET);
    let output = fx
        .command(&["send", "--to", "%30", "--message", "hello"])
        .env("TRIAGE_TEST_PANE_HARNESS", "777001")
        .env(
            "CAPTURE",
            "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(!fx.dir.join("pasted").exists());
    let audit = audit(&fx);
    assert_eq!(audit[0]["verdict"], "denied");
    assert_eq!(audit[0]["from"], "pi:%31");
}

#[test]
fn an_opencode_tool_shell_sends_as_its_pane() {
    // The caller's process chain never reaches a pane (opencode runs tool
    // shells under its shared server); its env names the session and pane.
    let fx = Fixture::new(
        "ocsend",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'pi|1.0|502|/dev/null|pi|/work/pi|1|%31|pi'"#,
    );
    // The fake `ps` puts 777001 between the caller and init; it stands in for
    // both the opencode process and the server named in service.json.
    fs::create_dir_all(fx.dir.join("state/opencode")).unwrap();
    fs::write(
        fx.dir.join("state/opencode/service.json"),
        r#"{"url":"http://127.0.0.1:4096","pid":777001,"password":"x"}"#,
    )
    .unwrap();
    let send = |session: &str| {
        fx.command(&["send", "--to", "%31", "--message", "hi pi"])
            .envs(OPENCODE_IN_PANE_30)
            .env("OPENCODE_SESSION_ID", session)
            .output()
            .unwrap()
    };

    let output = send("ses_oc");
    assert!(output.status.success(), "{}", stderr(&output));
    let pasted = fs::read_to_string(fx.dir.join("pasted")).unwrap();
    assert_eq!(pasted, "[triage message from opencode:%30] hi pi");
    let sent = audit(&fx);
    assert_eq!(sent[0]["from"], "opencode:%30");
    assert_eq!(sent[0]["from_session"], "ses_oc");
    assert_eq!(sent[0]["target_id"], "%31");

    let output = send("ses_from_another_dir");
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("not a session in %30's directory"));
    assert_eq!(audit(&fx).len(), 1);

    fs::remove_file(fx.dir.join("state/opencode/service.json")).unwrap();
    let output = send("ses_oc");
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("not under the opencode server"));
    assert_eq!(audit(&fx).len(), 1);
}

#[test]
fn opencode_env_from_inside_another_pane_is_refused() {
    // The caller's chain reaches pane %32's root (777000): a process there
    // renamed `opencode` cannot speak for %30.
    let fx = Fixture::new(
        "ocforge",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'pi|1.0|502|/dev/null|pi|/work/pi|1|%31|pi'
  echo 'sh|1.0|777000|/dev/null|fish|/work/sh|1|%32|fish'"#,
    );
    fs::create_dir_all(fx.dir.join("state/opencode")).unwrap();
    fs::write(
        fx.dir.join("state/opencode/service.json"),
        r#"{"pid":777001}"#,
    )
    .unwrap();
    let output = fx
        .command(&["send", "--to", "%31", "--message", "hi pi"])
        .env("TRIAGE_TEST_PANE_HARNESS", "777001")
        .env("OPENCODE_SESSION_ID", "ses_oc")
        .env("TMUX_PANE", "%30")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("sits in a tmux pane"),
        "{}",
        stderr(&output)
    );
    assert!(!fx.dir.join("pasted").exists());
}

#[test]
fn a_server_or_one_shot_harness_pane_is_not_listed() {
    // A real process named `opencode` as the pane's own process, so
    // discovery finds the harness pid and asks `ps` for its arguments.
    let fx = Fixture::new(
        "serve",
        "  echo \"oc|1.0|$HARNESS_PID|/dev/null|opencode|/work/oc|1|%30|opencode\"",
    );
    let bin = fx.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::copy("/bin/sleep", bin.join("opencode")).unwrap();
    // Another test thread's fork can briefly hold the copy's write handle.
    let mut harness = (0..50)
        .find_map(
            |_| match Command::new(bin.join("opencode")).arg("30").spawn() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
                spawned => Some(spawned.unwrap()),
            },
        )
        .unwrap();
    let agents = |args: &str| {
        let output = fx
            .command(&["agents", "--json"])
            .env("HARNESS_PID", harness.id().to_string())
            .env("HARNESS_ARGS", args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout).unwrap()
    };
    let interactive = agents("opencode");
    let server = agents("opencode serve --port 4096");
    harness.kill().unwrap();
    harness.wait().unwrap();
    assert_eq!(interactive.len(), 1, "{interactive:?}");
    assert!(server.is_empty(), "{server:?}");
}

#[test]
fn two_pi_panes_in_one_directory_send_under_their_pane_only() {
    let fx = Fixture::new(
        "pitwins",
        r#"  echo 'pi|1.0|777000|/dev/null|pi|/work/pi|1|%31|pi'
  echo 'pi|2.0|503|/dev/null|pi|/work/pi|1|%33|pi'
  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'"#,
    );
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "hello oc"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let sent = audit(&fx);
    assert_eq!(sent[0]["from"], "pi:%31");
    assert_eq!(sent[0]["from_session"], serde_json::Value::Null);
    let inbox = fx.as_pi(&["inbox"]);
    assert!(!inbox.status.success());
    assert!(stderr(&inbox).contains("cannot tell which pi session is calling"));
}

#[test]
fn a_pi_tool_shell_sends_as_the_session_its_env_names() {
    let fx = Fixture::new("pisession", PI_CALLER_AND_OC_TARGET);
    let send = |session: &str| {
        fx.command(&["send", "--to", "%30", "--message", "hello oc"])
            .env("TRIAGE_TEST_PANE_HARNESS", "777001")
            .env("PI_SESSION_ID", session)
            .env("PI_SESSION_FILE", "/unused")
            .output()
            .unwrap()
    };

    let output = send(PI_SESSION);
    assert!(output.status.success(), "{}", stderr(&output));
    let sent = audit(&fx);
    assert_eq!(sent[0]["from"], "pi:%31");
    assert_eq!(sent[0]["from_session"], PI_SESSION);

    let output = send("not-a-session-here");
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("is not a pi session in /work/pi"),
        "{}",
        stderr(&output)
    );
    assert_eq!(audit(&fx).len(), 1);
}

/// An opencode TUI (777002) runs in pane %30 (root 501), started five
/// seconds ago; 777001, in every caller's chain, is its server.
const OPENCODE_IN_PANE_30: [(&str, &str); 4] = [
    ("TRIAGE_TEST_PANE_HARNESS", "777001,777002"),
    ("EXTRA_PS", "777002 501"),
    ("HARNESS_ARGS", "opencode"),
    ("TMUX_PANE", "%30"),
];

/// The caller's process chain reaches no pane: an opencode tool shell.
const OC_CALLER_AND_PI_TARGET: &str = r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'pi|1.0|502|/dev/null|pi|/work/pi|1|%31|pi'"#;

fn mail(fx: &Fixture, agent: &str, state: &str) -> Vec<serde_json::Value> {
    let Ok(entries) = fs::read_dir(fx.dir.join(format!("state/triage/mail/{agent}/{state}")))
    else {
        return Vec::new();
    };
    entries
        .map(|e| serde_json::from_slice(&fs::read(e.unwrap().path()).unwrap()).unwrap())
        .collect()
}

impl Fixture {
    /// The test process also stands in for the opencode server behind
    /// `fake_opencode_server`.
    fn as_pi(&self, args: &[&str]) -> Output {
        self.command(args)
            .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
            .env("PI_SESSION_ID", PI_SESSION)
            .env("PI_SESSION_FILE", "/unused")
            .output()
            .unwrap()
    }

    /// 777001 is in every caller's chain, so it can stand in for the
    /// opencode server as well.
    fn record_opencode_server(&self) {
        fs::create_dir_all(self.dir.join("state/opencode")).unwrap();
        fs::write(
            self.dir.join("state/opencode/service.json"),
            r#"{"url":"http://127.0.0.1:4096","pid":777001,"password":"x"}"#,
        )
        .unwrap();
    }

    fn as_opencode(&self, args: &[&str]) -> Output {
        self.record_opencode_server();
        self.command(args)
            .envs(OPENCODE_IN_PANE_30)
            .env("OPENCODE_SESSION_ID", "ses_oc")
            .output()
            .unwrap()
    }
}

#[test]
fn opencode_and_pi_sessions_have_mailboxes() {
    let fx = Fixture::new("mailbox", PI_CALLER_AND_OC_TARGET);

    let send = ["send", "--mode", "mailbox", "--to"];
    let output = fx.as_pi(&[&send[..], &["%30", "--message", "to oc"]].concat());
    assert!(output.status.success(), "{}", stderr(&output));
    let to_oc = mail(&fx, "ses_oc", "pasted");
    assert_eq!(to_oc.len(), 1, "{to_oc:?}");
    assert_eq!(to_oc[0]["from"]["agent"], PI_SESSION);
    assert_eq!(to_oc[0]["from"]["provider"], "pi");
    assert_eq!(to_oc[0]["body"], "to oc");

    let inbox = fx.as_pi(&["inbox"]);
    assert!(inbox.status.success(), "{}", stderr(&inbox));
    assert!(String::from_utf8_lossy(&inbox.stdout).contains("No pending messages"));

    // An opencode caller is reached through its env, not a pane walk.
    let fx = Fixture::new("mailbox-oc", OC_CALLER_AND_PI_TARGET);
    let output = fx.as_opencode(&[&send[..], &["%31", "--message", "to pi"]].concat());
    assert!(output.status.success(), "{}", stderr(&output));
    let to_pi = mail(&fx, PI_SESSION, "pasted");
    assert_eq!(to_pi.len(), 1, "{to_pi:?}");
    assert_eq!(to_pi[0]["from"]["agent"], "ses_oc");

    let id = "01900000-0000-7000-8000-0000000000c1";
    let mut pending = to_pi[0].clone();
    pending["to"]["agent"] = "ses_oc".into();
    pending["to"]["session_at_send"] = "ses_oc".into();
    pending["id"] = id.into();
    pending["body"] = "read me".into();
    let dir = fx.dir.join("state/triage/mail/ses_oc/pending");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{id}.json")), pending.to_string()).unwrap();
    let inbox = fx.as_opencode(&["inbox"]);
    assert!(inbox.status.success(), "{}", stderr(&inbox));
    assert!(String::from_utf8_lossy(&inbox.stdout).contains("read me"));
    assert!(mail(&fx, "ses_oc", "pending").is_empty());
    assert_eq!(mail(&fx, "ses_oc", "delivered").len(), 1);
}

struct Request {
    method: String,
    path: String,
    authorization: String,
    body: serde_json::Value,
}

fn fake_opencode_server(
    fx: &Fixture,
    status_for: fn(&str) -> u16,
) -> std::sync::mpsc::Receiver<Request> {
    fake_opencode_api(fx, move |_, path| (status_for(path), String::new()))
}

/// A loopback stand-in for the opencode server: answers each request with
/// `respond(method, path)` and hands the requests back once the test is done.
fn fake_opencode_api(
    fx: &Fixture,
    respond: impl Fn(&str, &str) -> (u16, String) + Send + 'static,
) -> std::sync::mpsc::Receiver<Request> {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    fs::create_dir_all(fx.dir.join("state/opencode")).unwrap();
    fs::write(
        fx.dir.join("state/opencode/service.json"),
        serde_json::json!({"url": url, "pid": std::process::id(), "password": "s3cret"})
            .to_string(),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let method = line.split_whitespace().next().unwrap_or("").to_string();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let (mut authorization, mut len) = (String::new(), 0);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(": ").unwrap();
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => authorization = value.to_string(),
                    "content-length" => len = value.parse().unwrap(),
                    _ => {}
                }
            }
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let (status, answer) = respond(&method, &path);
            let body = serde_json::from_slice(&body).unwrap_or_default();
            // Recorded before answering, so it is there once the sender exits.
            let sent = tx.send(Request {
                method,
                path,
                authorization,
                body,
            });
            if status == STALL {
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
            write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{answer}",
                answer.len()
            )
            .unwrap();
            if sent.is_err() {
                break;
            }
        }
    });
    rx
}

#[test]
fn mail_to_opencode_goes_through_its_server_not_the_pane() {
    let fx = Fixture::new("ocapi", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_server(&fx, |_| 200);
    let output = fx.as_pi(&["send", "--to", "%30", "--wait", "--message", "over the API"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("delivered"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fx.dir.join("pasted").exists());

    let request = requests.try_recv().unwrap();
    assert_eq!(request.path, "/api/session/ses_oc/synthetic");
    // base64("opencode:s3cret")
    assert_eq!(request.authorization, "Basic b3BlbmNvZGU6czNjcmV0");
    let text = request.body["text"].as_str().unwrap();
    assert!(text.starts_with("📨 Peer message from"), "{text}");
    assert!(text.contains("\nover the API\n"), "{text}");
    assert!(text.contains("Reply: triage send --to "), "{text}");
    assert_eq!(request.body["delivery"], "queue");
    assert!(requests.try_recv().is_err());

    let delivered = mail(&fx, "ses_oc", "delivered");
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0]["delivered_via"], "opencode-server");
    assert_eq!(delivered[0]["from"]["agent"], PI_SESSION);
    assert_eq!(audit(&fx)[0]["verdict"], "sent");
}

#[test]
fn a_server_without_synthetic_input_gets_a_queued_prompt() {
    let fx = Fixture::new("ocprompt", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_server(&fx, |path| {
        if path.ends_with("/synthetic") {
            404
        } else {
            200
        }
    });
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "fallback"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!fx.dir.join("pasted").exists());
    let paths: Vec<String> = requests.try_iter().map(|r| r.path).collect();
    assert_eq!(
        paths,
        [
            "/api/session/ses_oc/synthetic",
            "/api/session/ses_oc/prompt"
        ]
    );
    assert_eq!(mail(&fx, "ses_oc", "delivered").len(), 1);
}

fn pi_and_test_server() -> String {
    format!("777001,{}", std::process::id())
}

/// Points service.json at `pid` while the fake server keeps listening.
fn record_server_pid(fx: &Fixture, pid: u32) {
    let path = fx.dir.join("state/opencode/service.json");
    let mut service: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    service["pid"] = pid.into();
    fs::write(path, service.to_string()).unwrap();
}

#[test]
fn a_stale_service_record_gets_neither_the_password_nor_the_mail() {
    let fx = Fixture::new("ocstale", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_server(&fx, |_| 200);

    let mut gone = Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    record_server_pid(&fx, gone.id());
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "first"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not a running opencode server"),
        "{}",
        stderr(&output)
    );

    // A live process that is not opencode.
    let mut other = Command::new("sleep").arg("30").spawn().unwrap();
    record_server_pid(&fx, other.id());
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "second"]);
    assert!(
        stderr(&output).contains("not a running opencode server"),
        "{}",
        stderr(&output)
    );

    // An opencode process that does not hold the port the record names.
    if cfg!(target_os = "linux") {
        let output = fx
            .command(&["send", "--to", "%30", "--message", "third"])
            .env("TRIAGE_TEST_PANE_HARNESS", format!("777001,{}", other.id()))
            .env("PI_SESSION_ID", PI_SESSION)
            .env("PI_SESSION_FILE", "/unused")
            .output()
            .unwrap();
        assert!(
            stderr(&output).contains("does not own the listening port"),
            "{}",
            stderr(&output)
        );
    }
    let _ = other.kill();
    let _ = other.wait();

    assert_eq!(requests.try_iter().count(), 0);
    assert!(mail(&fx, "ses_oc", "delivered").is_empty());
    assert!(!mail(&fx, "ses_oc", "pasted").is_empty());
}

/// A `status_for` answer: read the request, then never reply.
const STALL: u16 = 0;

#[test]
fn a_server_that_takes_the_request_but_never_answers_gets_no_paste() {
    let fx = Fixture::new("ocstall", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_server(&fx, |_| STALL);
    let output = fx
        .command(&["send", "--to", "%30", "--message", "maybe twice"])
        .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
        .env("PI_SESSION_ID", PI_SESSION)
        .env("PI_SESSION_FILE", "/unused")
        .env("TRIAGE_TEST_HTTP_TIMEOUT_MS", "300")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("did not answer"),
        "{}",
        stderr(&output)
    );
    assert_eq!(requests.try_iter().count(), 1);
    assert!(!fx.dir.join("pasted").exists());
    let recorded = mail(&fx, "ses_oc", "delivered");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["delivered_via"], "opencode-server-unconfirmed");
    assert_eq!(audit(&fx)[0]["verdict"], "sent-unconfirmed");
}

#[test]
fn a_failing_or_unsafe_server_falls_back_to_a_paste() {
    let fx = Fixture::new("ocfail", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_server(&fx, |_| 500);
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "paste me"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("pasting it instead"),
        "{}",
        stderr(&output)
    );
    assert_eq!(requests.try_iter().count(), 1);
    let pasted = fs::read_to_string(fx.dir.join("pasted")).unwrap();
    assert_eq!(pasted, "[triage message from pi:%31] paste me");
    assert!(mail(&fx, "ses_oc", "delivered").is_empty());
    assert_eq!(mail(&fx, "ses_oc", "pasted").len(), 1);

    // The password never leaves the machine.
    fs::write(
        fx.dir.join("state/opencode/service.json"),
        r#"{"url":"http://192.0.2.1:4096","pid":1,"password":"s3cret"}"#,
    )
    .unwrap();
    let output = fx.as_pi(&["send", "--to", "%30", "--message", "again"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not loopback"),
        "{}",
        stderr(&output)
    );
    assert_eq!(mail(&fx, "ses_oc", "pasted").len(), 2);
}

#[test]
fn mail_to_a_pi_session_with_the_extension_waits_for_its_waiter() {
    // The test process plays the pi process inside pane %31 (root 502); the
    // extension registered a session other than the newest file in /work/pi.
    let fx = Fixture::new("pihost", OC_CALLER_AND_PI_TARGET);
    let pi = std::process::id().to_string();
    let hosted = "019a8f3c-0000-7000-8000-0000000000e1";
    let harnesses = format!("777001,777002,{pi}");
    let as_pi_hook = |event: &str, ack: serde_json::Value| {
        let mut child = fx
            .command(&["inbox", "--hook", "pi", event, "--triage-hook=v1"])
            .env("TRIAGE_TEST_PANE_HARNESS", &pi)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({ "session_id": hosted, "ack": ack }).to_string();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let with_pi = |args: &[&str]| {
        fx.command(args)
            .envs(OPENCODE_IN_PANE_30)
            .env("TRIAGE_TEST_PANE_HARNESS", &harnesses)
            .env("EXTRA_PS", format!("777002 501\n{pi} 502"))
            .env("OPENCODE_SESSION_ID", "ses_oc")
            .output()
            .unwrap()
    };

    fx.record_opencode_server();
    // pi recorded the session under its process's (the test's) directory.
    let cwd = std::env::current_dir().unwrap();
    let encoded = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    let sessions = fx.dir.join(format!(".pi/agent/sessions/--{encoded}--"));
    fs::create_dir_all(&sessions).unwrap();
    let header = serde_json::json!({"type": "session", "id": hosted, "cwd": cwd});
    fs::write(sessions.join("hosted.jsonl"), format!("{header}\n")).unwrap();
    assert!(
        as_pi_hook("session-start", serde_json::Value::Null)
            .status
            .success()
    );
    let rows = with_pi(&["agents", "--json", "--include-self"]);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&rows.stdout).unwrap();
    assert_eq!(row(&rows, "%31")["session_id"], hosted);
    assert_eq!(row(&rows, "%31")["agent_id"], hosted);

    let output = with_pi(&[
        "send",
        "--mode",
        "mailbox",
        "--to",
        "%31",
        "--message",
        "hi",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("queued for"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!fx.dir.join("pasted").exists());
    assert_eq!(mail(&fx, hosted, "pending").len(), 1);

    let printed = as_pi_hook("wait", serde_json::Value::Null);
    assert!(printed.status.success());
    let printed: serde_json::Value = serde_json::from_slice(&printed.stdout).unwrap();
    assert!(
        printed["text"].as_str().unwrap().contains("\nhi\n"),
        "{printed}"
    );
    assert!(as_pi_hook("ack", printed["ack"].clone()).status.success());
    assert_eq!(mail(&fx, hosted, "delivered").len(), 1);
}

#[test]
fn an_opencode_caller_is_pane_grade_when_another_session_shares_its_directory() {
    let fx = Fixture::new("octwin", OC_CALLER_AND_PI_TARGET);
    fx.record_opencode_server();
    let send = |session: &str| {
        fx.command(&["send", "--to", "%31", "--message", "hi pi"])
            .envs(OPENCODE_IN_PANE_30)
            .env("OPENCODE_SESSION_ID", session)
            .env(
                "EXTRA_OC_ROWS",
                r#",{"id":"ses_twin","directory":"/work/oc","time_updated":2000000000000}"#,
            )
            .output()
            .unwrap()
    };
    for session in ["ses_oc", "ses_twin"] {
        let output = send(session);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    for entry in audit(&fx) {
        assert_eq!(entry["from"], "opencode:%30");
        assert_eq!(entry["from_session"], serde_json::Value::Null);
    }
    assert_eq!(audit(&fx).len(), 2);
}

const ONE_PENDING: &str = r#"{"data":[{"id":"per_1","sessionID":"ses_oc","action":"shell","resources":["rm -rf build"],"save":["rm *"],"source":{"type":"tool","messageID":"msg_1","id":"call_1"}}]}"#;
const TWO_PENDING: &str = r#"{"data":[{"id":"per_1","action":"shell","resources":["rm -rf build"]},{"id":"per_2","action":"edit","resources":["/etc/hosts"]}]}"#;

fn answer_permissions(listing: &'static str) -> impl Fn(&str, &str) -> (u16, String) + Send {
    move |method, path| match (method, path.ends_with("/permission")) {
        ("GET", true) => (200, listing.to_string()),
        _ => (200, "{}".to_string()),
    }
}

fn replies(requests: &std::sync::mpsc::Receiver<Request>) -> Vec<(String, serde_json::Value)> {
    requests
        .try_iter()
        .filter(|r| r.method == "POST")
        .map(|r| (r.path, r.body))
        .collect()
}

#[test]
fn a_busy_opencode_session_waiting_on_a_permission_is_blocked() {
    let fx = Fixture::new(
        "ocblocked",
        r#"  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'"#,
    );
    let state = || {
        let output = fx
            .command(&["agents", "--json"])
            .env("OC_BUSY_AT", "30")
            .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        let rows: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        let oc = row(&rows, "%30");
        (oc["state"].clone(), oc["pending_tool"].clone())
    };
    // No server: the row is just busy.
    assert_eq!(state(), ("Working".into(), false.into()));

    let requests = fake_opencode_api(&fx, answer_permissions(ONE_PENDING));
    assert_eq!(state(), ("Blocked".into(), true.into()));
    let asked: Vec<String> = requests.try_iter().map(|r| r.path).collect();
    assert_eq!(asked, ["/api/session/ses_oc/permission"]);

    // An idle session is not asked.
    let rows = fx.agents();
    assert_ne!(row(&rows, "%30")["state"], "Blocked");
    assert!(requests.try_recv().is_err());
}

#[test]
fn approve_answers_the_pending_opencode_request_once_or_rejects_it() {
    let fx = Fixture::new("ocapprove", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(&fx, answer_permissions(ONE_PENDING));

    let output = fx.as_pi(&["approve", "--to", "%30", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("would approve %30"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(replies(&requests).is_empty());

    let output = fx.as_pi(&["approve", "--to", "%30"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let output = fx.as_pi(&["approve", "--to", "%30", "--deny"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let reply = "/api/session/ses_oc/permission/per_1/reply".to_string();
    assert_eq!(
        replies(&requests),
        [
            (reply.clone(), serde_json::json!({"decision": "once"})),
            (reply, serde_json::json!({"decision": "reject"})),
        ]
    );
    assert!(!fx.dir.join("keys").exists());

    let audit = audit(&fx);
    let verdicts: Vec<_> = audit.iter().map(|e| e["verdict"].clone()).collect();
    assert_eq!(verdicts, ["approved", "denied"]);
    assert_eq!(audit[0]["message_preview"], "shell: rm -rf build");
    assert_eq!(audit[0]["target_provider"], "oc");
}

#[test]
fn approve_refuses_an_opencode_row_without_exactly_one_named_request() {
    let fx = Fixture::new("ocambiguous", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(&fx, answer_permissions(TWO_PENDING));
    let output = fx.as_pi(&["approve", "--to", "%30"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("2 permission requests are pending"),
        "{}",
        stderr(&output)
    );
    assert!(replies(&requests).is_empty());
    assert_eq!(audit(&fx)[0]["verdict"], "approve-refused");

    let output = fx.as_pi(&["approve", "--to", "%30", "--request", "per_2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        replies(&requests),
        [(
            "/api/session/ses_oc/permission/per_2/reply".to_string(),
            serde_json::json!({"decision": "once"})
        )]
    );

    let fx = Fixture::new("ocnone", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(&fx, answer_permissions(r#"{"data":[]}"#));
    let output = fx.as_pi(&["approve", "--to", "%30"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stderr(&output).contains("no pending permission request"));
    assert!(replies(&requests).is_empty());
}

#[test]
fn approve_refuses_an_opencode_pane_not_matched_to_a_session() {
    // Two opencode panes in one directory: neither borrows the session.
    let fx = Fixture::new(
        "ocunmatched",
        r#"  echo 'pi|1.0|777000|/dev/null|pi|/work/pi|1|%31|pi'
  echo 'oc|1.0|501|/dev/null|opencode|/work/oc|1|%30|opencode'
  echo 'oc|2.0|503|/dev/null|opencode|/work/oc|1|%32|opencode'"#,
    );
    let requests = fake_opencode_api(&fx, answer_permissions(ONE_PENDING));
    let output = fx.as_pi(&["approve", "--to", "%30"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not matched to an opencode session"),
        "{}",
        stderr(&output)
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn pi_rows_can_be_neither_approved_nor_interrupted() {
    let fx = Fixture::new("pirefuse", OC_CALLER_AND_PI_TARGET);
    let output = fx.as_opencode(&["approve", "--to", "%31"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("pi has no tool-permission prompts"),
        "{}",
        stderr(&output)
    );
    let output = fx.run(&["interrupt", "--to", "%31"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not supported for pi"),
        "{}",
        stderr(&output)
    );
    assert!(!fx.dir.join("keys").exists());
}

/// A server whose session `ses_oc` went busy long ago (though a person
/// looked at it just now) and whose newest message is `newest`.
fn quiet_turn(newest: &'static str) -> impl Fn(&str, &str) -> (u16, String) + Send {
    move |method, path| {
        match (method, path) {
        ("GET", "/api/session/ses_oc") => (
            200,
            r#"{"data":{"id":"ses_oc","time":{"created":1,"updated":5,"busy":4,"idle":3,"viewed":9999999999999}}}"#
                .to_string(),
        ),
        ("GET", "/api/session/ses_oc/message?limit=1") => {
            (200, format!(r#"{{"data":[{newest}]}}"#))
        }
        ("GET", _) => (200, r#"{"data":[]}"#.to_string()),
        ("POST", "/api/session/ses_oc/interrupt") => (200, r#"{"interrupted":true}"#.to_string()),
        _ => (404, String::new()),
    }
    }
}

#[test]
fn interrupt_stops_a_quiet_opencode_turn_through_its_server() {
    let fx = Fixture::new("ocinterrupt", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(
        &fx,
        quiet_turn(
            r#"{"id":"msg_2","type":"assistant","time":{"created":5,"firstToken":6},"content":[{"type":"tool","name":"shell","state":{"status":"completed"},"time":{"created":6,"ran":7,"completed":8}}]}"#,
        ),
    );
    let output = fx
        .command(&["interrupt", "--to", "%30"])
        .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        replies(&requests),
        [(
            "/api/session/ses_oc/interrupt".to_string(),
            serde_json::json!({})
        )]
    );
    assert!(!fx.dir.join("keys").exists());
    assert_eq!(audit(&fx)[0]["verdict"], "interrupted");
}

#[test]
fn interrupt_leaves_an_opencode_turn_with_a_running_tool() {
    let fx = Fixture::new("ocbusytool", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(
        &fx,
        quiet_turn(
            r#"{"id":"msg_2","type":"assistant","time":{"created":5},"content":[{"type":"tool","name":"shell","state":{"status":"running"},"time":{"created":6,"ran":7}}]}"#,
        ),
    );
    let output = fx
        .command(&["interrupt", "--to", "%30"])
        .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unfinished tool call"),
        "{}",
        stderr(&output)
    );
    assert!(replies(&requests).is_empty());
    assert!(audit(&fx).is_empty());
}

#[test]
fn an_unanswered_opencode_approve_is_reported_uncertain_and_not_retried() {
    let fx = Fixture::new("ocapprovestall", PI_CALLER_AND_OC_TARGET);
    let requests = fake_opencode_api(&fx, |method, path| match method {
        "GET" => answer_permissions(ONE_PENDING)(method, path),
        _ => (STALL, String::new()),
    });
    let output = fx
        .command(&["approve", "--to", "%30"])
        .env("TRIAGE_TEST_PANE_HARNESS", pi_and_test_server())
        .env("PI_SESSION_ID", PI_SESSION)
        .env("PI_SESSION_FILE", "/unused")
        .env("TRIAGE_TEST_HTTP_TIMEOUT_MS", "300")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("may have taken effect, so it was not retried"),
        "{}",
        stderr(&output)
    );
    assert_eq!(replies(&requests).len(), 1);
    assert_eq!(audit(&fx)[0]["verdict"], "approved-unconfirmed");
}
