mod lifecycle;
mod orchestrator;
mod tmux;

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use log::info;

use crate::orchestrator::Orchestrator;

#[derive(Parser)]
#[command(name = "kbtz-tmux", about = "Tmux-based workspace orchestrator")]
struct Cli {
    /// Maximum concurrent agent sessions
    #[arg(long, default_value_t = 4)]
    max: usize,

    /// FTS preference text for task selection
    #[arg(long)]
    prefer: Option<String>,

    /// Fallback poll interval in seconds (safety net; normal operation is event-driven)
    #[arg(long, default_value_t = 60)]
    poll: u64,

    /// Tmux session name
    #[arg(long, default_value = "workspace")]
    session: String,
}

fn acquire_lock(workspace_dir: &str) -> Result<fs::File> {
    let lock_path = format!("{workspace_dir}/orchestrator.lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .context("failed to open lock file")?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            bail!("Orchestrator already running");
        }
        return Err(err).context("flock failed");
    }

    Ok(file)
}

fn setup_logging(workspace_dir: &str) -> Result<()> {
    let log_path = format!("{workspace_dir}/orchestrator.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .context("failed to open log file")?;

    env_logger::Builder::new()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .filter_level(log::LevelFilter::Info)
        .format_timestamp_secs()
        .init();

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let workspace_dir = std::env::var("KBTZ_WORKSPACE_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{home}/.kbtz/workspace")
    });
    fs::create_dir_all(&workspace_dir)?;

    let _lock = acquire_lock(&workspace_dir)?;
    setup_logging(&workspace_dir)?;

    info!(
        "Starting (max={}, poll={}s, session={})",
        cli.max, cli.poll, cli.session
    );

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .context("failed to set signal handler")?;

    let mut orch = Orchestrator::new(
        cli.session,
        cli.max,
        Duration::from_secs(cli.poll),
        cli.prefer,
        running,
    )?;

    orch.run()?;
    orch.shutdown();

    Ok(())
}
