use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
                r#"{"type":"session","version":3,"id":"01a1-pi","timestamp":"2026-10-14T18:49:13.644Z","cwd":"/work/pi"}"#,
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
*"where id = 'ses_oc'"*) printf '[{"directory":"/work/oc"}]' ;;
*"where id ="*) printf '[{"directory":"/work/other"}]' ;;
*session_v2*) printf '[{"id":"ses_oc","directory":"/work/oc","title":"Fix the parser","time_created":1,"time_updated":2000000000000,"time_busy":10,"time_idle":20}]' ;;
esac
"#,
            ),
            (
                "ps",
                r#"
case "$*" in
*etime*) [ -n "$HARNESS_ARGS" ] && echo "  00:05 $HARNESS_ARGS" ;;
*tty*|*comm*) ;;
*) echo "$PPID 777001"; echo "777001 777000"; echo "777000 1" ;;
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
            .env_remove("TMUX_PANE")
            .env("CAPTURE", "")
            .env_remove("HARNESS_ARGS")
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
    assert_eq!(oc["agent_id"], serde_json::Value::Null);
    assert_ne!(oc["state"], "Working");

    let pi = row(&rows, "%31");
    assert_eq!(pi["provider"], "pi");
    assert_eq!(pi["name"], "pi-worker");
    assert_eq!(pi["cwd"], "/work/pi");
    assert_eq!(pi["session_id"], "01a1-pi");
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
        stderr(&inbox).contains("triage mail is not supported for pi agents yet"),
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
            .env("TRIAGE_TEST_PANE_HARNESS", "777001")
            .env("OPENCODE_SESSION_ID", session)
            .env("TMUX_PANE", "%30")
            .output()
            .unwrap()
    };

    let output = send("ses_oc");
    assert!(output.status.success(), "{}", stderr(&output));
    let pasted = fs::read_to_string(fx.dir.join("pasted")).unwrap();
    assert_eq!(pasted, "[triage message from opencode:%30] hi pi");
    let sent = audit(&fx);
    assert_eq!(sent[0]["from"], "opencode:%30");
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
