use std::collections::{HashMap, HashSet};

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, Paragraph};

use crate::app::{App, TrackedSession};
use kbtz::ui;

pub fn render(frame: &mut Frame, app: &mut App) {
    if let Some(panel) = &app.notes_panel {
        panel.render(frame, frame.area(), app.tree.selected_name());
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    render_tree(frame, app, chunks[0]);
    render_footer(frame, app, chunks[1]);
}

struct SessionDecorator<'a> {
    task_to_session: &'a HashMap<String, String>,
    sessions: &'a HashMap<String, TrackedSession>,
    unread: &'a HashSet<String>,
}

impl ui::TreeDecorator for SessionDecorator<'_> {
    fn decorate(&self, row: &ui::TreeRow) -> ui::RowDecoration {
        // Workspace session: 🤖 + status indicator + session ID (+ unread marker)
        if let Some(sid) = self.task_to_session.get(&row.name) {
            if let Some(ts) = self.sessions.get(sid) {
                let mut after_name = vec![Span::styled(
                    format!(" {sid}"),
                    Style::default().fg(Color::Cyan),
                )];
                if self.unread.contains(sid) {
                    after_name.push(Span::styled(" \u{25cf}", Style::default().fg(Color::Blue)));
                }
                return ui::RowDecoration {
                    icon_override: Some((
                        format!("\u{1f916}{} ", ts.handle.status().indicator()),
                        ui::status_style(&row.status),
                    )),
                    after_name,
                };
            }
        }
        // Externally-claimed active task: 👽 + assignee name
        if row.status == "active" {
            if let Some(ref assignee) = row.assignee {
                return ui::RowDecoration {
                    icon_override: Some(("\u{1f47d} ".to_string(), ui::status_style(&row.status))),
                    after_name: vec![Span::styled(
                        format!(" {assignee}"),
                        Style::default().fg(Color::Cyan),
                    )],
                };
            }
        }
        ui::RowDecoration::default()
    }
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

    let decorator = SessionDecorator {
        task_to_session: &app.task_to_session,
        sessions: &app.sessions,
        unread: &app.unread,
    };
    let items = ui::build_tree_items(&app.tree.rows, &app.tree.collapsed, &decorator);

    let active = app.sessions.len();
    let filter_suffix = match app.tree.filter_label() {
        Some(label) => format!(", {label}"),
        None => String::new(),
    };
    let title = if app.manual {
        format!(" kbtz-workspace ({active} sessions, manual{filter_suffix}) ")
    } else {
        let max = app.max_concurrency;
        format!(" kbtz-workspace ({active}/{max} sessions{filter_suffix}) ")
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray));

    frame.render_stateful_widget(list, area, &mut app.tree.list_state);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let text = if let ui::TreeMode::Search(query) = &app.tree.mode {
        ui::search_footer_line(query)
    } else if let Some(err) = &app.tree.error {
        Line::from(vec![Span::styled(
            err.as_str(),
            Style::default().fg(Color::Red),
        )])
    } else {
        let mut spans = Vec::new();
        if let Some(filter) = &app.tree.filter {
            spans.extend(ui::filter_footer_spans(filter));
        }
        spans.extend(vec![
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
            Span::styled("n", Style::default().fg(Color::Cyan)),
            Span::raw(":notes  "),
            Span::styled("Space", Style::default().fg(Color::Cyan)),
            Span::raw(":collapse  "),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::raw(":search  "),
            Span::styled("D/P", Style::default().fg(Color::Cyan)),
            Span::raw(":filter  "),
            Span::styled("?", Style::default().fg(Color::Cyan)),
            Span::raw(":help  "),
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw(":quit"),
        ]);
        Line::from(spans)
    };

    frame.render_widget(Paragraph::new(text), area);
}

pub fn render_help(frame: &mut Frame) {
    let term = frame.area();
    let width = 55.min(term.width.saturating_sub(4));
    let height = 34.min(term.height.saturating_sub(2));
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
            Span::raw("Jump to needs-input/unread session"),
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
            Span::styled("  n          ", Style::default().fg(Color::Cyan)),
            Span::raw("View notes"),
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
            Span::styled("  /          ", Style::default().fg(Color::Cyan)),
            Span::raw("Search/filter tasks"),
        ]),
        Line::from(vec![
            Span::styled("  Esc        ", Style::default().fg(Color::Cyan)),
            Span::raw("Clear search filter"),
        ]),
        Line::from(vec![
            Span::styled("  D          ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle show done tasks"),
        ]),
        Line::from(vec![
            Span::styled("  P          ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle show paused tasks"),
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
            Span::raw("Jump to needs-input/unread session"),
        ]),
        Line::from(vec![
            Span::styled("  ^B ^B      ", Style::default().fg(Color::Cyan)),
            Span::raw("Send literal Ctrl-B"),
        ]),
        Line::from(vec![
            Span::styled("  ^B ?       ", Style::default().fg(Color::Cyan)),
            Span::raw("Show help"),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Stuck session?",
            Style::default().bold(),
        )]),
        Line::from(Span::raw("  Pause then unpause the task (p, p)")),
        Line::from(Span::raw("  to kill and respawn its session.")),
    ];

    frame.render_widget(Paragraph::new(help_text), inner);
}
