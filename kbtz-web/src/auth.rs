use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::Rng;
use subtle::ConstantTimeEq;

const TOKEN_LENGTH: usize = 32;

/// Load an existing token or create a new one at the given path.
/// The token file is created with mode 0600 (owner read/write only).
pub fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(contents) = fs::read_to_string(path) {
        let token = contents.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let token = generate_token();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating token file {}", path.display()))?;
    file.write_all(token.as_bytes())
        .with_context(|| format!("writing token to {}", path.display()))?;
    Ok(token)
}

/// Default path for the web auth token.
pub fn default_token_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".kbtz").join("web-token")
}

fn generate_token() -> String {
    let mut rng = rand::rng();
    let bytes: Vec<u8> = (0..TOKEN_LENGTH).map(|_| rng.random()).collect();
    bytes.iter().fold(String::with_capacity(TOKEN_LENGTH * 2), |mut s, b| {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// Verify a candidate token against the stored token using constant-time comparison.
pub fn verify_token(stored: &str, candidate: &str) -> bool {
    let stored_bytes = stored.as_bytes();
    let candidate_bytes = candidate.as_bytes();
    if stored_bytes.len() != candidate_bytes.len() {
        return false;
    }
    stored_bytes.ct_eq(candidate_bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_create_creates_new_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let token = load_or_create_token(&path).unwrap();
        assert_eq!(token.len(), TOKEN_LENGTH * 2); // hex encoding doubles length
        assert!(path.exists());
    }

    #[test]
    fn load_or_create_reuses_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let token1 = load_or_create_token(&path).unwrap();
        let token2 = load_or_create_token(&path).unwrap();
        assert_eq!(token1, token2);
    }

    #[test]
    fn verify_correct_token() {
        assert!(verify_token("abc123", "abc123"));
    }

    #[test]
    fn verify_wrong_token() {
        assert!(!verify_token("abc123", "abc124"));
    }

    #[test]
    fn verify_different_lengths() {
        assert!(!verify_token("abc", "abcd"));
    }
}
