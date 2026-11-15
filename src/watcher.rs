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

/// On Linux, inotify reports opening and closing a file or directory, so a
/// reader that wakes on every event wakes itself each time it looks.
pub fn is_change(event: &notify::Event) -> bool {
    !matches!(event.kind, notify::EventKind::Access(_))
}

impl FsWatcher {
    pub fn spawn() -> notify::Result<Self> {
        let (tx, rx): (Sender<()>, Receiver<()>) = mpsc::channel();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if res.as_ref().is_ok_and(is_change) {
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

#[cfg(test)]
mod tests {
    use super::is_change;
    use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, ModifyKind};

    #[test]
    fn opening_or_reading_is_not_a_change() {
        let event = |kind| notify::Event::new(kind);
        assert!(!is_change(&event(EventKind::Access(AccessKind::Open(
            AccessMode::Any
        )))));
        assert!(!is_change(&event(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        )))));
        assert!(is_change(&event(EventKind::Create(CreateKind::File))));
        assert!(is_change(&event(EventKind::Modify(ModifyKind::Any))));
    }
}
