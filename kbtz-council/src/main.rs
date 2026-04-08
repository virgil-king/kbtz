use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    Terminal,
};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use kbtz_council::project::ProjectDir;
use kbtz_council::tui::dashboard::render_dashboard;
use kbtz_council::tui::stream_view::render_stream_view;
use kbtz_council::tui::{AppState, View};

#[derive(Parser)]
#[command(name = "kbtz-council")]
#[command(about = "Leader-driven AI agent orchestrator")]
struct Cli {
    /// Path to the project directory. Created if it doesn't exist.
    #[arg(short, long)]
    project: PathBuf,

    /// Run as MCP stdio server (used by Claude Code, not for direct invocation)
    #[arg(long)]
    mcp_mode: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.mcp_mode {
        let (tx, _rx) = std::sync::mpsc::channel();
        let (_resp_tx, resp_rx) = std::sync::mpsc::channel();
        kbtz_council::mcp::run_mcp_server(tx, resp_rx)?;
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = AppState::new();
    let result = run_loop(&mut terminal, &mut app, &cli.project);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    project_path: &PathBuf,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            match app.view {
                View::Dashboard => {
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                        .split(area);

                    let steps = if let Ok(dir) = ProjectDir::load(project_path) {
                        dir.state().steps.clone()
                    } else {
                        vec![]
                    };

                    let sessions: Vec<String> = app
                        .session_events
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect();

                    render_dashboard(frame, chunks[0], &steps, &sessions, &app.selected_session);

                    let events = app.selected_events();
                    let session_id = app.selected_session.as_deref().unwrap_or("(none)");
                    render_stream_view(frame, chunks[1], events, session_id);
                }
                View::Leader => {
                    // Leader PTY rendering handled via raw byte forwarding
                }
            }
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Tab => {
                        let sessions: Vec<String> = app
                            .session_events
                            .iter()
                            .map(|(id, _)| id.clone())
                            .collect();
                        if !sessions.is_empty() {
                            let current_idx = app
                                .selected_session
                                .as_ref()
                                .and_then(|s| sessions.iter().position(|id| id == s))
                                .map(|i| (i + 1) % sessions.len())
                                .unwrap_or(0);
                            app.selected_session = Some(sessions[current_idx].clone());
                        }
                    }
                    KeyCode::Char('l') if app.leader_idle => {
                        app.view = View::Leader;
                    }
                    KeyCode::Esc => {
                        app.view = View::Dashboard;
                    }
                    _ => {}
                }
            }
        }
    }
}
