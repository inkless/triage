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
capture-pane) printf '\n' ;;
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
*session_v2*) printf '[{"id":"ses_oc","directory":"/work/oc","title":"Fix the parser","time_created":1,"time_updated":2000000000000,"time_busy":10,"time_idle":20}]' ;;
esac
"#,
            ),
            ("ps", ""),
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
