//! Centralized path resolution and session ID encoding for the kbtz system.
//!
//! Session IDs are system-generated strings of the form `ws/{N}` (e.g. `ws/3`).
//! Status files on disk use `-` in place of `/` (e.g. `ws-3`). The encoding
//! functions here are the single source of truth for this convention.

/// Resolve the kbtz database path.
/// Checks `KBTZ_DB` env var, falls back to `$HOME/.kbtz/kbtz.db`.
pub fn db_path() -> String {
    std::env::var("KBTZ_DB").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.kbtz/kbtz.db")
    })
}

/// Resolve the workspace status directory.
/// Checks `KBTZ_WORKSPACE_DIR` env var, falls back to `$HOME/.kbtz/workspace`.
pub fn workspace_dir() -> String {
    std::env::var("KBTZ_WORKSPACE_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.kbtz/workspace")
    })
}

/// Convert a session ID to a status filename.
/// `ws/3` → `ws-3`
pub fn session_id_to_filename(session_id: &str) -> String {
    session_id.replace('/', "-")
}

/// The prefix used for workspace session IDs (e.g. `ws/3`).
pub const SESSION_ID_PREFIX: &str = "ws/";

/// Returns true if `filename` (with or without an extension like `.sock`)
/// looks like a session status file produced by [`session_id_to_filename`].
pub fn is_session_filename(filename: &str) -> bool {
    let stem = filename.split_once('.').map_or(filename, |(s, _)| s);
    filename_to_session_id(stem).starts_with(SESSION_ID_PREFIX)
}

/// Convert a status filename back to a session ID.
/// `ws-3` → `ws/3`
///
/// Only the first `-` is replaced, preserving any literal hyphens that might
/// appear later in the ID (though current IDs are always `ws/{N}`).
pub fn filename_to_session_id(filename: &str) -> String {
    match filename.find('-') {
        Some(pos) => {
            let (prefix, rest) = filename.split_at(pos);
            format!("{prefix}/{}", &rest[1..])
        }
        None => filename.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let sid = "ws/3";
        let filename = session_id_to_filename(sid);
        assert_eq!(filename, "ws-3");
        assert_eq!(filename_to_session_id(&filename), sid);
    }

    #[test]
    fn roundtrip_zero() {
        let sid = "ws/0";
        let filename = session_id_to_filename(sid);
        assert_eq!(filename, "ws-0");
        assert_eq!(filename_to_session_id(&filename), sid);
    }

    #[test]
    fn roundtrip_large_number() {
        let sid = "ws/42";
        let filename = session_id_to_filename(sid);
        assert_eq!(filename, "ws-42");
        assert_eq!(filename_to_session_id(&filename), sid);
    }

    #[test]
    fn no_separator_unchanged() {
        assert_eq!(filename_to_session_id("plain"), "plain");
    }

    #[test]
    fn only_first_dash_replaced() {
        // If a filename has multiple dashes, only the first becomes /
        assert_eq!(filename_to_session_id("ws-foo-bar"), "ws/foo-bar");
    }

    #[test]
    fn is_session_filename_matches_status_files() {
        assert!(is_session_filename("ws-0"));
        assert!(is_session_filename("ws-42"));
        assert!(is_session_filename("ws-toplevel"));
    }

    #[test]
    fn is_session_filename_matches_with_extensions() {
        assert!(is_session_filename("ws-0.sock"));
        assert!(is_session_filename("ws-1.pid"));
        assert!(is_session_filename("ws-2.child-pid"));
    }

    #[test]
    fn is_session_filename_rejects_non_session_files() {
        assert!(!is_session_filename("kbtz.db"));
        assert!(!is_session_filename("kbtz.db-wal"));
        assert!(!is_session_filename("workspace.lock"));
        assert!(!is_session_filename("orchestrator.log"));
        assert!(!is_session_filename("something-else.txt"));
        assert!(!is_session_filename("plain"));
    }
}
