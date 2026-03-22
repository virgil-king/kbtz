use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    #[serde(default)]
    pub agent: HashMap<String, AgentConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleConfig {
    pub before_start: Option<String>,
    pub after_end: Option<String>,
    /// Agent type for headless hook execution (references [agent.*]).
    /// Falls back to the workspace default when unset.
    pub agent: Option<String>,
    /// Agent type for interactive error-resolution sessions.
    /// Falls back to the workspace default when unset.
    pub resolution_agent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub concurrency: Option<usize>,
    pub manual: Option<bool>,
    pub prefer: Option<String>,
    pub backend: Option<String>,
    pub persistent_sessions: Option<bool>,
    pub workspace_dir: Option<String>,
    pub directory: Option<String>,
}

/// The `command` field in agent config: either a plain string or an array
/// whose first element is the binary and the rest are prefix args.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AgentCommand {
    Simple(String),
    WithPrefix(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Which Rust backend implementation to use (e.g., "claude").
    /// Defaults to the agent name. Allows multiple agent types to share
    /// the same backend with different command/args.
    pub backend: Option<String>,
    pub command: Option<AgentCommand>,
    #[serde(default)]
    pub args: Vec<String>,
}

impl AgentConfig {
    /// The binary to run, if configured.
    pub fn binary(&self) -> Option<&str> {
        match &self.command {
            Some(AgentCommand::Simple(s)) => Some(s),
            Some(AgentCommand::WithPrefix(v)) => v.first().map(|s| s.as_str()),
            None => None,
        }
    }

    /// Prefix args from an array-valued command (elements after the binary).
    pub fn prefix_args(&self) -> &[String] {
        match &self.command {
            Some(AgentCommand::WithPrefix(v)) if v.len() > 1 => &v[1..],
            _ => &[],
        }
    }
}

