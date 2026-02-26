use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::app::App;
use kbtz::ui;

pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    render_tree(frame, app, chunks[0]);
    render_footer(frame, app, chunks[1]);
}

fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.tree.rows.is_empty() {
        let msg = Paragraph::new("No tasks. Add tasks with: kbtz add <name> <description>")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" kbtz-workspace "),
            );
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .tree
        .rows
        .iter()
        .map(|row| {
            let prefix = ui::tree_prefix(row);

            let collapse_indicator = if row.has_children {
                if app.tree.collapsed.contains(&row.name) {
                    "> "
                } else {
                    "v "
                }
            } else {
                "  "
            };

            // Session indicator: 🤖 for workspace sessions, 👽 for
            // externally-claimed tasks, status icon otherwise.
            let (bot, icon, session_suffix) = if let Some(sid) = app.task_to_session.get(&row.name)
            {
                if let Some(session) = app.sessions.get(sid) {
                    (
                        "\u{1f916}",
                        format!("{} ", session.status().indicator()),
                        format!(" {}", sid),
                    )
                } else {
                    ("", format!("{}  ", ui::icon_for_task(row)), String::new())
                }
            } else if row.status == "active" {
                if let Some(ref assignee) = row.assignee {
                    (
                        "\u{1f47d}",
                        format!("{}  ", ui::icon_for_task(row)),
                        format!(" {}", assignee),
                    )
                } else {
                    ("", format!("{}  ", ui::icon_for_task(row)), String::new())
                }
            } else {
                ("", format!("{}  ", ui::icon_for_task(row)), String::new())
            };
            let style = ui::status_style(&row.status);

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
                Span::raw(bot),
                Span::styled(icon, style),
                Span::styled(row.name.clone(), Style::default().bold()),
                Span::styled(session_suffix, Style::default().fg(Color::Cyan)),
                Span::styled(blocked_info, Style::default().fg(Color::Red)),
                Span::raw(desc),
            ]);

            ListItem::new(line)
        })
        .collect();

    let active = app.sessions.len();
    let title = if app.manual {
        format!(" kbtz-workspace ({active} sessions, manual) ")
    } else {
        let max = app.max_concurrency;
        format!(" kbtz-workspace ({active}/{max} sessions) ")
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray));

    frame.render_stateful_widget(list, area, &mut app.tree.list_state);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let Some(err) = &app.tree.error {
        Line::from(vec![Span::styled(
            err.as_str(),
            Style::default().fg(Color::Red),
        )])
    } else {
        Line::from(vec![
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::raw(":nav  "),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(":zoom  "),
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::raw(":input  "),
            Span::styled("s", Style::default().fg(Color::Cyan)),
            Span::raw(":spawn  "),
            Span::styled("c", Style::default().fg(Color::Cyan)),
            Span::raw(":manager  "),
            Span::styled("Space", Style::default().fg(Color::Cyan)),
            Span::raw(":collapse  "),
            Span::styled("p", Style::default().fg(Color::Cyan)),
            Span::raw(":pause  "),
            Span::styled("d", Style::default().fg(Color::Cyan)),
            Span::raw(":done  "),
            Span::styled("U", Style::default().fg(Color::Cyan)),
            Span::raw(":force-unassign  "),
            Span::styled("?", Style::default().fg(Color::Cyan)),
            Span::raw(":help  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(":quit"),
        ])
    };

    frame.render_widget(Paragraph::new(text), area);
}

pub fn render_help(frame: &mut Frame) {
    let term = frame.area();
    let width = 55.min(term.width.saturating_sub(4));
    let height = 26.min(term.height.saturating_sub(2));
    let area = ui::centered_rect(width, height, term);
    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let help_text = vec![
        Line::from(vec![Span::styled("Tree mode:", Style::default().bold())]),
        Line::from(vec![
            Span::styled("  j/k, Up/Down  ", Style::default().fg(Color::Cyan)),
            Span::raw("Navigate tasks"),
        ]),
        Line::from(vec![
            Span::styled("  Enter      ", Style::default().fg(Color::Cyan)),
            Span::raw("Zoom into session"),
        ]),
        Line::from(vec![
            Span::styled("  Tab        ", Style::default().fg(Color::Cyan)),
            Span::raw("Jump to needs-input session"),
        ]),
        Line::from(vec![
            Span::styled("  s          ", Style::default().fg(Color::Cyan)),
            Span::raw("Spawn session for task"),
        ]),
        Line::from(vec![
            Span::styled("  c          ", Style::default().fg(Color::Cyan)),
            Span::raw("Task manager session"),
        ]),
        Line::from(vec![
            Span::styled("  Space      ", Style::default().fg(Color::Cyan)),
            Span::raw("Collapse/expand"),
        ]),
        Line::from(vec![
            Span::styled("  p          ", Style::default().fg(Color::Cyan)),
            Span::raw("Pause/unpause task"),
        ]),
        Line::from(vec![
            Span::styled("  d          ", Style::default().fg(Color::Cyan)),
            Span::raw("Mark task done"),
        ]),
        Line::from(vec![
            Span::styled("  U          ", Style::default().fg(Color::Cyan)),
            Span::raw("Force-unassign task"),
        ]),
        Line::from(vec![
            Span::styled("  q          ", Style::default().fg(Color::Cyan)),
            Span::raw("Quit (releases all sessions)"),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Zoomed / Manager mode:",
            Style::default().bold(),
        )]),
        Line::from(vec![
            Span::styled("  ^B t       ", Style::default().fg(Color::Cyan)),
            Span::raw("Return to tree"),
        ]),
        Line::from(vec![
            Span::styled("  ^B c       ", Style::default().fg(Color::Cyan)),
            Span::raw("Task manager session"),
        ]),
        Line::from(vec![
            Span::styled("  ^B n/p     ", Style::default().fg(Color::Cyan)),
            Span::raw("Next/prev session"),
        ]),
        Line::from(vec![
            Span::styled("  ^B Tab     ", Style::default().fg(Color::Cyan)),
            Span::raw("Jump to needs-input session"),
        ]),
        Line::from(vec![
            Span::styled("  ^B ^B      ", Style::default().fg(Color::Cyan)),
            Span::raw("Send literal Ctrl-B"),
        ]),
        Line::from(vec![
            Span::styled("  ^B ?       ", Style::default().fg(Color::Cyan)),
            Span::raw("Show help"),
        ]),
    ];

    frame.render_widget(Paragraph::new(help_text), inner);
}

pub fn render_confirm(frame: &mut Frame, action: &str, task_name: &str, message: &str) {
    let term = frame.area();
    let width = 50.min(term.width.saturating_sub(4));
    let height = 5.min(term.height.saturating_sub(2));
    let area = ui::centered_rect(width, height, term);
    frame.render_widget(Clear, area);

    let title = format!(" {action} ");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = vec![
        Line::from(vec![
            Span::raw("Task "),
            Span::styled(task_name, Style::default().bold()),
            Span::raw(format!(" {message}")),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Proceed? "),
            Span::styled("y", Style::default().fg(Color::Green).bold()),
            Span::raw("/"),
            Span::styled("n", Style::default().fg(Color::Red).bold()),
        ]),
    ];

    frame.render_widget(Paragraph::new(text), inner);
}
