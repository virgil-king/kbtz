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
}
