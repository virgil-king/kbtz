pub mod dashboard;
pub mod leader;
pub mod stream_view;

use crate::stream::StreamEvent;

#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Dashboard,
    Leader,
}

pub struct AppState {
    pub view: View,
    pub selected_session: Option<String>,
    pub session_events: Vec<(String, Vec<StreamEvent>)>,
    pub leader_idle: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            view: View::Dashboard,
            selected_session: None,
            session_events: vec![],
            leader_idle: true,
        }
    }

    pub fn push_event(&mut self, session_id: &str, event: StreamEvent) {
        if let Some((_, events)) = self
            .session_events
            .iter_mut()
            .find(|(id, _)| id == session_id)
        {
            events.push(event);
        } else {
            self.session_events
                .push((session_id.to_string(), vec![event]));
        }
    }

    pub fn selected_events(&self) -> &[StreamEvent] {
        if let Some(ref sid) = self.selected_session {
            if let Some((_, events)) = self.session_events.iter().find(|(id, _)| id == sid) {
                return events;
            }
        }
        &[]
    }
}
