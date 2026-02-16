use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub agent: HashMap<String, AgentConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub concurrency: Option<usize>,
    pub manual: Option<bool>,
    pub prefer: Option<String>,
    pub backend: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Config {
    /// Load config from `~/.kbtz/workspace.toml`.
    /// Returns default config if the file doesn't exist.
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let path = format!("{home}/.kbtz/workspace.toml");
        Self::load_from(Path::new(&path))
    }

    fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                toml::from_str(&contents).with_context(|| format!("failed to parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn missing_file_returns_default() {
        let config = Config::load_from(Path::new("/nonexistent/workspace.toml")).unwrap();
        assert!(config.workspace.concurrency.is_none());
        assert!(config.workspace.backend.is_none());
        assert!(config.agent.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[workspace]
concurrency = 3
manual = true
prefer = "frontend"
backend = "claude"

[agent.claude]
command = "/usr/local/bin/claude"
args = ["--verbose"]

[agent.gemini]
command = "gemini-cli"
args = ["--model", "gemini-2.5-pro"]
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert_eq!(config.workspace.concurrency, Some(3));
        assert_eq!(config.workspace.manual, Some(true));
        assert_eq!(config.workspace.prefer.as_deref(), Some("frontend"));
        assert_eq!(config.workspace.backend.as_deref(), Some("claude"));

        let claude = config.agent.get("claude").unwrap();
        assert_eq!(claude.command.as_deref(), Some("/usr/local/bin/claude"));
        assert_eq!(claude.args, vec!["--verbose"]);

        let gemini = config.agent.get("gemini").unwrap();
        assert_eq!(gemini.command.as_deref(), Some("gemini-cli"));
        assert_eq!(gemini.args, vec!["--model", "gemini-2.5-pro"]);
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"
[workspace]
concurrency = 2
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert_eq!(config.workspace.concurrency, Some(2));
        assert!(config.workspace.backend.is_none());
        assert!(config.agent.is_empty());
    }

    #[test]
    fn parse_agent_without_workspace() {
        let toml = r#"
[agent.claude]
args = ["--flag"]
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert!(config.workspace.concurrency.is_none());
        let claude = config.agent.get("claude").unwrap();
        assert!(claude.command.is_none());
        assert_eq!(claude.args, vec!["--flag"]);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"not valid toml [[[").unwrap();

        let result = Config::load_from(f.path());
        assert!(result.is_err());
    }

    #[test]
    fn empty_file_returns_default() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let config = Config::load_from(f.path()).unwrap();
        assert!(config.workspace.concurrency.is_none());
        assert!(config.agent.is_empty());
    }

    #[test]
    fn misspelled_workspace_field_rejected() {
        let toml = r#"
[workspace]
concurency = 3
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let result = Config::load_from(f.path());
        assert!(result.is_err());
    }

    #[test]
    fn misspelled_agent_field_rejected() {
        let toml = r#"
[agent.claude]
commnd = "claude"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let result = Config::load_from(f.path());
        assert!(result.is_err());
    }
}
