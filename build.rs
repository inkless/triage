// Auto-build the macOS notification helper `triage-notify.app` whenever
// `cargo build` runs on macOS. Best-effort: if `swiftc` isn't installed
// (no Xcode CLI tools) or the build fails, we print a warning but don't
// fail the parent build — `notify_os` falls through to `osascript` for
// display-only notifications in that case.
//
// After build, we also stage the bundle into `$HOME/.config/triage/`. The
// workspace path is fine for dev (`target/release/triage`) but the
// cargo-installed binary at `~/.cargo/bin/triage` can't reach back into
// the workspace at runtime — without the staged copy it'd fall through to
// osascript and show macOS's default "Show" button (which routes to
// Script Editor, not the user's terminal). Writing into `$HOME` from a
// build script is unusual; it's acceptable here because triage is a
// personal-use tool and the staged copy is what makes `cargo install`
// produce a working binary.

use std::path::{Path, PathBuf};
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
    let built_app = Path::new("scripts/triage-notify/triage-notify.app");
    match Command::new("bash").arg(script).status() {
        Ok(s) if s.success() => {
            stage_to_user_config(built_app);
        }
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

fn stage_to_user_config(built_app: &Path) {
    let Some(home) = std::env::var_os("HOME") else {
        println!("cargo:warning=HOME unset; skipping notify-helper stage");
        return;
    };
    let dest_dir: PathBuf = PathBuf::from(home).join(".config/triage");
    let dest_app = dest_dir.join("triage-notify.app");

    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        println!("cargo:warning=could not create {}: {e}", dest_dir.display());
        return;
    }
    // Wipe stale copy first — handles main.swift renames and avoids `cp -R`'s
    // "copy into destination" behavior when the target already exists.
    if dest_app.exists()
        && let Err(e) = std::fs::remove_dir_all(&dest_app)
    {
        println!("cargo:warning=could not remove stale {}: {e}", dest_app.display());
        return;
    }
    let status = Command::new("cp")
        .arg("-R")
        .arg(built_app)
        .arg(&dest_app)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => println!(
            "cargo:warning=cp -R {} {} exited {}",
            built_app.display(),
            dest_app.display(),
            s
        ),
        Err(e) => println!(
            "cargo:warning=failed to stage triage-notify.app to {}: {e}",
            dest_app.display()
        ),
    }
}
