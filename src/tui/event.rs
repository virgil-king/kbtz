use crossterm::event::{KeyCode, KeyEvent};

use super::app::App;

/// Handle a key press. Returns true if the app should quit.
pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char(' ') => app.toggle_collapse(),
        KeyCode::Enter | KeyCode::Char('n') => app.toggle_notes(),
        _ => {}
    }
    false
}
