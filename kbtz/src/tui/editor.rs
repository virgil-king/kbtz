use std::io::{self, Write as _};
use std::process::Command;

use anyhow::{bail, Context, Result};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;

/// Suspends the TUI, opens `$EDITOR` with `initial_content` in a temp file,
/// waits for the editor to exit, reads the result, and restores the TUI.
pub fn open_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    initial_content: &str,
) -> Result<String> {
    let editor = std::env::var("EDITOR").context("$EDITOR is not set")?;

    let mut tmp = tempfile::Builder::new()
        .prefix("kbtz-")
        .suffix(".md")
        .tempfile()
        .context("failed to create temp file")?;
    tmp.write_all(initial_content.as_bytes())
        .context("failed to write to temp file")?;
    tmp.flush()?;

    let path = tmp.path().to_path_buf();

    // Suspend TUI
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    let status_result = Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("failed to run editor '{editor}'"));

    // Restore TUI (always, even if the editor failed to launch)
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;
    terminal.clear()?;

    let status = status_result?;
    if !status.success() {
        bail!("editor exited with status {status}");
    }

    let content = std::fs::read_to_string(&path)
        .context("failed to read temp file after editor closed")?;

    Ok(content)
}
