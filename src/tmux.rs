use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::models::Pane;

/// Returns a map of pane_pid → Pane.
pub fn list_panes() -> HashMap<u32, Pane> {
    let mut map = HashMap::new();
    let out = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{window_index}.#{pane_index}|#{pane_pid}|#{pane_tty}|#{pane_current_command}|#{pane_current_path}|#{?pane_active,1,0}",
        ])
        .output();
    let Ok(out) = out else { return map };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(7, '|').collect();
        if parts.len() < 7 {
            continue;
        }
        let Ok(pid) = parts[2].parse::<u32>() else {
            continue;
        };
        // pane_active=1 marks the most-recently-focused pane within each
        // tmux session, even when that session isn't currently attached.
        // That makes it usable as a "which pane was the user last in for
        // this cwd?" signal regardless of where they're typing now.
        let active = parts[6] == "1";
        map.insert(
            pid,
            Pane {
                target: format!("{}:{}", parts[0], parts[1]),
                pid,
                tty: parts[3].to_string(),
                current_command: parts[4].to_string(),
                cwd: PathBuf::from(parts[5]),
                active,
            },
        );
    }
    map
}

/// One-shot snapshot of pid → ppid for the whole system. Consumers walk the
/// map in-process instead of calling `ps` per hop. With up to 8 hops × N
/// sessions per refresh the per-call cost was the dominant lag source.
pub fn build_ppid_map() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some(ppid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else { continue };
        map.insert(pid, ppid);
    }
    map
}

/// Walk parent PIDs upward (max `max_hops`) until we find one in `pane_pids`.
pub fn find_owning_pane(
    pid: u32,
    pane_pids: &HashMap<u32, Pane>,
    ppid_map: &HashMap<u32, u32>,
    max_hops: usize,
) -> Option<Pane> {
    let mut cur = pid;
    for _ in 0..max_hops {
        let ppid = *ppid_map.get(&cur)?;
        if ppid <= 1 {
            return None;
        }
        if let Some(pane) = pane_pids.get(&ppid) {
            return Some(pane.clone());
        }
        cur = ppid;
    }
    None
}

/// Switch tmux focus to the given pane target (`session:window.pane`).
pub fn jump_to(target: &str) -> std::io::Result<()> {
    // `select-pane -t session:window.pane` handles session + window + pane,
    // but `switch-client` is required to actually focus the session if the
    // user is currently attached to a different one.
    let session = target.split_once(':').map(|(s, _)| s).unwrap_or("");
    if !session.is_empty() {
        Command::new("tmux")
            .args(["switch-client", "-t", session])
            .status()?;
    }
    Command::new("tmux")
        .args(["select-pane", "-t", target])
        .status()?;
    Ok(())
}
