use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
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
use kbtz_council::session::SessionKey;
use kbtz_council::tui::dashboard::{render_dashboard, SessionInfo, SessionStatus};
use kbtz_council::tui::input::TextInput;
use kbtz_council::tui::stream_view::render_stream_view;
use kbtz_council::tui::InputMode;

#[derive(Parser)]
#[command(name = "kbtz-council")]
#[command(about = "Leader-driven AI agent orchestrator")]
struct Cli {
    /// Path to the project directory. Created if it doesn't exist.
    #[arg(short, long)]
    project: PathBuf,

    /// Run as MCP stdio server (spawned by claude as a subprocess).
    #[arg(long)]
    mcp_stdio: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.mcp_stdio {
        return mcp::run_mcp_stdio(&cli.project);
    }

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

    // Get our own binary path for the MCP config
    let self_binary = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("kbtz-council"))
        .to_string_lossy()
        .to_string();
    let mcp_config_path = mcp::write_mcp_config(&cli.project, &self_binary)?;

    let mut orchestrator = Orchestrator::new(Arc::clone(&project_dir), mcp_config_path);
    orchestrator.app.selected_session = Some("leader".to_string());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input = TextInput::new();

    let result = run_loop(&mut terminal, &mut orchestrator, &project_dir, &mut input);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    orch: &mut Orchestrator,
    project_dir: &Arc<Mutex<ProjectDir>>,
    input: &mut TextInput,
) -> io::Result<()> {
    loop {
        orch.poll_sessions();
        orch.process_tick()?;

        let editing = orch.app.input_mode == InputMode::Editing;

        terminal.draw(|frame| {
            let area = frame.area();

            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(area);

            // Dashboard (left)
            let dir = project_dir.lock().unwrap();
            let steps = dir.state().steps.clone();
            drop(dir);

            let session_infos = collect_session_infos(orch);

            render_dashboard(
                frame,
                h_chunks[0],
                &steps,
                &session_infos,
                &orch.app.selected_session,
            );

            // Right panel: stream view + input
            let input_height = input.height(h_chunks[1].width);
            let v_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(input_height)])
                .split(h_chunks[1]);

            let events = orch.app.selected_events();
            let session_id = orch.app.selected_session.as_deref().unwrap_or("leader");
            let is_running = orch.app.selected_session.as_ref()
                .and_then(|name| {
                    let key = parse_session_key(name);
                    orch.sessions.get(&key)
                })
                .map(|ms| ms.is_running())
                .unwrap_or(false);
            render_stream_view(frame, v_chunks[0], events, session_id, is_running);

            let title = if editing {
                " Ctrl+S send | Esc cancel "
            } else {
                " Enter to type | Tab switch | q quit "
            };
            input.render(frame, v_chunks[1], editing, title);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match orch.app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Enter => {
                            orch.app.input_mode = InputMode::Editing;
                        }
                        KeyCode::Tab => {
                            let keys: Vec<String> = collect_session_infos(orch)
                            .iter()
                            .map(|i| i.name.clone())
                            .collect();
                            if !keys.is_empty() {
                                let idx = orch
                                    .app
                                    .selected_session
                                    .as_ref()
                                    .and_then(|s| keys.iter().position(|k| k == s))
                                    .map(|i| (i + 1) % keys.len())
                                    .unwrap_or(0);
                                orch.app.selected_session = Some(keys[idx].clone());
                            }
                        }
                        _ => {}
                    },
                    InputMode::Editing => {
                        if key.code == KeyCode::Char('s')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            let text = input.text().trim().to_string();
                            if !text.is_empty() {
                                let session_name = orch
                                    .app
                                    .selected_session
                                    .clone()
                                    .unwrap_or_else(|| "leader".to_string());
                                let session_key = parse_session_key(&session_name);
                                orch.send_message(&session_key, text);
                            }
                            input.clear();
                            orch.app.input_mode = InputMode::Normal;
                        } else if key.code == KeyCode::Esc {
                            input.clear();
                            orch.app.input_mode = InputMode::Normal;
                        } else if key.code == KeyCode::Enter {
                            input.insert_newline();
                        } else if key.code == KeyCode::Backspace {
                            input.backspace();
                        } else if let KeyCode::Char(c) = key.code {
                            input.insert_char(c);
                        }
                    }
                }
            }
        }
    }
}

fn collect_session_infos(orch: &Orchestrator) -> Vec<SessionInfo> {
    let mut infos: Vec<SessionInfo> = orch
        .sessions
        .values()
        .map(|ms| SessionInfo {
            name: ms.key.to_string(),
            status: if ms.is_running() {
                SessionStatus::Running
            } else if !ms.queue.is_empty() {
                SessionStatus::Queued
            } else {
                SessionStatus::Idle
            },
            queue_depth: ms.queue.len(),
        })
        .collect();
    // Always show leader
    if !infos.iter().any(|i| i.name == "leader") {
        infos.insert(
            0,
            SessionInfo {
                name: "leader".to_string(),
                status: SessionStatus::Idle,
                queue_depth: 0,
            },
        );
    }
    infos
}

fn parse_session_key(s: &str) -> SessionKey {
    if s == "leader" {
        SessionKey::Leader
    } else if let Some(name) = s.strip_prefix("stakeholder-") {
        SessionKey::Stakeholder {
            name: name.to_string(),
        }
    } else if let Some(step_id) = s.strip_suffix("-impl") {
        SessionKey::Implementation {
            step_id: step_id.to_string(),
        }
    } else {
        SessionKey::Leader
    }
}