impl Config {
    /// Load config from `~/.kbtz/workspace.toml`.
    /// Returns default config if the file doesn't exist.
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let path = format!("{home}/.kbtz/workspace.toml");
        Self::load_from(Path::new(&path))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let config: Config = match std::fs::read_to_string(path) {
            Ok(contents) => toml::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        for (name, agent) in &self.agent {
            if let Some(AgentCommand::WithPrefix(v)) = &agent.command {
                if v.is_empty() {
                    bail!(
                        "failed to parse {}: agent.{name}.command array must have at least one element",
                        path.display()
                    );
                }
            }
        }
        Ok(())
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
workspace_dir = "/tmp/my-workspace"
directory = "/home/user/projects"

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
        assert_eq!(
            config.workspace.workspace_dir.as_deref(),
            Some("/tmp/my-workspace")
        );
        assert_eq!(
            config.workspace.directory.as_deref(),
            Some("/home/user/projects")
        );

        let claude = config.agent.get("claude").unwrap();
        assert_eq!(claude.binary(), Some("/usr/local/bin/claude"));
        assert!(claude.prefix_args().is_empty());
        assert_eq!(claude.args, vec!["--verbose"]);

        let gemini = config.agent.get("gemini").unwrap();
        assert_eq!(gemini.binary(), Some("gemini-cli"));
        assert!(gemini.prefix_args().is_empty());
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
        assert!(claude.binary().is_none());
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

    #[test]
    fn parse_array_command() {
        let toml = r#"
[agent.claude]
command = ["wrapper", "--flag", "claude"]
args = ["--verbose"]
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        let claude = config.agent.get("claude").unwrap();
        assert_eq!(claude.binary(), Some("wrapper"));
        assert_eq!(claude.prefix_args(), &["--flag", "claude"]);
        assert_eq!(claude.args, vec!["--verbose"]);
    }

    #[test]
    fn parse_single_element_array_command() {
        let toml = r#"
[agent.claude]
command = ["claude"]
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        let claude = config.agent.get("claude").unwrap();
        assert_eq!(claude.binary(), Some("claude"));
        assert!(claude.prefix_args().is_empty());
    }

    #[test]
    fn empty_array_command_rejected() {
        let toml = r#"
[agent.claude]
command = []
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let result = Config::load_from(f.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("at least one element"),
            "error should mention empty array: {err}"
        );
    }

    #[test]
    fn parse_backend_field() {
        let toml = r#"
[agent.claude-yolo]
backend = "claude"
command = "claude"
args = ["--dangerously-skip-permissions"]
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        let agent = config.agent.get("claude-yolo").unwrap();
        assert_eq!(agent.backend.as_deref(), Some("claude"));
        assert_eq!(agent.binary(), Some("claude"));
        assert_eq!(agent.args, vec!["--dangerously-skip-permissions"]);
    }

    #[test]
    fn backend_field_defaults_to_none() {
        let toml = r#"
[agent.gemini]
command = "gemini-cli"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        let agent = config.agent.get("gemini").unwrap();
        assert!(agent.backend.is_none());
    }

    #[test]
    fn parse_lifecycle_config() {
        let toml = r#"
[lifecycle]
before_start = "Set up worktree for task '{name}'."
after_end = "Tear down worktree for task '{name}'."
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert_eq!(
            config.lifecycle.before_start.as_deref(),
            Some("Set up worktree for task '{name}'.")
        );
        assert_eq!(
            config.lifecycle.after_end.as_deref(),
            Some("Tear down worktree for task '{name}'.")
        );
    }

    #[test]
    fn parse_lifecycle_with_workspace() {
        let toml = r#"
[workspace]
concurrency = 2

[lifecycle]
before_start = "Setup."

[agent.claude]
command = "claude"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert_eq!(config.workspace.concurrency, Some(2));
        assert_eq!(config.lifecycle.before_start.as_deref(), Some("Setup."));
        assert!(config.lifecycle.after_end.is_none());
        assert!(config.agent.contains_key("claude"));
    }

    #[test]
    fn parse_partial_lifecycle() {
        let toml = r#"
[lifecycle]
after_end = "Cleanup."
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert!(config.lifecycle.before_start.is_none());
        assert_eq!(config.lifecycle.after_end.as_deref(), Some("Cleanup."));
    }

    #[test]
    fn lifecycle_defaults_to_none() {
        let toml = r#"
[workspace]
concurrency = 1
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert!(config.lifecycle.before_start.is_none());
        assert!(config.lifecycle.after_end.is_none());
    }

    #[test]
    fn misspelled_lifecycle_field_rejected() {
        let toml = r#"
[lifecycle]
before_strat = "typo"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let result = Config::load_from(f.path());
        assert!(result.is_err());
    }

    #[test]
    fn parse_multiline_lifecycle_prompts() {
        let toml = r#"
[lifecycle]
before_start = """
Create a working directory for task '{name}'.
Acquire worktrees for ~/kbtz and ~/agents.
Return the directory path.
"""
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        let prompt = config.lifecycle.before_start.unwrap();
        assert!(prompt.contains("working directory"));
        assert!(prompt.contains("Acquire worktrees"));
    }

    #[test]
    fn parse_lifecycle_agent_fields() {
        let toml = r#"
[lifecycle]
before_start = "Setup."
agent = "claude"
resolution_agent = "gemini"
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert_eq!(config.lifecycle.agent.as_deref(), Some("claude"));
        assert_eq!(
            config.lifecycle.resolution_agent.as_deref(),
            Some("gemini")
        );
    }

    #[test]
    fn lifecycle_agent_fields_default_to_none() {
        let toml = r#"
[lifecycle]
before_start = "Setup."
"#;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml.as_bytes()).unwrap();

        let config = Config::load_from(f.path()).unwrap();
        assert!(config.lifecycle.agent.is_none());
        assert!(config.lifecycle.resolution_agent.is_none());
    }
}
