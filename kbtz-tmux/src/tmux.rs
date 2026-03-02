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
    let output = Command::new("tmux")
        .args(["set-window-option", "-t", window_id, option, value])
        .output()
        .context("failed to run tmux set-window-option")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set-window-option failed for {window_id}: {stderr}");
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
    let output = Command::new("tmux")
        .args(["set-option", "-t", session, option, value])
        .output()
        .context("failed to run tmux set-option")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set-option {option} failed for session {session}: {stderr}");
    }
    Ok(())
}

/// Set a tmux window option as the session default for new windows.
pub fn set_window_option_default(session: &str, option: &str, value: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["set-option", "-w", "-t", session, option, value])
        .output()
        .context("failed to run tmux set-option -w")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set-option -w {option} failed for session {session}: {stderr}");
    }
    Ok(())
}

/// Apply tmux settings for the workspace session.
pub fn configure_session(session: &str) -> Result<()> {
    // Session options.
    set_session_option(session, "status-interval", "1")?;

    // Window option defaults (apply to all new windows in this session).
    set_window_option_default(session, "automatic-rename", "off")?;
    set_window_option_default(session, "allow-rename", "off")?;
    set_window_option_default(session, "remain-on-exit", "off")?;

    // Also set on the existing window 0 (kbtz watch) since it was
    // created before these defaults were applied.
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_id}"])
        .output()
        .context("failed to list windows")?;
    if output.status.success() {
        for wid in String::from_utf8_lossy(&output.stdout).lines() {
            let wid = wid.trim();
            if !wid.is_empty() {
                let _ = set_window_option(wid, "automatic-rename", "off");
                let _ = set_window_option(wid, "allow-rename", "off");
            }
        }
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
        .output(); // capture stderr to avoid leaking to terminal
    Ok(())
}

/// Install a tmux hook that touches a sentinel file when any pane exits.
/// This gives us event-driven dead-window detection instead of polling.
pub fn install_pane_hook(session: &str, sentinel_path: &str) -> Result<()> {
    let hook_cmd = format!("run-shell 'touch {sentinel_path}'");
    let output = Command::new("tmux")
        .args(["set-hook", "-t", session, "pane-exited", &hook_cmd])
        .output()
        .context("failed to install pane-exited hook")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set-hook pane-exited failed: {stderr}");
    }
    Ok(())
}

/// Remove the pane-exited hook (for cleanup on shutdown).
pub fn remove_pane_hook(session: &str) -> Result<()> {
    let _ = Command::new("tmux")
        .args(["set-hook", "-u", "-t", session, "pane-exited"])
        .output(); // capture stderr to avoid leaking to terminal
    Ok(())
}
