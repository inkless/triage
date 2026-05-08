// Auto-build the macOS notification helper `triage-notify.app` whenever
// `cargo build` runs on macOS. Best-effort: if `swiftc` isn't installed
// (no Xcode CLI tools) or the build fails, we print a warning but don't
// fail the parent build — `notify_os` falls through to `osascript` for
// display-only notifications in that case.
//
// `notify_os::triage_notify_path` looks for the resulting bundle relative
// to the running binary at runtime; the workspace path
// `scripts/triage-notify/triage-notify.app/...` is the primary candidate.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=scripts/triage-notify/main.swift");
    println!("cargo:rerun-if-changed=scripts/triage-notify/Info.plist");
    println!("cargo:rerun-if-changed=scripts/triage-notify/build.sh");

    if !cfg!(target_os = "macos") {
        return;
    }
    let script = Path::new("scripts/triage-notify/build.sh");
    if !script.exists() {
        return;
    }
    match Command::new("bash").arg(script).status() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            println!(
                "cargo:warning=triage-notify build exited {} — falling back to osascript-only notifications",
                s
            );
        }
        Err(e) => {
            println!(
                "cargo:warning=triage-notify build skipped: {} — install Xcode CLI tools (`xcode-select --install`) for click-to-jump notifications",
                e
            );
        }
    }
}
