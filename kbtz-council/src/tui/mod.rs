pub mod dashboard;
pub mod input;
pub mod stream_view;

use crate::orchestrator::SelectedSession;
use crate::stream::StreamEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum InputMode {
    Normal,
    Editing,
}

pub struct AppState {
    pub selected_session: Option<SelectedSession>,
    pub session_events: Vec<(SelectedSession, Vec<StreamEvent>)>,
    pub input_mode: InputMode,
    /// Scroll offset from bottom (0 = pinned to bottom / auto-scroll)
    pub scroll_offset: u16,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            selected_session: None,
            session_events: vec![],
            input_mode: InputMode::Normal,
            scroll_offset: 0,
        }
    }

    pub fn push_event(&mut self, session: &SelectedSession, event: StreamEvent) {
        if let Some((_, events)) = self
            .session_events
            .iter_mut()
            .find(|(id, _)| id == session)
        {
            events.push(event);
        } else {
            self.session_events
                .push((session.clone(), vec![event]));
        }
    }

    pub fn selected_events(&self) -> &[StreamEvent] {
        if let Some(ref sel) = self.selected_session {
            if let Some((_, events)) = self.session_events.iter().find(|(id, _)| id == sel) {
                return events;
            }
        }
        &[]
    }
}
