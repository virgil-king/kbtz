use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

/// Creates a watcher for the database file and returns a receiver for change events.
/// The watcher must be kept alive for events to be received.
pub fn watch_db(db_path: &str) -> Result<(RecommendedWatcher, Receiver<()>)> {
    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if res.is_ok() {
            // Ignore send errors (receiver dropped)
            let _ = tx.send(());
        }
    })
    .context("failed to create file watcher")?;

    // Watch the parent directory since SQLite uses temp files during writes
    let path = Path::new(db_path);
    let watch_path = path.parent().unwrap_or(path);
    watcher
        .watch(watch_path, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", watch_path.display()))?;

    Ok((watcher, rx))
}

/// Waits for a database change event with timeout.
/// Returns true if an event was received, false on timeout.
pub fn wait_for_change(rx: &Receiver<()>, timeout: Duration) -> bool {
    rx.recv_timeout(timeout).is_ok()
}

/// Drains any pending events from the receiver.
pub fn drain_events(rx: &Receiver<()>) {
    while rx.try_recv().is_ok() {}
}
