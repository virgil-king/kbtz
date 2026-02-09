use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::app::{App, Mode};

/// Result of handling a key press.
pub enum KeyAction {
    Quit,
    Submit,
    Refresh,
    Continue,
}

/// Handle a key press. Returns an action indicating what the event loop should do.
pub fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::AddTask => handle_add(app, key),
        Mode::Help => handle_help(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => {
            app.move_down();
            KeyAction::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.move_up();
            KeyAction::Continue
        }
        KeyCode::Char(' ') => {
            app.toggle_collapse();
            KeyAction::Refresh
        }
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
        KeyCode::Char('?') => {
            app.toggle_help();
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
            if key.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
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

fn handle_help(app: &mut App, key: KeyEvent) -> KeyAction {
    match key.code {
        KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
            app.toggle_help();
            KeyAction::Continue
        }
        _ => KeyAction::Continue,
    }
}
