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
    is_running: bool,
    scroll_offset: u16,
) {
    let mut lines: Vec<Line> = events
        .iter()
        .flat_map(|event| match event {
            StreamEvent::UserMessage(text) => vec![Line::from(Span::styled(
                format!("▶ {}", text),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ))],
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
            StreamEvent::Result { result, .. } => vec![Line::from(Span::styled(
                format!("[done] {}", truncate(result, 200)),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))],
            StreamEvent::Other(_) => vec![],
        })
        .collect();

    if is_running && lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "⏳ Session running...",
            Style::default().fg(Color::Yellow),
        )));
    }

    let total_lines = lines.len() as u16;
    let visible_height = area.height.saturating_sub(2); // minus border
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = scroll_offset.min(max_scroll);

    let status = if is_running { " ⏳ " } else { "" };
    let scroll_info = if scroll > 0 {
        format!(" [{}/{}] ", total_lines.saturating_sub(scroll), total_lines)
    } else {
        String::new()
    };
    let title = format!(" {}{}{}", session_id, status, scroll_info);

    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_string(),
    }
}
