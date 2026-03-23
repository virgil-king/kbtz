use serde::{Deserialize, Serialize};

use crate::protocol::AgentEvent;
use crate::task_tree::TaskNode;

/// Messages sent from server to client over WebSocket.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// Full task tree snapshot (sent on connect and on changes).
    #[serde(rename = "task_tree")]
    TaskTree { tasks: Vec<TaskNode> },
    /// Session event batch (history replay on subscribe).
    #[serde(rename = "session_events")]
    SessionEvents {
        session_id: String,
        events: Vec<AgentEvent>,
    },
    /// Single new session event.
    #[serde(rename = "session_event")]
    SessionEvent {
        session_id: String,
        event: AgentEvent,
    },
    /// Session lifecycle change.
    #[serde(rename = "session_status")]
    SessionStatus {
        session_id: String,
        status: SessionStatusKind,
        task_name: Option<String>,
    },
    /// Auth result.
    #[serde(rename = "auth_result")]
    AuthResult { ok: bool },
    /// Error message.
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusKind {
    Running,
    Stopping,
    Exited,
}

/// Messages sent from client to server over WebSocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Subscribe to events from a session.
    #[serde(rename = "subscribe")]
    Subscribe { session_id: String },
    /// Send input to a session's agent.
    #[serde(rename = "input")]
    Input { session_id: String, data: String },
    /// Request current task tree.
    #[serde(rename = "get_tree")]
    GetTree,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_message_serialization() {
        let msg = ServerMessage::AuthResult { ok: true };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "auth_result");
        assert_eq!(parsed["ok"], true);
    }

    #[test]
    fn client_message_deserialization() {
        let json = r#"{"type":"subscribe","session_id":"ws/1"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Subscribe { session_id } if session_id == "ws/1"));
    }

    #[test]
    fn client_input_deserialization() {
        let json = r#"{"type":"input","session_id":"ws/2","data":"yes\n"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert!(matches!(msg, ClientMessage::Input { session_id, data } if session_id == "ws/2" && data == "yes\n"));
    }

    #[test]
    fn session_status_serialization() {
        let msg = ServerMessage::SessionStatus {
            session_id: "ws/1".into(),
            status: SessionStatusKind::Running,
            task_name: Some("my-task".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "session_status");
        assert_eq!(parsed["status"], "running");
    }
}
