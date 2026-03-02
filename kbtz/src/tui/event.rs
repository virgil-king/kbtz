use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{AddField, App};
use crate::ui::TreeKeyAction;

/// Result of handling a key press.
pub enum KeyAction {
    Quit,
    Submit,
    Refresh,
    OpenEditor,
    AddNote,
    Pause(String),
    Unpause(String),
    MarkDone(String),
    ForceUnassign(String),
    Continue,
}

/// Handle a key press. Returns an action indicating what the event loop should do.
pub fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    if app.add_form.is_some() {
        return handle_add(app, key);
    }

    if app.show_notes {
        return handle_notes(app, key);
    }

    match app.tree.handle_key(key) {
        TreeKeyAction::Quit => KeyAction::Quit,
        TreeKeyAction::Refresh => KeyAction::Refresh,
        TreeKeyAction::Pause(n) => KeyAction::Pause(n),
        TreeKeyAction::Unpause(n) => KeyAction::Unpause(n),
        TreeKeyAction::MarkDone(n) => KeyAction::MarkDone(n),
        TreeKeyAction::ForceUnassign(n) => KeyAction::ForceUnassign(n),
        TreeKeyAction::Continue => KeyAction::Continue,
        TreeKeyAction::Unhandled => match key.code {
            KeyCode::Esc => KeyAction::Quit,
            KeyCode::Enter | KeyCode::Char('n') => {
                app.toggle_notes();
                KeyAction::Continue
            }
            KeyCode::Char('a') => {
                app.enter_add_mode(true);
                KeyAction::Continue
            }
            KeyCode::Char('A') => {
                app.enter_add_mode(false);
                KeyAction::Continue
            }
            KeyCode::Char('N') => KeyAction::AddNote,
            _ => KeyAction::Continue,
        },
    }
}

fn handle_notes(app: &mut App, key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('q') => {
            app.toggle_notes();
            KeyAction::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.notes_scroll = app.notes_scroll.saturating_add(1);
            KeyAction::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.notes_scroll = app.notes_scroll.saturating_sub(1);
            KeyAction::Continue
        }
        KeyCode::PageDown => {
            app.notes_scroll = app.notes_scroll.saturating_add(20);
            KeyAction::Continue
        }
        KeyCode::PageUp => {
            app.notes_scroll = app.notes_scroll.saturating_sub(20);
            KeyAction::Continue
        }
        KeyCode::Char('G') => {
            // Approximate total line count (word-wrap may add more, but this
            // is a reasonable upper bound that ratatui clamps safely).
            let lines: u16 = app
                .notes
                .iter()
                .map(|n| n.content.lines().count() as u16 + 2)
                .sum();
            app.notes_scroll = lines.saturating_sub(1);
            KeyAction::Continue
        }
        KeyCode::Char('g') => {
            app.notes_scroll = 0;
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}

fn handle_add(app: &mut App, key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Esc => {
            app.cancel_add_mode();
            KeyAction::Continue
        }
        KeyCode::Tab => {
            if let Some(form) = &mut app.add_form {
                form.next_field();
            }
            KeyAction::Continue
        }
        KeyCode::BackTab => {
            if let Some(form) = &mut app.add_form {
                form.prev_field();
            }
            KeyAction::Continue
        }
        KeyCode::Enter => KeyAction::Submit,
        KeyCode::Backspace => {
            if let Some(form) = &mut app.add_form {
                form.focused_buf_mut().pop();
                form.error = None;
            }
            KeyAction::Continue
        }
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'e' {
                if let Some(form) = &app.add_form {
                    if form.focused == AddField::Note {
                        return KeyAction::OpenEditor;
                    }
                }
            } else if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                if let Some(form) = &mut app.add_form {
                    form.focused_buf_mut().clear();
                    form.error = None;
                }
            } else if let Some(form) = &mut app.add_form {
                form.focused_buf_mut().push(c);
                form.error = None;
            }
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}
