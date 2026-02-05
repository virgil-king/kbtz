use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use super::app::App;
use crate::model::Status;

pub fn render(frame: &mut Frame, app: &App) {
    if app.show_notes {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(frame.area());
        render_tree(frame, app, chunks[0]);
        render_notes(frame, app, chunks[1]);
    } else {
        render_tree(frame, app, frame.area());
    }
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut prefix = String::new();

            // Build tree lines
            for d in 1..row.depth + 1 {
                if d == row.depth {
                    if row.is_last_at_depth[d] {
                        prefix.push_str("└── ");
                    } else {
                        prefix.push_str("├── ");
                    }
                } else if row.is_last_at_depth[d] {
                    prefix.push_str("    ");
                } else {
                    prefix.push_str("│   ");
                }
            }

            // Collapse indicator for nodes with children
            let collapse_indicator = if row.has_children {
                if app.collapsed.contains(&row.name) {
                    "> "
                } else {
                    "v "
                }
            } else {
                "  "
            };

            let status_icon = row.status.icon();
            let status_style = match row.status {
                Status::Active => Style::default().fg(Color::Green),
                Status::Idle => Style::default().fg(Color::Yellow),
                Status::Done => Style::default().fg(Color::DarkGray),
            };

            let blocked_info = if row.blocked_by.is_empty() {
                String::new()
            } else {
                format!(" [blocked by: {}]", row.blocked_by.join(", "))
            };

            let desc = if row.description.is_empty() {
                String::new()
            } else {
                format!("  {}", row.description)
            };

            let line = Line::from(vec![
                Span::raw(prefix),
                Span::raw(collapse_indicator),
                Span::styled(format!("{status_icon} "), status_style),
                Span::styled(
                    row.name.clone(),
                    Style::default().bold(),
                ),
                Span::styled(
                    blocked_info,
                    Style::default().fg(Color::Red),
                ),
                Span::raw(desc),
            ]);

            let item = ListItem::new(line);
            if i == app.cursor {
                item.style(Style::default().bg(Color::DarkGray))
            } else {
                item
            }
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tasks "),
        );

    frame.render_widget(list, area);
}

fn render_notes(frame: &mut Frame, app: &App, area: Rect) {
    let title = app
        .selected_name()
        .map(|n| format!(" Notes: {n} "))
        .unwrap_or_else(|| " Notes ".to_string());

    let text = if app.notes.is_empty() {
        "No notes.".to_string()
    } else {
        app.notes
            .iter()
            .map(|n| format!("[{}]\n{}\n", n.created_at, n.content))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let paragraph = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}
