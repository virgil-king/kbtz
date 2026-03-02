use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, Paragraph, Wrap};

use super::app::{AddField, App};
use crate::ui;

pub fn render(frame: &mut Frame, app: &mut App) {
    if app.show_notes {
        render_notes(frame, app, frame.area());
        return;
    }

    let area = frame.area();
    render_tree(frame, app, area);

    match &app.tree.mode {
        ui::TreeMode::ConfirmDone(name) => {
            ui::render_confirm(frame, "Done", name, "has an active session.")
        }
        ui::TreeMode::ConfirmPause(name) => {
            ui::render_confirm(frame, "Pause", name, "has an active session.")
        }
        ui::TreeMode::Help => render_help(frame),
        ui::TreeMode::Normal => {}
    }
    if app.add_form.is_some() {
        render_add_dialog(frame, app);
    }
}

fn render_tree(frame: &mut Frame, app: &mut App, area: Rect) {
    let (tree_area, error_area) = if app.tree.error.is_some() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    if let (Some(err), Some(err_area)) = (&app.tree.error, error_area) {
        frame.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red)),
            err_area,
        );
    }

    let items = ui::build_tree_items(&app.tree.rows, &app.tree.collapsed, |_| {
        ui::RowDecoration::default()
    });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Tasks "))
        .highlight_style(Style::default().bg(Color::DarkGray));
    frame.render_stateful_widget(list, tree_area, &mut app.tree.list_state);
}

fn render_notes(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let notes_area = chunks[0];
    let hint_area = chunks[1];

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
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.notes_scroll, 0));

    frame.render_widget(paragraph, notes_area);

    frame.render_widget(
        Paragraph::new("Esc/q/n: back  j/k: scroll  g/G: top/bottom")
            .style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );
}

fn render_field(
    frame: &mut Frame,
    label: &str,
    value: &str,
    focused: bool,
    chunks: &[Rect],
    idx: &mut usize,
) {
    let label_style = if focused {
        Style::default().fg(Color::Cyan).bold()
    } else {
        Style::default()
    };
    frame.render_widget(Paragraph::new(label).style(label_style), chunks[*idx]);
    *idx += 1;

    let cursor = if focused { "_" } else { "" };
    frame.render_widget(
        Paragraph::new(format!("  {value}{cursor}")).style(Style::default().fg(Color::White)),
        chunks[*idx],
    );
    *idx += 1;
}

fn render_add_dialog(frame: &mut Frame, app: &App) {
    let form = match &app.add_form {
        Some(f) => f,
        None => return,
    };

    let term = frame.area();
    let width = 60.min(term.width.saturating_sub(4));
    let content_rows: u16 = 8 + u16::from(form.error.is_some()); // parent + 3*(label+input) + hint
    let height = (content_rows + 2).min(term.height.saturating_sub(2)); // +2 for borders
    let area = ui::centered_rect(width, height, term);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Add Task ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let has_error = form.error.is_some();
    let mut constraints = vec![
        Constraint::Length(1), // parent
        Constraint::Length(1), // name label
        Constraint::Length(1), // name input
        Constraint::Length(1), // desc label
        Constraint::Length(1), // desc input
        Constraint::Length(1), // note label
        Constraint::Length(1), // note input
    ];
    if has_error {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // hint
    constraints.push(Constraint::Min(0)); // spacer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut idx = 0;

    // Parent
    let parent_text = match &form.parent {
        Some(p) => format!("Parent: {p}"),
        None => "Parent: (none)".into(),
    };
    frame.render_widget(
        Paragraph::new(parent_text).style(Style::default().fg(Color::DarkGray)),
        chunks[idx],
    );
    idx += 1;

    render_field(
        frame,
        "Name:",
        &form.name,
        form.focused == AddField::Name,
        &chunks,
        &mut idx,
    );
    render_field(
        frame,
        "Description:",
        &form.description,
        form.focused == AddField::Description,
        &chunks,
        &mut idx,
    );
    render_field(
        frame,
        "Note:",
        &form.note,
        form.focused == AddField::Note,
        &chunks,
        &mut idx,
    );

    // Error
    if let Some(err) = &form.error {
        frame.render_widget(
            Paragraph::new(err.as_str()).style(Style::default().fg(Color::Red)),
            chunks[idx],
        );
        idx += 1;
    }

    // Hint
    frame.render_widget(
        Paragraph::new("Enter: submit  Tab/S-Tab: fields  C-e: editor  Esc: cancel  C-u: clear")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[idx],
    );
}

fn render_help(frame: &mut Frame) {
    let term = frame.area();
    let width = 50.min(term.width.saturating_sub(4));
    let height = 21.min(term.height.saturating_sub(2));
    let area = ui::centered_rect(width, height, term);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let help_text = vec![
        Line::from(vec![
            Span::styled("j/Down  ", Style::default().fg(Color::Cyan)),
            Span::raw("Move down"),
        ]),
        Line::from(vec![
            Span::styled("k/Up    ", Style::default().fg(Color::Cyan)),
            Span::raw("Move up"),
        ]),
        Line::from(vec![
            Span::styled("Space   ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle collapse"),
        ]),
        Line::from(vec![
            Span::styled("Enter/n ", Style::default().fg(Color::Cyan)),
            Span::raw("View notes"),
        ]),
        Line::from(vec![
            Span::styled("a       ", Style::default().fg(Color::Cyan)),
            Span::raw("Add child task"),
        ]),
        Line::from(vec![
            Span::styled("A       ", Style::default().fg(Color::Cyan)),
            Span::raw("Add root task"),
        ]),
        Line::from(vec![
            Span::styled("N       ", Style::default().fg(Color::Cyan)),
            Span::raw("Add note to selected task"),
        ]),
        Line::from(vec![
            Span::styled("p       ", Style::default().fg(Color::Cyan)),
            Span::raw("Pause/unpause task"),
        ]),
        Line::from(vec![
            Span::styled("d       ", Style::default().fg(Color::Cyan)),
            Span::raw("Close task (mark done)"),
        ]),
        Line::from(vec![
            Span::styled("U       ", Style::default().fg(Color::Cyan)),
            Span::raw("Force-unassign task"),
        ]),
        Line::from(vec![
            Span::styled("?       ", Style::default().fg(Color::Cyan)),
            Span::raw("Toggle help"),
        ]),
        Line::from(vec![
            Span::styled("q/Esc   ", Style::default().fg(Color::Cyan)),
            Span::raw("Quit"),
        ]),
        Line::raw(""),
        Line::from(vec![Span::styled(
            "Add Task Dialog:",
            Style::default().bold(),
        )]),
        Line::from(vec![
            Span::styled("  Tab/S-Tab ", Style::default().fg(Color::Cyan)),
            Span::raw("Next/prev field"),
        ]),
        Line::from(vec![
            Span::styled("  Enter     ", Style::default().fg(Color::Cyan)),
            Span::raw("Submit"),
        ]),
        Line::from(vec![
            Span::styled("  Esc       ", Style::default().fg(Color::Cyan)),
            Span::raw("Cancel"),
        ]),
        Line::from(vec![
            Span::styled("  C-u       ", Style::default().fg(Color::Cyan)),
            Span::raw("Clear field"),
        ]),
        Line::from(vec![
            Span::styled("  C-e       ", Style::default().fg(Color::Cyan)),
            Span::raw("Open $EDITOR for note"),
        ]),
    ];

    let paragraph = Paragraph::new(help_text);
    frame.render_widget(paragraph, inner);
}
