use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use serde::Deserialize;

/// JSON schema passed to `claude --json-schema` for structured hook output.
///
/// Both fields are optional: `directory` is set on success (before_start),
/// `error` is set on failure.
const HOOK_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "directory": { "type": "string" },
    "error": { "type": "string" }
  },
  "additionalProperties": false
}"#;

/// System prompt appended to the headless Claude session so it understands
/// the structured output contract.
const HOOK_SYSTEM_PROMPT: &str = "\
You are running as a headless lifecycle hook for kbtz-workspace. \
Your output MUST conform to the JSON schema provided via --json-schema. \
On success, return {\"directory\": \"/path/to/working/dir\"}. \
On failure, return {\"error\": \"description of what went wrong\"}. \
Never return both fields. Execute the task described in the user prompt.";

/// Result of a successful hook execution.
#[derive(Debug, Clone)]
pub struct HookResult {
    pub directory: Option<PathBuf>,
}

/// The structured_output portion of the Claude JSON envelope.
#[derive(Debug, Deserialize)]
struct StructuredOutput {
    directory: Option<String>,
    error: Option<String>,
}

/// Top-level JSON envelope returned by `claude -p --output-format json`.
#[derive(Debug, Deserialize)]
struct ClaudeJsonEnvelope {
    structured_output: Option<StructuredOutput>,
}

