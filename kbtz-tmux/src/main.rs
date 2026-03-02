mod orchestrator;

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use log::info;

use kbtz::paths;
use kbtz_tmux::tmux;
use kbtz_workspace::config::Config;
use kbtz_workspace::prompt::TOPLEVEL_PROMPT;

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
    #[arg(long, default_value = "workspace", env = "KBTZ_TMUX_SESSION")]
    session: String,

    /// Run orchestrator directly (no session bootstrap)
    #[arg(long)]
    no_attach: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Jump to the next window whose agent needs input
    JumpNeedsInput {
        #[arg(long, default_value = "workspace", env = "KBTZ_TMUX_SESSION")]
        session: String,
    },
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

fn check_tmux() -> Result<()> {
    let output = Command::new("tmux")
        .arg("-V")
        .output()
        .context("tmux is not installed or not on PATH")?;
    if !output.status.success() {
        bail!("tmux -V failed; is tmux installed?");
    }
    Ok(())
}

fn spawn_manager_window(session: &str, config: &Config) -> Result<()> {
    let backend_name = config.workspace.backend.as_deref().unwrap_or("claude");
    let agent_cfg = config.agent.get(backend_name);
    let binary = agent_cfg
        .and_then(|a| a.binary())
        .unwrap_or("claude")
        .to_string();

    let db_path = paths::db_path();

    let mut args: Vec<String> = Vec::new();
    if let Some(cfg) = agent_cfg {
        args.extend(cfg.prefix_args().iter().map(|s| s.to_string()));
    }
    args.extend([
        "--append-system-prompt".into(),
        TOPLEVEL_PROMPT.into(),
        "You are the task manager. Help the user organize work.".into(),
    ]);
    if let Some(cfg) = agent_cfg {
        args.extend(cfg.args.iter().cloned());
    }

    let mut env = HashMap::new();
    env.insert("KBTZ_DB".into(), db_path);

    let window_id = tmux::spawn_window(session, "manager", &env, &binary, &args)?;
    tmux::set_window_option(&window_id, "@kbtz_toplevel", "true")?;
    Ok(())
}

fn bootstrap(cli: &Cli) -> Result<()> {
    check_tmux()?;

    if tmux::has_session(&cli.session) {
        eprintln!("Session '{}' exists, attaching...", cli.session);
        let err = exec::execvp(
            "tmux",
            &["tmux", "attach-session", "-t", &cli.session],
        );
        bail!("exec tmux attach failed: {err}");
    }

    eprintln!("Creating session '{}'...", cli.session);

    // Step 3: Create session with kbtz watch in window 0.
    tmux::create_session(&cli.session, "tasks", "kbtz", &["watch"])?;

    // Step 4: Configure tmux settings.
    tmux::configure_session(&cli.session)?;

    // Store workspace dir as a session option for keybindings.
    let workspace_dir = paths::workspace_dir();
    tmux::set_session_option(&cli.session, "@kbtz_workspace_dir", &workspace_dir)?;

    // Step 5: Spawn the toplevel task-management session.
    let config = Config::load()?;
    spawn_manager_window(&cli.session, &config)?;

    // Step 5b: Install keybindings.
    install_keybindings(&cli.session)?;

    // Step 6: Spawn the orchestrator as a window in the session.
    let self_exe =
        std::env::current_exe().context("failed to determine kbtz-tmux binary path")?;
    let self_exe = self_exe.to_string_lossy();

    let mut orch_args = vec![
        "--no-attach".to_string(),
        "--session".to_string(),
        cli.session.clone(),
        "--max".to_string(),
        cli.max.to_string(),
        "--poll".to_string(),
        cli.poll.to_string(),
    ];
    if let Some(ref pref) = cli.prefer {
        orch_args.push("--prefer".into());
        orch_args.push(pref.clone());
    }

    let env = HashMap::new();
    tmux::spawn_window(&cli.session, "orchestrator", &env, &self_exe, &orch_args)?;

    // Step 7: Attach.
    eprintln!("Attaching to session '{}'...", cli.session);
    let err = exec::execvp(
        "tmux",
        &["tmux", "attach-session", "-t", &cli.session],
    );
    bail!("exec tmux attach failed: {err}");
}

