use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn run_failure(tool: &str) {
    let dir = std::env::temp_dir().join(format!("triage-discovery-{}-{tool}", std::process::id()));
    fs::create_dir_all(dir.join(".codex")).unwrap();
    for (name, body) in [
        ("tmux", "exit 0".to_string()),
        ("ps", format!("echo '{} codex'", std::process::id())),
        ("lsof", "exit 0".to_string()),
        ("sqlite3", "exit 0".to_string()),
    ] {
        let body = if name == tool {
            "echo denied >&2; exit 1"
        } else {
            &body
        };
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    if tool == "sqlite3" {
        fs::write(dir.join(".codex/state_5.sqlite"), "").unwrap();
    }
    for args in [
        vec!["agents", "--json"],
        vec!["agents", "whoami"],
        vec!["send", "--to", "%1", "--message", "test", "--dry-run"],
        vec!["--probe"],
        vec!["interrupt", "--to", "%1", "--dry-run"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_triage"))
            .args(&args)
            .env("TMUX_PANE", "%fixture")
            .env("HOME", &dir)
            .env("PATH", &dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr.contains("Codex discovery unavailable"),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr.contains(tool) && stderr.contains("denied"),
            "{stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "must not report partial discovery as success"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lsof_failure_is_explicit() {
    run_failure("lsof");
}

#[test]
fn sqlite_failure_is_explicit() {
    run_failure("sqlite3");
}

#[test]
fn ps_failure_is_explicit() {
    run_failure("ps");
}
