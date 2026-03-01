use anyhow::Result;
use clap::Parser;

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

fn main() -> Result<()> {
    let cli = Cli::parse();
    eprintln!(
        "kbtz-tmux: max={} poll={}s session={}",
        cli.max, cli.poll, cli.session
    );
    Ok(())
}
