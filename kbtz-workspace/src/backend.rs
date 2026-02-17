use anyhow::{bail, Result};

use crate::session::SessionHandle;

/// Defines how kbtz-workspace interacts with a specific coding agent tool.
///
/// Each backend encapsulates the agent-specific details: the binary to run,
/// how to inject protocol instructions and task prompts via CLI args, and
/// how to request a graceful exit.
///
/// Implementations must call `session.mark_stopping()` in `request_exit`
/// after sending the backend-specific exit signal, so the lifecycle tick
/// can enforce the force-kill timeout.
pub trait Backend: Send + Sync {
    /// The command binary to run (e.g., "claude", "codex").
    fn command(&self) -> &str;

    /// Build CLI args for a worker session.
    ///
    /// `protocol_prompt`: kbtz task protocol instructions (from prompt.rs)
    /// `task_prompt`: the task-specific prompt (e.g., "Work on task 'foo': ...")
    fn worker_args(&self, protocol_prompt: &str, task_prompt: &str) -> Vec<String>;

    /// Build CLI args for the toplevel task management session.
    ///
    /// Defaults to `worker_args`. Override if the backend needs different
    /// arg structure for toplevel vs worker sessions.
    fn toplevel_args(&self, protocol_prompt: &str, task_prompt: &str) -> Vec<String> {
        self.worker_args(protocol_prompt, task_prompt)
    }

    /// Request graceful exit from the agent process.
    ///
    /// Implementations must call `session.mark_stopping()` after sending
    /// the exit signal so the lifecycle tick can track the timeout.
    fn request_exit(&self, session: &mut dyn SessionHandle);
}

/// Claude Code backend. Injects prompts via `--append-system-prompt` and
/// exits via SIGTERM.
pub struct Claude {
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
}

impl Backend for Claude {
    fn command(&self) -> &str {
        &self.command
    }

    fn worker_args(&self, protocol_prompt: &str, task_prompt: &str) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 3 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--append-system-prompt".into(),
            protocol_prompt.into(),
            task_prompt.into(),
        ]);
        args.extend(self.extra_args.iter().cloned());
        args
    }

    fn request_exit(&self, session: &mut dyn SessionHandle) {
        if session.stopping_since().is_some() {
            return;
        }
        if let Some(pid) = session.process_id() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        session.mark_stopping();
    }
}

/// Create a backend by name, with an optional command override, prefix args,
/// and extra args.
///
/// The command override replaces the backend's default binary path
/// (e.g., `--command /usr/local/bin/claude` with `--backend claude`).
/// Prefix args (from array-valued `command` config) are inserted before
/// kbtz-generated args. Extra args are appended after.
pub fn from_name(
    name: &str,
    command_override: Option<&str>,
    prefix_args: &[String],
    extra_args: &[String],
) -> Result<Box<dyn Backend>> {
    match name {
        "claude" => Ok(Box::new(Claude {
            command: command_override.unwrap_or("claude").to_string(),
            prefix_args: prefix_args.to_vec(),
            extra_args: extra_args.to_vec(),
        })),
        _ => bail!("unknown backend '{name}'; available backends: claude"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_claude_default_command() {
        let backend = from_name("claude", None, &[], &[]).unwrap();
        assert_eq!(backend.command(), "claude");
    }

    #[test]
    fn from_name_claude_command_override() {
        let backend = from_name("claude", Some("/usr/local/bin/claude"), &[], &[]).unwrap();
        assert_eq!(backend.command(), "/usr/local/bin/claude");
    }

    #[test]
    fn from_name_unknown_backend_fails() {
        let result = from_name("nonexistent", None, &[], &[]);
        let err = result
            .err()
            .expect("should fail for unknown backend")
            .to_string();
        assert!(err.contains("nonexistent"), "error should name the backend");
        assert!(
            err.contains("claude"),
            "error should list available backends"
        );
    }

    #[test]
    fn claude_worker_args_structure() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(
            args,
            vec!["--append-system-prompt", "protocol text", "task text"]
        );
    }

    #[test]
    fn claude_worker_args_with_extra_args() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec!["--verbose".into(), "--model".into(), "opus".into()],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(
            args,
            vec![
                "--append-system-prompt",
                "protocol text",
                "task text",
                "--verbose",
                "--model",
                "opus",
            ]
        );
    }

    #[test]
    fn claude_worker_args_with_prefix_args() {
        let backend = Claude {
            command: "wrapper".into(),
            prefix_args: vec!["--flag".into(), "claude".into()],
            extra_args: vec![],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(
            args,
            vec![
                "--flag",
                "claude",
                "--append-system-prompt",
                "protocol text",
                "task text",
            ]
        );
    }

    #[test]
    fn claude_worker_args_with_prefix_and_extra_args() {
        let backend = Claude {
            command: "wrapper".into(),
            prefix_args: vec!["--".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(
            args,
            vec![
                "--",
                "--append-system-prompt",
                "protocol text",
                "task text",
                "--verbose",
            ]
        );
    }

    #[test]
    fn claude_toplevel_args_delegates_to_worker() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let worker = backend.worker_args("proto", "task");
        let toplevel = backend.toplevel_args("proto", "task");
        assert_eq!(worker, toplevel);
    }
}
