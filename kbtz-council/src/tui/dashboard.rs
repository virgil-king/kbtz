use crate::job::JobPhase;
use crate::orchestrator::SelectedSession;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

/// Unified session info for dashboard display.
pub struct SessionInfo {
    pub session: SelectedSession,
    pub status: SessionStatus,
    pub queue_depth: usize,
    pub job_phase: Option<JobPhase>,
    pub job_summary: Option<String>,
}

pub enum SessionStatus {
    Idle,
    Running,
    Queued,
}

pub fn render_dashboard(
    frame: &mut Frame,
    area: Rect,
    sessions: &[SessionInfo],
    selected_session: &Option<SelectedSession>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let header = Paragraph::new("kbtz-council")
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::BOTTOM));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = sessions
        .iter()
        .map(|s| {
            let selected = Some(&s.session) == selected_session.as_ref();
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

            let mut spans = vec![
                Span::styled(format!("{} ", indicator), Style::default().fg(indicator_color)),
                Span::styled(s.session.to_string(), name_style),
            ];

            if let Some(ref phase) = s.job_phase {
                let (phase_str, phase_color) = match phase {
                    JobPhase::Dispatched => ("DISPATCHED", Color::Yellow),
                    JobPhase::Running => ("RUNNING", Color::Blue),
                    JobPhase::Completed => ("COMPLETED", Color::Green),
                    JobPhase::Reviewing => ("REVIEWING", Color::Magenta),
                    JobPhase::Reviewed => ("REVIEWED", Color::Cyan),
                    JobPhase::Merged => ("MERGED", Color::DarkGray),
                    JobPhase::Rework => ("REWORK", Color::Red),
                };
                spans.push(Span::styled(
                    format!(" [{}]", phase_str),
                    Style::default().fg(phase_color),
                ));
            }

            if let Some(ref summary) = s.job_summary {
                let truncated = if summary.len() > 60 {
                    format!(" {}...", &summary[..57])
                } else {
                    format!(" {}", summary)
                };
                spans.push(Span::styled(truncated, Style::default().fg(Color::DarkGray)));
            }

            spans.push(Span::styled(queue_info, Style::default().fg(Color::DarkGray)));

            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).block(Block::default().title(" Sessions ").borders(Borders::ALL));
    frame.render_widget(list, chunks[1]);
}
