use std::io::{Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// A single event emitted by the agent process (one JSON line from stdout).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentEvent {
    /// Monotonically increasing sequence number assigned by the shepherd.
    pub seq: u64,
    /// Unix timestamp (seconds) when the shepherd received this event.
    pub timestamp: u64,
    /// The raw JSON line from the agent, preserved as-is.
    pub data: serde_json::Value,
}

/// Messages exchanged over the Unix socket between shepherd and server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum Message {
    /// Batch of events sent on connect (history replay) or as new events arrive.
    #[serde(rename = "event_batch")]
    EventBatch { events: Vec<AgentEvent> },
    /// A single new event (shepherd -> server).
    #[serde(rename = "event")]
    Event { event: AgentEvent },
    /// Text input to send to the agent's stdin (server -> shepherd).
    #[serde(rename = "input")]
    Input { data: String },
    /// Request graceful shutdown (server -> shepherd).
    #[serde(rename = "shutdown")]
    Shutdown,
}

// ── Wire format: 4-byte BE length prefix + JSON payload ────────────────

/// Serialize a message to bytes: `[4-byte BE length][JSON payload]`.
/// Length covers only the JSON payload, not the 4-byte prefix itself.
pub fn encode(msg: &Message) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("Message serialization cannot fail");
    let length = json.len() as u32;
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&length.to_be_bytes());
    buf.extend_from_slice(&json);
    buf
}

/// Deserialize a message from a JSON payload (without length prefix).
pub fn decode(buf: &[u8]) -> Result<Message> {
    serde_json::from_slice(buf).context("failed to decode JSON message")
}

/// Read one length-framed message from a stream.
/// Maximum message size (16 MiB). Rejects frames larger than this to
/// prevent a corrupted or malicious length prefix from triggering a
/// multi-gigabyte allocation.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Returns `Ok(None)` on clean EOF, `Ok(Some(msg))` on success.
pub fn read_message(reader: &mut impl Read) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("reading message length"),
    }
    let length = u32::from_be_bytes(len_buf) as usize;
    if length == 0 {
        bail!("zero-length message frame");
    }
    if length > MAX_MESSAGE_SIZE {
        bail!("message frame too large: {length} bytes (max {MAX_MESSAGE_SIZE})");
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .context("reading message payload")?;
    let msg = decode(&payload)?;
    Ok(Some(msg))
}

/// Write one length-framed message to a stream and flush.
pub fn write_message(writer: &mut impl Write, msg: &Message) -> Result<()> {
    let data = encode(msg);
    writer.write_all(&data).context("writing message")?;
    writer.flush().context("flushing message")?;
    Ok(())
}

// ── Shepherd state file ────────────────────────────────────────────────

/// Persisted shepherd state, written atomically so readers never see
/// partial data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShepherdState {
    /// PID of the shepherd process.
    pub shepherd_pid: u32,
    /// PID of the agent child process (once spawned).
    pub child_pid: Option<u32>,
    /// Monotonic session ID (e.g. "web/1").
    pub session_id: String,
    /// Number of events in the ring buffer.
    pub event_count: u64,
    /// Sequence number of the most recent event (0 if none).
    pub last_seq: u64,
}

impl ShepherdState {
    /// Write state to a file atomically (write to temp, then rename).
    pub fn write_state_file(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec(self).context("serializing state")?;
        let dir = path
            .parent()
            .context("state file path has no parent directory")?;
        let tmp_path = dir.join(format!(
            ".{}.tmp",
            path.file_name()
                .context("state file has no filename")?
                .to_string_lossy()
        ));
        std::fs::write(&tmp_path, &json)
            .with_context(|| format!("writing temp state file {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, path)
            .with_context(|| format!("renaming temp state to {}", path.display()))?;
        Ok(())
    }

    /// Read state from a file.
    pub fn read_state_file(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("reading state file {}", path.display()))?;
        serde_json::from_slice(&data).context("deserializing state file")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_event_batch() {
        let msg = Message::EventBatch {
            events: vec![AgentEvent {
                seq: 1,
                timestamp: 1000,
                data: serde_json::json!({"type": "text", "content": "hello"}),
            }],
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_event() {
        let msg = Message::Event {
            event: AgentEvent {
                seq: 42,
                timestamp: 2000,
                data: serde_json::json!({"type": "tool_use"}),
            },
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_input() {
        let msg = Message::Input {
            data: "yes\n".to_string(),
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_shutdown() {
        let msg = Message::Shutdown;
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn read_write_message_stream() {
        let msg1 = Message::Event {
            event: AgentEvent {
                seq: 1,
                timestamp: 100,
                data: serde_json::json!("line1"),
            },
        };
        let msg2 = Message::Input {
            data: "hello".into(),
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg1).unwrap();
        write_message(&mut buf, &msg2).unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).unwrap().unwrap(), msg1);
        assert_eq!(read_message(&mut cursor).unwrap().unwrap(), msg2);
        assert!(read_message(&mut cursor).unwrap().is_none()); // EOF
    }

    #[test]
    fn read_message_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::new());
        assert!(read_message(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn zero_length_frame_is_error() {
        let mut cursor = Cursor::new(vec![0, 0, 0, 0]);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn oversized_frame_is_error() {
        // Encode a length larger than MAX_MESSAGE_SIZE.
        let huge_len = (MAX_MESSAGE_SIZE as u32) + 1;
        let mut cursor = Cursor::new(huge_len.to_be_bytes().to_vec());
        let err = read_message(&mut cursor).unwrap_err();
        assert!(
            format!("{err}").contains("too large"),
            "expected 'too large' error, got: {err}"
        );
    }

    #[test]
    fn state_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = ShepherdState {
            shepherd_pid: 1234,
            child_pid: Some(5678),
            session_id: "web/1".into(),
            event_count: 42,
            last_seq: 42,
        };
        state.write_state_file(&path).unwrap();
        let loaded = ShepherdState::read_state_file(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn state_file_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state1 = ShepherdState {
            shepherd_pid: 1,
            child_pid: None,
            session_id: "web/1".into(),
            event_count: 0,
            last_seq: 0,
        };
        state1.write_state_file(&path).unwrap();

        let state2 = ShepherdState {
            shepherd_pid: 1,
            child_pid: Some(2),
            session_id: "web/1".into(),
            event_count: 10,
            last_seq: 10,
        };
        state2.write_state_file(&path).unwrap();

        // Should see state2, not a mix
        let loaded = ShepherdState::read_state_file(&path).unwrap();
        assert_eq!(loaded, state2);

        // Temp file should be cleaned up
        assert!(!dir.path().join(".state.json.tmp").exists());
    }

    #[test]
    fn message_json_serialization_format() {
        // Verify the tagged enum format is correct for external consumers
        let msg = Message::Shutdown;
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"type":"shutdown"}"#);

        let msg = Message::Input { data: "x".into() };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "input");
        assert_eq!(parsed["data"], "x");
    }
}
