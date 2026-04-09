use crate::job::{Job, JobPhase};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

/// Session info for dashboard display.
pub struct SessionInfo {
    pub name: String,
    pub status: SessionStatus,
    pub queue_depth: usize,
}

pub enum SessionStatus {
    Idle,
    Running,
    Queued,
}

pub fn render_dashboard(
    frame: &mut Frame,
    area: Rect,
    steps: &[Job],
    sessions: &[SessionInfo],
    selected_session: &Option<String>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(8),
        ])
        .split(area);

    let header = Paragraph::new("kbtz-council")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    let job_items: Vec<ListItem> = steps
        .iter()
        .map(|job| {
            let phase = match &job.phase {
                JobPhase::Dispatched => ("DISPATCHED", Color::Yellow),
                JobPhase::Running => ("RUNNING", Color::Blue),
                JobPhase::Completed => ("COMPLETED", Color::Green),
                JobPhase::Reviewing => ("REVIEWING", Color::Magenta),
                JobPhase::Reviewed => ("REVIEWED", Color::Cyan),
                JobPhase::Merged => ("MERGED", Color::DarkGray),
                JobPhase::Rework => ("REWORK", Color::Red),
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{} ", job.id),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("[{}] ", phase.0),
                    Style::default().fg(phase.1),
                ),
                Span::raw(&job.dispatch.prompt),
            ]);
            ListItem::new(line)
        })
        .collect();

    let job_list =
        List::new(job_items).block(Block::default().title(" Jobs ").borders(Borders::ALL));
    frame.render_widget(job_list, chunks[1]);

    let session_items: Vec<ListItem> = sessions
        .iter()
        .map(|s| {
            let selected = Some(&s.name) == selected_session.as_ref();
            let (indicator, indicator_color) = match s.status {
                SessionStatus::Running => (">>", Color::Green),
                SessionStatus::Queued => ("..", Color::Yellow),
                SessionStatus::Idle => ("  ", Color::DarkGray),
            };
            let queue_info = if s.queue_depth > 0 {
                format!(" [+{}]", s.queue_depth)
            } else {
                String::new()
            };
            let name_style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(format!("{} ", indicator), Style::default().fg(indicator_color)),
                Span::styled(s.name.clone(), name_style),
                Span::styled(queue_info, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let session_list = List::new(session_items)
        .block(Block::default().title(" Sessions ").borders(Borders::ALL));
    frame.render_widget(session_list, chunks[2]);
}
