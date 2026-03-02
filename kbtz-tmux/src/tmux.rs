use std::collections::HashMap;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// List all window IDs in the given tmux session.
pub fn list_window_ids(session: &str) -> Result<Vec<String>> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_id}"])
        .output()
        .context("failed to run tmux list-windows")?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// Get a tmux window option value (e.g., @kbtz_task).
pub fn get_window_option(window_id: &str, option: &str) -> Result<Option<String>> {
    let output = Command::new("tmux")
        .args(["show-window-option", "-t", window_id, "-v", option])
        .output()
        .context("failed to run tmux show-window-option")?;
    if !output.status.success() {
        return Ok(None);
    }
    let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if val.is_empty() {
        Ok(None)
    } else {
        Ok(Some(val))
    }
}

/// Set a tmux window option (e.g., @kbtz_task = "my-task").
pub fn set_window_option(window_id: &str, option: &str, value: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["set-window-option", "-t", window_id, option, value])
        .status()
        .context("failed to run tmux set-window-option")?;
    if !status.success() {
        bail!("tmux set-window-option failed for {window_id}");
    }
    Ok(())
}

/// Check if a tmux session exists.
pub fn has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a new tmux session with the given name and initial window.
pub fn create_session(
    name: &str,
    window_name: &str,
    command: &str,
    args: &[&str],
) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", name, "-n", window_name, "--"]);
    cmd.arg(command);
    cmd.args(args);

    let output = cmd.output().context("failed to run tmux new-session")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux new-session failed: {stderr}");
    }
    Ok(())
}

/// Set a tmux session-level option.
pub fn set_session_option(session: &str, option: &str, value: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["set-option", "-t", session, option, value])
        .status()
        .context("failed to run tmux set-option")?;
    if !status.success() {
        bail!("tmux set-option {option} failed for session {session}");
    }
    Ok(())
}

/// Apply session-level tmux settings for the workspace.
pub fn configure_session(session: &str) -> Result<()> {
    let settings = [
        ("automatic-rename", "off"),
        ("allow-rename", "off"),
        ("remain-on-exit", "off"),
        ("mouse", "on"),
        ("status-interval", "1"),
    ];
    for (opt, val) in settings {
        set_session_option(session, opt, val)?;
    }
    Ok(())
}

/// Bind a key to a run-shell command (global, not session-scoped).
pub fn bind_key(key: &str, command: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["bind-key", "-T", "prefix", key, "run-shell", command])
        .status()
        .context("failed to run tmux bind-key")?;
    if !status.success() {
        bail!("tmux bind-key {key} failed");
    }
    Ok(())
}

/// Spawn a new tmux window running the given command with environment variables.
/// Returns the new window ID.
pub fn spawn_window(
    session: &str,
    name: &str,
    env: &HashMap<String, String>,
    command: &str,
    args: &[String],
) -> Result<String> {
    let mut cmd = Command::new("tmux");
    cmd.args([
        "new-window",
        "-d",
        "-P",
        "-F",
        "#{window_id}",
        "-t",
        session,
        "-n",
        name,
    ]);
    for (key, val) in env {
        cmd.args(["-e", &format!("{key}={val}")]);
    }
    cmd.arg("--");
    cmd.arg(command);
    cmd.args(args);

    let output = cmd.output().context("failed to run tmux new-window")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux new-window failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Get the PID of the process running in a window's pane.
pub fn pane_pid(window_id: &str) -> Result<Option<u32>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", window_id, "-F", "#{pane_pid}"])
        .output()
        .context("failed to run tmux list-panes")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(text.parse::<u32>().ok())
}

/// Kill a tmux window.
pub fn kill_window(window_id: &str) -> Result<()> {
    let _ = Command::new("tmux")
        .args(["kill-window", "-t", window_id])
        .status();
    Ok(())
}

/// Install a tmux hook that touches a sentinel file when any pane exits.
/// This gives us event-driven dead-window detection instead of polling.
pub fn install_pane_hook(session: &str, sentinel_path: &str) -> Result<()> {
    let hook_cmd = format!("run-shell 'touch {sentinel_path}'");
    let status = Command::new("tmux")
        .args(["set-hook", "-t", session, "pane-exited", &hook_cmd])
        .status()
        .context("failed to install pane-exited hook")?;
    if !status.success() {
        bail!("tmux set-hook pane-exited failed");
    }
    Ok(())
}

/// Remove the pane-exited hook (for cleanup on shutdown).
pub fn remove_pane_hook(session: &str) -> Result<()> {
    let _ = Command::new("tmux")
        .args(["set-hook", "-u", "-t", session, "pane-exited"])
        .status();
    Ok(())
}
