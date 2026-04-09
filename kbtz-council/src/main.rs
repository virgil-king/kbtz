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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbtz_council::mcp;
use kbtz_council::orchestrator::Orchestrator;
use kbtz_council::project::{Project, ProjectDir};
use kbtz_council::tui::dashboard::render_dashboard;
use kbtz_council::tui::stream_view::render_stream_view;
use kbtz_council::tui::View;

#[derive(Parser)]
#[command(name = "kbtz-council")]
#[command(about = "Leader-driven AI agent orchestrator")]
struct Cli {
    /// Path to the project directory. Created if it doesn't exist.
    #[arg(short, long)]
    project: PathBuf,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    // Init or load project
    let project_dir = if cli.project.join("state.json").exists() {
        ProjectDir::load(&cli.project)?
    } else {
        let project = Project {
            repos: vec![],
            stakeholders: vec![],
            goal_summary: String::new(),
        };
        ProjectDir::init(&cli.project, &project)?
    };

    let project_dir = Arc::new(Mutex::new(project_dir));

    // Start embedded MCP HTTP server
    let mcp_port = mcp::start_mcp_server(Arc::clone(&project_dir))?;
    let mcp_config_path = mcp::write_mcp_config(&cli.project, mcp_port)?;
    std::fs::write(cli.project.join("mcp-port"), mcp_port.to_string())?;

    let mut orchestrator = Orchestrator::new(Arc::clone(&project_dir), mcp_config_path);

    // Init terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut orchestrator, &project_dir);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    orch: &mut Orchestrator,
    project_dir: &Arc<Mutex<ProjectDir>>,
) -> io::Result<()> {
    loop {
        // Poll sessions for events and exits
        orch.poll_sessions();

        // Run lifecycle state machine (picks up new steps from MCP tool calls)
        orch.process_tick()?;

        // Render TUI
        terminal.draw(|frame| {
            let area = frame.area();

            match orch.app.view {
                View::Dashboard => {
                    let chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                        .split(area);

                    let dir = project_dir.lock().unwrap();
                    let steps = dir.state().steps.clone();
                    drop(dir);

                    let sessions: Vec<String> = orch
                        .app
                        .session_events
                        .iter()
                        .map(|(id, _)| id.clone())
                        .collect();

                    render_dashboard(
                        frame,
                        chunks[0],
                        &steps,
                        &sessions,
                        &orch.app.selected_session,
                    );

                    let events = orch.app.selected_events();
                    let session_id = orch.app.selected_session.as_deref().unwrap_or("(none)");
                    render_stream_view(frame, chunks[1], events, session_id);
                }
                View::Leader => {
                    // Leader PTY rendering handled via raw byte forwarding
                }
            }
        })?;

        // Handle keyboard input
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Tab => {
                        let sessions: Vec<String> = orch
                            .app
                            .session_events
                            .iter()
                            .map(|(id, _)| id.clone())
                            .collect();
                        if !sessions.is_empty() {
                            let current_idx = orch
                                .app
                                .selected_session
                                .as_ref()
                                .and_then(|s| sessions.iter().position(|id| id == s))
                                .map(|i| (i + 1) % sessions.len())
                                .unwrap_or(0);
                            orch.app.selected_session = Some(sessions[current_idx].clone());
                        }
                    }
                    KeyCode::Char('l') if orch.app.leader_idle => {
                        orch.app.view = View::Leader;
                    }
                    KeyCode::Esc => {
                        orch.app.view = View::Dashboard;
                    }
                    _ => {}
                }
            }
        }
    }
}
