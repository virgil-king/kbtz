use crate::session::{SessionHandle, SessionStatus};

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
    fn worker_args(
        &self,
        system_instructions: &str,
        initial_prompt: &str,
        session_profile: Option<&str>,
    ) -> Vec<String>;

    /// Build CLI args for the toplevel task management session.
    ///
    /// Defaults to `worker_args`. Override if the backend needs different
    /// arg structure for toplevel vs worker sessions.
    fn toplevel_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String> {
        self.worker_args(system_instructions, initial_prompt, None)
    }

    /// Return the initial status for backends without lifecycle hooks.
    ///
    /// Claude's plugin hooks replace this status as soon as the session
    /// starts, while hookless backends need a useful steady state.
    fn initial_status(&self) -> Option<SessionStatus> {
        None
    }

    /// Whether the backend writes its generated session ID through a
    /// session-start hook instead of accepting a caller-provided ID.
    fn captures_session_id_on_start(&self) -> bool {
        false
    }

    /// Return an inline Codex profile that captures a generated session ID.
    fn session_start_profile(&self) -> Option<String> {
        None
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
    /// `initial_prompt` is sent as the first user message in the resumed
    /// session so the agent has a message to process instead of waiting
    /// for interactive input.
    ///
    /// Returns `Some(args)` if the backend supports session resume.
    /// Returns `None` if resume is not supported (always starts fresh).
    fn resume_args(
        &self,
        _system_instructions: &str,
        _session_id: &str,
        _initial_prompt: &str,
    ) -> Option<Vec<String>> {
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

    fn worker_args(
        &self,
        system_instructions: &str,
        initial_prompt: &str,
        _session_profile: Option<&str>,
    ) -> Vec<String> {
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

    fn resume_args(
        &self,
        system_instructions: &str,
        session_id: &str,
        initial_prompt: &str,
    ) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 5 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.extend([
            "--resume".into(),
            session_id.into(),
            "--append-system-prompt".into(),
            system_instructions.into(),
            initial_prompt.into(),
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

/// Codex backend launched through `airchat codex`.
///
/// The protocol and task prompt are passed as Codex's initial positional
/// prompt. A session-start hook captures Codex's generated session ID, which
/// is passed to `codex resume` on relaunch.
pub struct Codex {
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
}

impl Backend for Codex {
    fn command(&self) -> &str {
        &self.command
    }

    fn worker_args(
        &self,
        system_instructions: &str,
        initial_prompt: &str,
        session_profile: Option<&str>,
    ) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + self.extra_args.len() + 6);
        args.extend(self.prefix_args.iter().cloned());
        if !args.iter().any(|arg| arg == "--") {
            args.push("--".into());
        }
        args.extend(self.extra_args.iter().cloned());
        if let Some(profile) = session_profile {
            args.extend([
                "--dangerously-bypass-hook-trust".into(),
                "--profile".into(),
                profile.into(),
            ]);
        }
        args.push(format!("{system_instructions}\n\n{initial_prompt}"));
        args
    }

    fn toplevel_args(&self, system_instructions: &str, initial_prompt: &str) -> Vec<String> {
        self.worker_args(system_instructions, initial_prompt, None)
    }

    fn resume_args(
        &self,
        system_instructions: &str,
        session_id: &str,
        initial_prompt: &str,
    ) -> Option<Vec<String>> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + self.extra_args.len() + 4);
        args.extend(self.prefix_args.iter().cloned());
        if !args.iter().any(|arg| arg == "--") {
            args.push("--".into());
        }
        args.extend(self.extra_args.iter().cloned());
        args.extend(["resume".into(), session_id.into()]);
        args.push(format!("{system_instructions}\n\n{initial_prompt}"));
        Some(args)
    }

    fn initial_status(&self) -> Option<SessionStatus> {
        Some(SessionStatus::Active)
    }

    fn captures_session_id_on_start(&self) -> bool {
        true
    }

    fn session_start_profile(&self) -> Option<String> {
        let command =
            serde_json::to_string(&codex_hook_command()).expect("hook command is valid JSON");
        Some(format!(
            "[[hooks.SessionStart]]\nmatcher = \"^startup$\"\n\n\
             [[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = {command}\n"
        ))
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

fn codex_hook_command() -> String {
    let executable = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "kbtz-workspace".to_string());
    format!("{} --codex-session-hook", shell_quote(&executable))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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

    fn worker_args(
        &self,
        system_instructions: &str,
        initial_prompt: &str,
        _session_profile: Option<&str>,
    ) -> Vec<String> {
        let mut args = Vec::with_capacity(self.prefix_args.len() + 1 + self.extra_args.len());
        args.extend(self.prefix_args.iter().cloned());
        args.push(format!("{system_instructions}\n\n{initial_prompt}"));
        args.extend(self.extra_args.iter().cloned());
        args
    }

    fn initial_status(&self) -> Option<SessionStatus> {
        Some(SessionStatus::Active)
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
/// Named backends (e.g., "claude" and "codex") get type-specific behavior.
/// All other names produce a generic backend that concatenates system
/// instructions and initial prompt into a single arg.
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
        "codex" => Box::new(Codex {
            command: command_override.unwrap_or("airchat").to_string(),
            prefix_args: if prefix_args.is_empty() {
                vec!["codex".to_string()]
            } else {
                prefix_args.to_vec()
            },
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
    fn from_name_codex_defaults_to_airchat_codex() {
        let backend = from_name("codex", None, &[], &[]);
        assert_eq!(backend.command(), "airchat");
        let args = backend.worker_args("system text", "task text", Some("kbtz-test"));
        assert_eq!(
            args,
            [
                "codex",
                "--",
                "--dangerously-bypass-hook-trust",
                "--profile",
                "kbtz-test",
                "system text\n\ntask text",
            ]
        );
        let profile = backend.session_start_profile().unwrap();
        assert!(profile.contains("[[hooks.SessionStart]]"));
        assert!(profile.contains("--codex-session-hook"));
    }

    #[test]
    fn from_name_codex_preserves_configured_command_parts() {
        let prefix = vec!["codex".into(), "-a".into(), "on-request".into()];
        let extra = vec!["--model".into(), "gpt-5".into()];
        let backend = from_name("codex", Some("airchat"), &prefix, &extra);
        assert_eq!(backend.command(), "airchat");
        let args = backend.worker_args("system text", "task text", Some("kbtz-test"));
        assert_eq!(
            &args[..8],
            [
                "codex",
                "-a",
                "on-request",
                "--",
                "--model",
                "gpt-5",
                "--dangerously-bypass-hook-trust",
                "--profile",
            ]
        );
        assert_eq!(args[8], "kbtz-test");
        assert_eq!(args[9], "system text\n\ntask text");
    }

    #[test]
    fn from_name_codex_reuses_configured_airchat_separator() {
        let prefix = vec![
            "codex".into(),
            "-a".into(),
            "on-request".into(),
            "--".into(),
        ];
        let backend = from_name("codex", Some("airchat"), &prefix, &[]);
        let args = backend.worker_args("system text", "task text", Some("kbtz-test"));
        assert_eq!(
            args,
            [
                "codex",
                "-a",
                "on-request",
                "--",
                "--dangerously-bypass-hook-trust",
                "--profile",
                "kbtz-test",
                "system text\n\ntask text",
            ]
        );
    }

    #[test]
    fn codex_resume_reuses_configured_airchat_separator() {
        let prefix = vec!["codex".into(), "--".into()];
        let backend = from_name("codex", Some("airchat"), &prefix, &[]);
        let args = backend
            .resume_args("system text", "thread-id", "resume text")
            .unwrap();
        assert_eq!(
            args,
            [
                "codex",
                "--",
                "resume",
                "thread-id",
                "system text\n\nresume text",
            ]
        );
    }

    #[test]
    fn generic_worker_args_concatenates_instructions_and_prompt() {
        let backend = Generic {
            command: "my-agent".into(),
            prefix_args: vec![],
            extra_args: vec![],
        };
        let args = backend.worker_args("system text", "task text", None);
        assert_eq!(args, vec!["system text\n\ntask text"]);
    }

    #[test]
    fn generic_worker_args_with_prefix_and_extra() {
        let backend = Generic {
            command: "wrapper".into(),
            prefix_args: vec!["--flag".into()],
            extra_args: vec!["--verbose".into()],
        };
        let args = backend.worker_args("system text", "task text", None);
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
        assert!(backend.resume_args("sys", "sess-1", "continue").is_none());
    }

    #[test]
    fn codex_passes_protocol_and_task_prompt_positionally() {
        let backend = from_name("codex", None, &[], &[]);
        assert_eq!(
            backend.toplevel_args("system text", "task text"),
            vec!["codex", "--", "system text\n\ntask text"]
        );
    }

    #[test]
    fn codex_captures_generated_session_id_and_resumes_it() {
        let backend = from_name("codex", None, &[], &[]);
        assert!(backend.captures_session_id_on_start());
        assert!(backend.fresh_args("sys", "task", "sess-1").is_none());
        assert_eq!(
            backend.resume_args("sys", "sess-1", "continue"),
            Some(vec![
                "codex".into(),
                "--".into(),
                "resume".into(),
                "sess-1".into(),
                "sys\n\ncontinue".into(),
            ])
        );
    }

    #[test]
    fn codex_resume_args_keep_configured_flags() {
        let prefix = vec!["codex".into(), "--approval-policy".into(), "never".into()];
        let extra = vec!["--reasoning-effort".into(), "high".into()];
        let backend = from_name("codex", Some("airchat"), &prefix, &extra);
        assert_eq!(
            backend.resume_args("sys", "sess-1", "continue"),
            Some(vec![
                "codex".into(),
                "--approval-policy".into(),
                "never".into(),
                "--".into(),
                "--reasoning-effort".into(),
                "high".into(),
                "resume".into(),
                "sess-1".into(),
                "sys\n\ncontinue".into(),
            ])
        );
    }

    #[test]
    fn codex_command_override_keeps_codex_subcommand() {
        let backend = from_name("codex", Some("/path/to/airchat"), &[], &[]);
        assert_eq!(backend.command(), "/path/to/airchat");
        assert_eq!(
            backend.toplevel_args("system text", "task text"),
            vec!["codex", "--", "system text\n\ntask text"]
        );
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("/tmp/a'b"), "'/tmp/a'\"'\"'b'");
    }

    #[test]
    fn codex_starts_active_without_claude_hooks() {
        let backend = from_name("codex", None, &[], &[]);
        assert_eq!(backend.initial_status(), Some(SessionStatus::Active));
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
        let args = backend.worker_args("system text", "task text", None);
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
        let args = backend.worker_args("system text", "task text", None);
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
        let args = backend.worker_args("system text", "task text", None);
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
        let args = backend.worker_args("system text", "task text", None);
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
        let args = backend
            .resume_args("system text", "abc-123", "continue task")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "system text",
                "continue task",
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
        let args = backend
            .resume_args("system text", "abc-123", "continue task")
            .unwrap();
        assert_eq!(
            args,
            vec![
                "--flag",
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "system text",
                "continue task",
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
        let worker = backend.worker_args("sys", "task", None);
        let toplevel = backend.toplevel_args("sys", "task");
        assert_eq!(worker, toplevel);
    }
}