fn install_keybindings(session: &str) -> Result<()> {
    // prefix-c → switch to manager window
    let manager_cmd = concat!(
        "tmux list-windows -F '#{window_id} #{@kbtz_toplevel}' ",
        "| awk '$2==\"true\" {print $1}' ",
        "| head -1 ",
        "| xargs -r tmux select-window -t"
    );
    tmux::bind_key("c", manager_cmd)?;

    // prefix-Tab → jump to next needs-input session
    let self_exe =
        std::env::current_exe().context("failed to determine kbtz-tmux binary path")?;
    let tab_cmd = format!(
        "{} jump-needs-input --session {}",
        self_exe.display(),
        session
    );
    tmux::bind_key("Tab", &tab_cmd)?;

    Ok(())
}

fn run_orchestrator(cli: Cli) -> Result<()> {
    let workspace_dir = paths::workspace_dir();
    fs::create_dir_all(&workspace_dir)?;

    let _lock = acquire_lock(&workspace_dir)?;
    setup_logging(&workspace_dir)?;

    info!(
        "Starting (max={}, poll={}s, session={})",
        cli.max, cli.poll, cli.session
    );

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    let (wake_tx, wake_rx) = mpsc::channel();
    let wake = wake_tx.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
        let _ = wake.send(());
    })
    .context("failed to set signal handler")?;

    let mut orch = Orchestrator::new(
        cli.session,
        cli.max,
        Duration::from_secs(cli.poll),
        cli.prefer,
        running,
    )?;

    orch.run(wake_tx, wake_rx)?;
    orch.shutdown();

    Ok(())
}

fn jump_needs_input(session: &str) -> Result<()> {
    // Get workspace dir from tmux session option or env.
    let workspace_dir = std::env::var("KBTZ_WORKSPACE_DIR").ok().or_else(|| {
        let output = Command::new("tmux")
            .args([
                "show-option",
                "-t",
                session,
                "-v",
                "@kbtz_workspace_dir",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let val = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if val.is_empty() { None } else { Some(val) }
    });
    let workspace_dir = match workspace_dir {
        Some(d) => d,
        None => bail!("cannot determine workspace dir: set KBTZ_WORKSPACE_DIR or @kbtz_workspace_dir session option"),
    };

    // Get current window's session ID to know where we are in the cycle.
    let current_sid = Command::new("tmux")
        .args(["display-message", "-p", "#{@kbtz_sid}"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            } else {
                None
            }
        });

    // Find session IDs with needs_input status.
    let mut needs_input_sids: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&workspace_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if content.trim() == "needs_input" {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        // Skip non-status files (locks, sentinels, sockets, pid files).
                        if name.contains('.') || name == "pane-exited" || name == "orchestrator" {
                            continue;
                        }
                        needs_input_sids.push(paths::filename_to_session_id(name));
                    }
                }
            }
        }
    }

    if needs_input_sids.is_empty() {
        return Ok(());
    }

    needs_input_sids.sort();

    // Pick the next one after current_sid (cycle).
    let target = match &current_sid {
        Some(cur) => {
            let pos = needs_input_sids.iter().position(|s| s > cur);
            match pos {
                Some(i) => &needs_input_sids[i],
                None => &needs_input_sids[0],
            }
        }
        None => &needs_input_sids[0],
    };

    // Find the window with this session ID.
    let output = Command::new("tmux")
        .args([
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_id} #{@kbtz_sid}",
        ])
        .output()
        .context("failed to list windows")?;

    if !output.status.success() {
        return Ok(());
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() == 2 && parts[1] == target {
            let _ = Command::new("tmux")
                .args(["select-window", "-t", parts[0]])
                .status();
            break;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle subcommands first.
    if let Some(cmd) = &cli.command {
        match cmd {
            Commands::JumpNeedsInput { session } => return jump_needs_input(session),
        }
    }

    // If --no-attach, run the orchestrator directly (daemon mode).
    if cli.no_attach {
        return run_orchestrator(cli);
    }

    // Default: bootstrap the tmux session and attach.
    bootstrap(&cli)
}
