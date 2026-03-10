use crate::session::SessionHandle;

/// Defines how kbtz-workspace interacts with a specific coding agent tool.
///
/// Each backend encapsulates the agent-specific details: the binary to run,
/// how to inject system instructions and the initial prompt via CLI args,
/// and how to request a graceful exit.
///
/// Implementations must call `session.mark_stopping()` in `request_exit`
/// after sending the backend-specific exit signal, so the lifecycle tick
/// can enforce the force-kill timeout.
pub trait Backend: Send + Sync {
    /// The command binary to run (e.g., "claude", "codex").
    fn command(&self) -> &str;

    /// Build CLI args for a worker session.
    ///
    /// `system_instructions`: kbtz task protocol (from prompt.rs), injected
    ///     as persistent system-level context where the backend supports it.
    /// `initial_prompt`: the task-specific prompt (e.g., "Work on task 'foo': ...")
    ///     that becomes the first user message.
    fn worker_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String>;

    /// Build CLI args for the toplevel task management session.
    ///
    /// Defaults to `worker_args`. Override if the backend needs different
    /// arg structure for toplevel vs worker sessions.
    fn toplevel_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String> {
        self.worker_args(system_instructions, initial_prompt)
    }

    /// Build CLI args for a fresh session with a named session ID.
    ///
    /// Returns `Some(args)` if the backend supports named sessions (enabling
    /// future resume). Returns `None` to fall back to `worker_args` without
    /// session tracking.
    fn fresh_args(
        &self,
        _system_instructions: &str,
        _initial_prompt: &str,
        _session_id: &str,
    ) -> Option<Vec<String>> {
        None
    }

    /// Build CLI args to resume a previous session by ID.
    ///
    /// Returns `Some(args)` if the backend supports session resume.
    /// Returns `None` if resume is not supported (always starts fresh).
    fn resume_args(&self, _system_instructions: &str, _session_id: &str) -> Option<Vec<String>> {
        None
    }

    /// Request graceful exit from the agent process.
    ///
    /// Implementations must call `session.mark_stopping()` after sending
    /// the exit signal so the lifecycle tick can track the timeout.
    fn request_exit(&self, session: &mut dyn SessionHandle);
}

/// Claude Code backend. Injects system instructions via
/// `--append-system-prompt` and exits via SIGTERM.
pub struct Claude {
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
}

impl Backend for Claude {
    fn command(&self) -> &str {
        &self.command
    }

    fn worker_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 3 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--append-system-prompt".into(),
            system_instructions.into(),
            initial_prompt.into(),
        ]);
        args.extend(self.extra_args.iter().cloned());
        args
    }

    fn fresh_args(
        &self,
        system_instructions: &str,
        initial_prompt: &str,
        session_id: &str,
    ) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 5 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--session-id".into(),
            session_id.into(),
            "--append-system-prompt".into(),
            system_instructions.into(),
            initial_prompt.into(),
        ]);
        args.extend(self.extra_args.iter().cloned());
        Some(args)
    }

    fn resume_args(&self, system_instructions: &str, session_id: &str) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 4 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--resume".into(),
            session_id.into(),
            "--append-system-prompt".into(),
            system_instructions.into(),
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
/// Concatenates system instructions and the initial prompt into a single
/// positional arg, since most coding CLIs only accept one prompt input
/// and have no separate system prompt mechanism. Uses SIGTERM for graceful
/// exit and does not support session resume.
pub struct Generic {
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
}

impl Backend for Generic {
    fn command(&self) -> &str {
        &self.command
    }

    fn worker_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 1 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.push(format!("{system_instructions}\n\n{initial_prompt}"));
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
/// Named backends (e.g., "claude") get type-specific behavior like separate
/// system prompt injection. All other names produce a generic backend that
/// concatenates system instructions and initial prompt into a single arg.
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
    fn generic_worker_args_concatenates_instructions_and_prompt() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend.worker_args("system text", "task text");
        assert_eq!(args, vec!["system text\n\ntask text"]);
    }

    #[test]
    fn generic_worker_args_with_prefix_and_extra() {
        let backend = Generic {
            command: "wrapper".into(),
            prefix_args: vec!["--flag".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend.worker_args("system text", "task text");
        assert_eq!(
            args,
            vec!["--flag", "system text\n\ntask text", "--verbose"]
        );
    }

    #[test]
    fn generic_no_fresh_args() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        assert!(backend.fresh_args("sys", "task", "sess-1").is_none());
    }

    #[test]
    fn generic_no_resume_args() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        assert!(backend.resume_args("sys", "sess-1").is_none());
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
        let args = backend.worker_args("system text", "task text");
        assert_eq!(
            args,
            vec!["--append-system-prompt", "system text", "task text"]
        );
    }

    #[test]
    fn claude_worker_args_with_extra_args() {
        let backend = Claude {
            command: "claude".into(),
            prefix_args: vec![],
            extra_args: vec!["--verbose".into(), "--model".into(), "opus".into()],
        };
        let args = backend.worker_args("system text", "task text");
        assert_eq!(
            args,
            vec![
                "--append-system-prompt",
                "system text",
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
        let args = backend.worker_args("system text", "task text");
        assert_eq!(
            args,
            vec![
                "--flag",
                "claude",
                "--append-system-prompt",
                "system text",
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
        let args = backend.worker_args("system text", "task text");
        assert_eq!(
            args,
            vec![
                "--",
                "--append-system-prompt",
                "system text",
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
            .fresh_args("system text", "task text", "abc-123")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--session-id",
                "abc-123",
                "--append-system-prompt",
                "system text",
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
            .fresh_args("system text", "task text", "abc-123")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--flag",
                "--session-id",
                "abc-123",
                "--append-system-prompt",
                "system text",
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
        let args = backend.resume_args("system text", "abc-123").unwrap();
        assert_eq!(
            args,
            vec![
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "system text",
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
        let args = backend.resume_args("system text", "abc-123").unwrap();
        assert_eq!(
            args,
            vec![
                "--flag",
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "system text",
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
        let worker = backend.worker_args("sys", "task");
        let toplevel = backend.toplevel_args("sys", "task");
        assert_eq!(worker, toplevel);
    }
}
