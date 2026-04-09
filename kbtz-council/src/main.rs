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

use kbtz_council::global::{GlobalState, ProjectStatus};
use kbtz_council::mcp;
use kbtz_council::orchestrator::{Orchestrator, ProjectState};
use kbtz_council::project::Project;
use kbtz_council::session::SessionKey;
use kbtz_council::tui::dashboard::{render_dashboard, SessionInfo, SessionStatus};
use kbtz_council::tui::input::TextInput;
use kbtz_council::tui::stream_view::render_stream_view;
use kbtz_council::tui::InputMode;

fn default_global_dir() -> PathBuf {
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".kbtz-council")
}

const DEFAULT_MAX_SESSIONS: usize = 16;

#[derive(Parser)]
#[command(name = "kbtz-council")]
#[command(about = "Leader-driven AI agent orchestrator")]
struct Cli {
    /// Project name to focus on. If omitted, loads all active projects.
    project: Option<String>,

    /// Path to the global directory (default: ~/.kbtz-council/).
    #[arg(long, default_value_os_t = default_global_dir())]
    global_dir: PathBuf,

    /// Maximum concurrent agent sessions across all projects.
    #[arg(long, default_value_t = DEFAULT_MAX_SESSIONS)]
    max_sessions: usize,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let mut global = GlobalState::open(&cli.global_dir)?;

    // Determine which projects to load
    let project_names: Vec<String> = if let Some(ref name) = cli.project {
        // Ensure the specified project exists (create if needed)
        if global.load_project(name).is_err() {
            let project = Project {
                repos: vec![],
                stakeholders: vec![],
                goal_summary: String::new(),
            };
            global.create_project(name, "", &project)?;
        }
        vec![name.clone()]
    } else {
        global
            .list_projects(Some(ProjectStatus::Active))
            .into_iter()
            .map(|e| e.name.clone())
            .collect()
    };

    let mut orchestrator = Orchestrator::new(cli.max_sessions);

    // Load each project: open its dir, start its MCP server, register it
    for name in &project_names {
        let project_dir = global.load_project(name)?;
        let project_path = project_dir.root().to_path_buf();
        let project_dir = Arc::new(Mutex::new(project_dir));

        let mcp_port = mcp::start_mcp_server(Arc::clone(&project_dir))?;
        let mcp_config_path = mcp::write_mcp_config(&project_path, mcp_port)?;

        let mut ps = ProjectState::new(name.clone(), project_dir, mcp_config_path);
        ps.recover_from_state();
        orchestrator.add_project(ps);
    }