/// Error from hook execution.
#[derive(Debug, Clone)]
pub struct HookError {
    pub message: String,
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HookError {}

/// Expand template variables in a hook prompt.
///
/// Supported variables: `{name}`, `{description}`, `{directory}`.
pub fn expand_template(
    template: &str,
    name: &str,
    description: &str,
    directory: Option<&str>,
) -> String {
    let mut result = template.replace("{name}", name);
    result = result.replace("{description}", description);
    if let Some(dir) = directory {
        result = result.replace("{directory}", dir);
    }
    result
}

/// Run a headless Claude session synchronously and parse the structured output.
///
/// `command` is the Claude binary path (e.g. "claude").
/// `prefix_args` are inserted before kbtz-generated args (from array-valued command config).
/// `extra_args` are appended after (from agent config).
/// `expanded_prompt` is the template with variables already substituted.
fn run_hook_sync(
    command: &str,
    prefix_args: &[String],
    extra_args: &[String],
    expanded_prompt: &str,
) -> Result<HookResult, HookError> {
    let mut cmd = Command::new(command);
    cmd.args(prefix_args);
    cmd.args([
        "-p",
        "--output-format",
        "json",
        "--json-schema",
        HOOK_SCHEMA,
        "--append-system-prompt",
        HOOK_SYSTEM_PROMPT,
        expanded_prompt,
    ]);
    cmd.args(extra_args);

    let output = cmd.output().map_err(|e| HookError {
        message: format!("failed to spawn hook process: {e}"),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(HookError {
            message: format!(
                "hook process exited with {}: {}",
                output.status,
                stderr.trim()
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: ClaudeJsonEnvelope = serde_json::from_str(&stdout).map_err(|e| HookError {
        message: format!("failed to parse hook JSON output: {e}"),
    })?;

    let structured = envelope.structured_output.ok_or_else(|| HookError {
        message: "hook output missing structured_output field".into(),
    })?;

    if let Some(error) = structured.error {
        return Err(HookError { message: error });
    }

    Ok(HookResult {
        directory: structured.directory.map(PathBuf::from),
    })
}

/// Handle to a hook running on a background thread.
///
/// The caller polls `try_recv()` from the main loop to check for completion
/// without blocking the UI.
pub struct HookHandle {
    receiver: mpsc::Receiver<Result<HookResult, HookError>>,
}

impl HookHandle {
    /// Non-blocking check for hook completion.
    ///
    /// Returns `None` if the hook is still running, `Some(result)` when done.
    /// Returns `Some(Err)` if the background thread panicked.
    pub fn try_recv(&self) -> Option<Result<HookResult, HookError>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(HookError {
                message: "hook thread panicked".into(),
            })),
        }
    }
}

/// Spawn a headless hook on a background thread.
///
/// Returns a `HookHandle` that can be polled from the main loop.
pub fn spawn_hook(
    command: String,
    prefix_args: Vec<String>,
    extra_args: Vec<String>,
    expanded_prompt: String,
) -> HookHandle {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = run_hook_sync(&command, &prefix_args, &extra_args, &expanded_prompt);
        let _ = sender.send(result);
    });
    HookHandle { receiver }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Template expansion ────────────────────────────────────────────

    #[test]
    fn expand_all_variables() {
        let template = "Setup task '{name}'. Desc: {description}. Dir: {directory}.";
        let result = expand_template(template, "my-task", "do stuff", Some("/tmp/work"));
        assert_eq!(result, "Setup task 'my-task'. Desc: do stuff. Dir: /tmp/work.");
    }

    #[test]
    fn expand_without_directory() {
        let template = "Task '{name}': {description}";
        let result = expand_template(template, "foo", "bar", None);
        assert_eq!(result, "Task 'foo': bar");
    }

    #[test]
    fn expand_no_variables() {
        let template = "No variables here.";
        let result = expand_template(template, "name", "desc", Some("/dir"));
        assert_eq!(result, "No variables here.");
    }

    #[test]
    fn expand_repeated_variables() {
        let template = "{name} and {name} again";
        let result = expand_template(template, "task1", "", None);
        assert_eq!(result, "task1 and task1 again");
    }

    #[test]
    fn expand_preserves_directory_placeholder_when_none() {
        let template = "Dir is {directory}";
        let result = expand_template(template, "t", "d", None);
        assert_eq!(result, "Dir is {directory}");
    }

    // ── JSON parsing ─────────────────────────────────────────────────

    #[test]
    fn parse_success_with_directory() {
        let json = r#"{"structured_output": {"directory": "/tmp/work"}}"#;
        let envelope: ClaudeJsonEnvelope = serde_json::from_str(json).unwrap();
        let so = envelope.structured_output.unwrap();
        assert_eq!(so.directory.as_deref(), Some("/tmp/work"));
        assert!(so.error.is_none());
    }

    #[test]
    fn parse_success_empty_object() {
        let json = r#"{"structured_output": {}}"#;
        let envelope: ClaudeJsonEnvelope = serde_json::from_str(json).unwrap();
        let so = envelope.structured_output.unwrap();
        assert!(so.directory.is_none());
        assert!(so.error.is_none());
    }

    #[test]
    fn parse_error_response() {
        let json = r#"{"structured_output": {"error": "disk full"}}"#;
        let envelope: ClaudeJsonEnvelope = serde_json::from_str(json).unwrap();
        let so = envelope.structured_output.unwrap();
        assert!(so.directory.is_none());
        assert_eq!(so.error.as_deref(), Some("disk full"));
    }

    #[test]
    fn parse_envelope_with_extra_fields() {
        let json = r#"{"model": "claude-4", "cost_usd": 0.01, "structured_output": {"directory": "/x"}}"#;
        let envelope: ClaudeJsonEnvelope = serde_json::from_str(json).unwrap();
        let so = envelope.structured_output.unwrap();
        assert_eq!(so.directory.as_deref(), Some("/x"));
    }

    #[test]
    fn parse_missing_structured_output() {
        let json = r#"{"model": "claude-4"}"#;
        let envelope: ClaudeJsonEnvelope = serde_json::from_str(json).unwrap();
        assert!(envelope.structured_output.is_none());
    }

    // ── run_hook_sync with a mock command ────────────────────────────
    //
    // run_hook_sync passes CLI args (-p, --output-format, etc.) to the
    // command, so we use `sh -c 'echo JSON'` to produce controlled stdout
    // regardless of the args run_hook_sync adds.

    /// Build args to simulate a command that prints fixed JSON output.
    fn mock_cmd(json: &str) -> (&str, Vec<String>) {
        ("sh", vec!["-c".into(), format!("echo '{json}'")])
    }

    #[test]
    fn hook_sync_success_with_directory() {
        let (cmd, prefix) = mock_cmd(r#"{"structured_output": {"directory": "/tmp/test-dir"}}"#);
        let result = run_hook_sync(cmd, &prefix, &[], "");
        let hook_result = result.unwrap();
        assert_eq!(
            hook_result.directory,
            Some(PathBuf::from("/tmp/test-dir"))
        );
    }

    #[test]
    fn hook_sync_error_from_structured_output() {
        let (cmd, prefix) = mock_cmd(r#"{"structured_output": {"error": "worktree locked"}}"#);
        let result = run_hook_sync(cmd, &prefix, &[], "");
        let err = result.unwrap_err();
        assert_eq!(err.message, "worktree locked");
    }

    #[test]
    fn hook_sync_missing_structured_output() {
        let (cmd, prefix) = mock_cmd(r#"{"model": "claude-4"}"#);
        let result = run_hook_sync(cmd, &prefix, &[], "");
        let err = result.unwrap_err();
        assert!(err.message.contains("missing structured_output"));
    }

    #[test]
    fn hook_sync_invalid_json() {
        let (cmd, prefix) = mock_cmd("not json at all");
        let result = run_hook_sync(cmd, &prefix, &[], "");
        let err = result.unwrap_err();
        assert!(err.message.contains("failed to parse"));
    }

    #[test]
    fn hook_sync_command_not_found() {
        let result = run_hook_sync(
            "nonexistent-binary-that-does-not-exist-29481",
            &[],
            &[],
            "test",
        );
        let err = result.unwrap_err();
        assert!(err.message.contains("failed to spawn"));
    }

    #[test]
    fn hook_sync_nonzero_exit() {
        let result = run_hook_sync("false", &[], &[], "");
        let err = result.unwrap_err();
        assert!(err.message.contains("exited with"));
    }

    #[test]
    fn hook_sync_success_no_directory() {
        let (cmd, prefix) = mock_cmd(r#"{"structured_output": {}}"#);
        let result = run_hook_sync(cmd, &prefix, &[], "");
        let hook_result = result.unwrap();
        assert!(hook_result.directory.is_none());
    }

    // ── HookHandle (async spawn) ────────────────────────────────────

    #[test]
    fn hook_handle_receives_result() {
        let json = r#"{"structured_output": {"directory": "/async/dir"}}"#;
        let handle = spawn_hook(
            "sh".into(),
            vec!["-c".into(), format!("echo '{json}'")],
            vec![],
            String::new(),
        );

        // Poll until complete (should be nearly instant for echo)
        let result = loop {
            if let Some(r) = handle.try_recv() {
                break r;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        };

        let hook_result = result.unwrap();
        assert_eq!(
            hook_result.directory,
            Some(PathBuf::from("/async/dir"))
        );
    }

    #[test]
    fn hook_handle_receives_error() {
        let handle = spawn_hook(
            "nonexistent-binary-that-does-not-exist-29481".into(),
            vec![],
            vec![],
            "test".into(),
        );

        let result = loop {
            if let Some(r) = handle.try_recv() {
                break r;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        };

        assert!(result.is_err());
    }

    #[test]
    fn hook_handle_try_recv_returns_none_before_completion() {
        // Use 'sleep' to create a hook that takes a moment
        let handle = spawn_hook(
            "sleep".into(),
            vec![],
            vec![],
            "1".into(),
        );
        // Immediately check — should not be done yet
        assert!(handle.try_recv().is_none());
    }

    // ── Schema validation ────────────────────────────────────────────

    #[test]
    fn hook_schema_is_valid_json() {
        let parsed: serde_json::Value = serde_json::from_str(HOOK_SCHEMA).unwrap();
        assert_eq!(parsed["type"], "object");
        assert!(parsed["properties"]["directory"].is_object());
        assert!(parsed["properties"]["error"].is_object());
    }
}
