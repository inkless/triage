use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::discovery::{projects_dir, sessions_dir};

/// A coalescing watcher: emits a single tick whenever anything under sessions/ or projects/
/// changes. The receiver should treat the tick as "go re-read state."
pub struct FsWatcher {
    pub rx: Receiver<()>,
    _watcher: RecommendedWatcher,
}

impl FsWatcher {
    pub fn spawn() -> notify::Result<Self> {
        let (tx, rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if res.is_ok() {
                    let _ = tx.send(());
                }
            })?;
        let sessions = sessions_dir();
        let projects = projects_dir();
        let codex_sessions = crate::codex::sessions_dir();
        if sessions.exists() {
            watcher.watch(&sessions, RecursiveMode::NonRecursive)?;
        }
        if projects.exists() {
            watcher.watch(&projects, RecursiveMode::Recursive)?;
        }
        if codex_sessions.exists() {
            watcher.watch(&codex_sessions, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            rx,
            _watcher: watcher,
        })
    }

    /// Drain accumulated events; returns true if at least one was waiting.
    pub fn drain(&self) -> bool {
        let mut got = false;
        while self.rx.recv_timeout(Duration::from_millis(0)).is_ok() {
            got = true;
        }
        got
    }
}