    // Default selection: first project's leader
    if let Some(name) = project_names.first() {
        orchestrator.app.selected_session = Some(format!("{}/leader", name));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut input = TextInput::new();

    let result = run_loop(&mut terminal, &mut orchestrator, &mut input);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    orch: &mut Orchestrator,
    input: &mut TextInput,
) -> io::Result<()> {
    loop {
        orch.poll_sessions();
        orch.process_tick()?;
        orch.reap_and_dispatch()?;

        let editing = orch.app.input_mode == InputMode::Editing;

        terminal.draw(|frame| {
            let area = frame.area();

            let h_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(area);

            // Dashboard (left) — sessions grouped by project
            let session_infos = collect_session_infos(orch);

            render_dashboard(
                frame,
                h_chunks[0],
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
            let session_id = orch.app.selected_session.as_deref().unwrap_or("");
            let is_running = orch.app.selected_session.as_ref()
                .and_then(|name| {
                    let (proj, key) = parse_qualified_session(name);
                    orch.projects.get(proj).and_then(|ps| ps.sessions.get(&key))
                })
                .map(|ms| ms.is_running())
                .unwrap_or(false);
            render_stream_view(frame, v_chunks[0], events, session_id, is_running, orch.app.scroll_offset);

            let title = if editing {
                " Enter send | Ctrl+J newline | Esc cancel "
            } else {
                " Enter to type | Up/Down switch | q quit "
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
                        KeyCode::PageUp => {
                            orch.app.scroll_offset = orch.app.scroll_offset.saturating_add(10);
                        }
                        KeyCode::PageDown => {
                            orch.app.scroll_offset = orch.app.scroll_offset.saturating_sub(10);
                        }
                        KeyCode::Home => {
                            orch.app.scroll_offset = u16::MAX;
                        }
                        KeyCode::End => {
                            orch.app.scroll_offset = 0;
                        }
                        KeyCode::Tab | KeyCode::Down => {
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
                                orch.app.scroll_offset = 0;
                            }
                        }
                        KeyCode::Up => {
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
                                    .map(|i| if i == 0 { keys.len() - 1 } else { i - 1 })
                                    .unwrap_or(keys.len() - 1);
                                orch.app.selected_session = Some(keys[idx].clone());
                                orch.app.scroll_offset = 0;
                            }
                        }
                        _ => {}
                    },
                    InputMode::Editing => {
                        if key.code == KeyCode::Enter {
                            let text = input.text().trim().to_string();
                            if !text.is_empty() {
                                let session_name = orch
                                    .app
                                    .selected_session
                                    .clone()
                                    .unwrap_or_default();
                                let (proj, session_key) = parse_qualified_session(&session_name);
                                orch.send_message(proj, &session_key, text);
                            }
                            input.clear();
                            orch.app.input_mode = InputMode::Normal;
                        } else if key.code == KeyCode::Esc {
                            input.clear();
                            orch.app.input_mode = InputMode::Normal;
                        } else if key.code == KeyCode::Char('j')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
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

/// Collect session info across all projects, grouped by project name.
/// Each session name is qualified as "project/session_key".
fn collect_session_infos(orch: &Orchestrator) -> Vec<SessionInfo> {
    let mut infos = Vec::new();

    // Sort project names for stable ordering
    let mut project_names: Vec<&String> = orch.projects.keys().collect();
    project_names.sort();

    for project_name in project_names {
        let ps = &orch.projects[project_name];
        let dir = ps.project_dir.lock().unwrap();
        let jobs = &dir.state().jobs;

        let mut project_infos: Vec<SessionInfo> = ps
            .sessions
            .values()
            .map(|ms| {
                let (job_phase, job_summary) = match &ms.key {
                    SessionKey::Implementation { job_id } => {
                        let job = jobs.iter().find(|j| &j.id == job_id);
                        (
                            job.map(|j| j.phase.clone()),
                            job.map(|j| j.dispatch.prompt.clone()),
                        )
                    }
                    _ => (None, None),
                };

                SessionInfo {
                    name: format!("{}/{}", project_name, ms.key),
                    status: if ms.is_running() {
                        SessionStatus::Running
                    } else if !ms.queue.is_empty() {
                        SessionStatus::Queued
                    } else {
                        SessionStatus::Idle
                    },
                    queue_depth: ms.queue.len(),
                    job_phase,
                    job_summary,
                }
            })
            .collect();

        // Always show leader for each project
        let leader_key = format!("{}/leader", project_name);
        if !project_infos.iter().any(|i| i.name == leader_key) {
            project_infos.insert(
                0,
                SessionInfo {
                    name: leader_key,
                    status: SessionStatus::Idle,
                    queue_depth: 0,
                    job_phase: None,
                    job_summary: None,
                },
            );
        }

        infos.extend(project_infos);
    }

    infos
}

/// Parse a qualified session name "project/session_key" into (project_name, SessionKey).
///
/// Panics if the name does not contain a "/" separator — all session names
/// must be qualified with their project.
fn parse_qualified_session(s: &str) -> (&str, SessionKey) {
    let pos = s.find('/').unwrap_or_else(|| {
        panic!("session name must be qualified as 'project/key', got '{}'", s)
    });
    let project = &s[..pos];
    let session = &s[pos + 1..];
    (project, parse_session_key(session))
}

fn parse_session_key(s: &str) -> SessionKey {
    if s == "leader" {
        SessionKey::Leader
    } else if let Some(rest) = s.strip_suffix("-impl") {
        SessionKey::Implementation {
            job_id: rest.to_string(),
        }
    } else {
        // Try to parse as job_id-stakeholder_name (e.g. "job-001-security")
        if let Some(pos) = s.find('-').and_then(|first| {
            s[first + 1..].find('-').map(|second| first + 1 + second)
        }) {
            let (job_part, name_part) = s.split_at(pos);
            SessionKey::Stakeholder {
                job_id: job_part.to_string(),
                name: name_part[1..].to_string(),
            }
        } else {
            SessionKey::Leader
        }
    }
}
