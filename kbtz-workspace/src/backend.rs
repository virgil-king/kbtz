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

    /// Build CLI args for a fresh session with a named session ID.
    ///
    /// Returns `Some(args)` if the backend supports named sessions (enabling
    /// future resume). Returns `None` to fall back to `worker_args` without
    /// session tracking.
    fn fresh_args(
        &self,
        _protocol_prompt: &str,
        _task_prompt: &str,
        _session_id: &str,
    ) -> Option<Vec<String>> {
        None
    }

    /// Build CLI args to resume a previous session by ID.
    ///
    /// Returns `Some(args)` if the backend supports session resume.
    /// Returns `None` if resume is not supported (always starts fresh).
    fn resume_args(&self, _protocol_prompt: &str, _session_id: &str) -> Option<Vec<String>> {
        None
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

    fn fresh_args(
        &self,
        protocol_prompt: &str,
        task_prompt: &str,
        session_id: &str,
    ) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 5 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--session-id".into(),
            session_id.into(),
            "--append-system-prompt".into(),
            protocol_prompt.into(),
            task_prompt.into(),
        ]);
        args.extend(self.extra_args.iter().cloned());
        Some(args)
    }

    fn resume_args(&self, protocol_prompt: &str, session_id: &str) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 4 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--resume".into(),
            session_id.into(),
            "--append-system-prompt".into(),
            protocol_prompt.into(),
        ]);
        args.extend(self.extra_args.iter().cloned());
        Some(args)
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

/// Generic backend for agent types without a named implementation.
///
/// Passes the protocol prompt and task prompt as positional args, uses
/// SIGTERM for graceful exit, and does not support session resume.
pub struct Generic {
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
}

impl Backend for Generic {
    fn command(&self) -> &str {
        &self.command
    }

    fn worker_args(&self, protocol_prompt: &str, task_prompt: &str) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 2 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([protocol_prompt.into(), task_prompt.into()]);
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
/// Named backends (e.g., "claude") get type-specific behavior. All other
/// names produce a generic backend that passes prompts as positional args.
///
/// The command override replaces the backend's default binary path.
/// Prefix args (from array-valued `command` config) are inserted before
/// kbtz-generated args. Extra args are appended after.
pub fn from_name(
    name: &str,
    command_override: Option<&str>,
    prefix_args: &[String],
    extra_args: &[String],
) -> Box<dyn Backend> {
    match name {
        "claude" => Box::new(Claude {
            command: command_override.unwrap_or("claude").to_string(),
            prefix_args: prefix_args.to_vec(),
            extra_args: extra_args.to_vec(),
        }),
        _ => Box::new(Generic {
            command: command_override.unwrap_or(name).to_string(),
            prefix_args: prefix_args.to_vec(),
            extra_args: extra_args.to_vec(),
        }),
    }
}

/// Create a generic backend using the agent type name as the command.
pub fn generic(name: &str) -> Box<dyn Backend> {
    Box::new(Generic {
        command: name.to_string(),
        prefix_args: vec![],
        extra_args: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_claude_default_command() {
        let backend = from_name("claude", None, &[], &[]);
        assert_eq!(backend.command(), "claude");
    }

    #[test]
    fn from_name_claude_command_override() {
        let backend = from_name("claude", Some("/usr/local/bin/claude"), &[], &[]);
        assert_eq!(backend.command(), "/usr/local/bin/claude");
    }

    #[test]
    fn from_name_unknown_creates_generic() {
        let backend = from_name("gemini", None, &[], &[]);
        assert_eq!(backend.command(), "gemini");
    }

    #[test]
    fn from_name_unknown_with_command_override() {
        let backend = from_name("gemini", Some("/usr/local/bin/gemini-cli"), &[], &[]);
        assert_eq!(backend.command(), "/usr/local/bin/gemini-cli");
    }

    #[test]
    fn generic_worker_args_passes_prompts_as_positional() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(args, vec!["protocol text", "task text"]);
    }

    #[test]
    fn generic_worker_args_with_prefix_and_extra() {
        let backend = Generic {
            command: "wrapper".into(),
            prefix_args: vec!["--flag".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend.worker_args("protocol text", "task text");
        assert_eq!(
            args,
            vec!["--flag", "protocol text", "task text", "--verbose"]
        );
    }

    #[test]
    fn generic_no_fresh_args() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        assert!(backend.fresh_args("proto", "task", "sess-1").is_none());
    }

    #[test]
    fn generic_no_resume_args() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        assert!(backend.resume_args("proto", "sess-1").is_none());
    }

    #[test]
    fn generic_factory_uses_name_as_command() {
        let backend = generic("custom-tool");
        assert_eq!(backend.command(), "custom-tool");
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
    fn claude_fresh_args_includes_session_id() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend
            .fresh_args("protocol text", "task text", "abc-123")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--session-id",
                "abc-123",
                "--append-system-prompt",
                "protocol text",
                "task text",
            ]
        );
    }

    #[test]
    fn claude_fresh_args_with_prefix_and_extra() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec!["--flag".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend
            .fresh_args("protocol text", "task text", "abc-123")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--flag",
                "--session-id",
                "abc-123",
                "--append-system-prompt",
                "protocol text",
                "task text",
                "--verbose",
            ]
        );
    }

    #[test]
    fn claude_resume_args_uses_resume_flag() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend.resume_args("protocol text", "abc-123").unwrap();
        assert_eq!(
            args,
            vec![
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "protocol text",
            ]
        );
    }

    #[test]
    fn claude_resume_args_with_prefix_and_extra() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec!["--flag".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend.resume_args("protocol text", "abc-123").unwrap();
        assert_eq!(
            args,
            vec![
                "--flag",
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "protocol text",
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
