use crate::stream::StreamEvent;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub fn render_stream_view(
    frame: &mut Frame,
    area: Rect,
    events: &[StreamEvent],
    session_id: &str,
) {
    let lines: Vec<Line> = events
        .iter()
        .flat_map(|event| match event {
            StreamEvent::Thinking(text) => vec![Line::from(Span::styled(
                format!("[thinking] {}", truncate(text, 200)),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))],
            StreamEvent::AssistantText(text) => {
                vec![Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::White),
                ))]
            }
            StreamEvent::ToolUse { name, input } => vec![Line::from(Span::styled(
                format!("[tool] {} {}", name, truncate(input, 100)),
                Style::default().fg(Color::Yellow),
            ))],
            StreamEvent::ToolResult { content } => vec![Line::from(Span::styled(
                format!("[result] {}", truncate(content, 100)),
                Style::default().fg(Color::Green),
            ))],
            StreamEvent::Result { result } => vec![Line::from(Span::styled(
                format!("[done] {}", truncate(result, 200)),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))],
            StreamEvent::Other(_) => vec![],
        })
        .collect();

    let title = format!(" {} ", session_id);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}
