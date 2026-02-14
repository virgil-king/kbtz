use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Creates a watcher for the database file and returns a receiver for change events.
/// The watcher must be kept alive for events to be received.
///
/// We watch the parent directory (since SQLite uses temp files like -wal and -shm
/// alongside the main database file), but filter events to only those affecting
/// files whose name starts with the database filename.
pub fn watch_db(db_path: &str) -> Result<(RecommendedWatcher, Receiver<()>)> {
    let (tx, rx) = mpsc::channel();

    let db_filename = Path::new(db_path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Ignore access events (open/close/read) — these fire from any
            // process reading the database and can cause cascading wakes.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }

            // Only react to events on the database file or its auxiliaries
            // (-wal, -shm, -journal). Watching the parent directory means we
            // see events for every file in it; ignore unrelated files.
            let dominated = event.paths.iter().any(|p| {
                p.file_name()
                    .map(|f| f.to_string_lossy().starts_with(&*db_filename))
                    .unwrap_or(false)
            });
            if dominated {
                let _ = tx.send(());
            }
        }
    })
    .context("failed to create file watcher")?;

    let path = Path::new(db_path);
    let watch_path = path.parent().unwrap_or(path);
    watcher
        .watch(watch_path, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", watch_path.display()))?;

    Ok((watcher, rx))
}

/// Creates a watcher for a directory and returns a receiver for change events.
pub fn watch_dir(dir: &Path) -> Result<(RecommendedWatcher, Receiver<()>)> {
    let (tx, rx) = mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            let _ = tx.send(());
        }
    })
    .context("failed to create file watcher")?;

    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", dir.display()))?;

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
