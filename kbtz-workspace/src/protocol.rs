use std::io::{Read, Write};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// PTY output from child (shepherd -> workspace). Type 0x01.
    PtyOutput(Vec<u8>),
    /// Keyboard input to child (workspace -> shepherd). Type 0x02.
    PtyInput(Vec<u8>),
    /// Terminal resize (workspace -> shepherd). Type 0x03.
    Resize { rows: u16, cols: u16 },
    /// Full output buffer for state recovery on connect (shepherd -> workspace). Type 0x04.
    InitialState(Vec<u8>),
    /// Request graceful shutdown (workspace -> shepherd). Type 0x05.
    Shutdown,
}

const TYPE_PTY_OUTPUT: u8 = 0x01;
const TYPE_PTY_INPUT: u8 = 0x02;
const TYPE_RESIZE: u8 = 0x03;
const TYPE_INITIAL_STATE: u8 = 0x04;
const TYPE_SHUTDOWN: u8 = 0x05;

/// Serialize a message to bytes using the wire format:
/// `[4 bytes big-endian length] [1 byte type] [payload]`
///
/// Length includes the type byte but NOT the 4-byte length prefix.
pub fn encode(msg: &Message) -> Vec<u8> {
    let (type_byte, payload) = match msg {
        Message::PtyOutput(data) => (TYPE_PTY_OUTPUT, data.as_slice()),
        Message::PtyInput(data) => (TYPE_PTY_INPUT, data.as_slice()),
        Message::Resize { rows, cols } => {
            // Handled specially below since we need to build the payload.
            let mut buf = Vec::with_capacity(4 + 1 + 4);
            let length: u32 = 1 + 4; // type byte + 4 bytes payload
            buf.extend_from_slice(&length.to_be_bytes());
            buf.push(TYPE_RESIZE);
            buf.extend_from_slice(&rows.to_be_bytes());
            buf.extend_from_slice(&cols.to_be_bytes());
            return buf;
        }
        Message::InitialState(data) => (TYPE_INITIAL_STATE, data.as_slice()),
        Message::Shutdown => (TYPE_SHUTDOWN, [].as_slice()),
    };

    let length: u32 = 1 + payload.len() as u32; // type byte + payload
    let mut buf = Vec::with_capacity(4 + length as usize);
    buf.extend_from_slice(&length.to_be_bytes());
    buf.push(type_byte);
    buf.extend_from_slice(payload);
    buf
}

/// Deserialize a message from a complete frame buffer (type byte + payload, no length prefix).
pub fn decode(buf: &[u8]) -> Result<Message> {
    if buf.is_empty() {
        bail!("empty frame buffer");
    }

    let type_byte = buf[0];
    let payload = &buf[1..];

    match type_byte {
        TYPE_PTY_OUTPUT => Ok(Message::PtyOutput(payload.to_vec())),
        TYPE_PTY_INPUT => Ok(Message::PtyInput(payload.to_vec())),
        TYPE_RESIZE => {
            if payload.len() < 4 {
                bail!(
                    "resize payload too short: expected 4 bytes, got {}",
                    payload.len()
                );
            }
            let rows = u16::from_be_bytes([payload[0], payload[1]]);
            let cols = u16::from_be_bytes([payload[2], payload[3]]);
            Ok(Message::Resize { rows, cols })
        }
        TYPE_INITIAL_STATE => Ok(Message::InitialState(payload.to_vec())),
        TYPE_SHUTDOWN => Ok(Message::Shutdown),
        _ => bail!("unknown message type: 0x{:02x}", type_byte),
    }
}

/// Read one framed message from a reader. Returns `None` on clean EOF
/// (zero bytes read when expecting the length prefix).
pub fn read_message(reader: &mut impl Read) -> Result<Option<Message>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("failed to read message length"),
    }

    let length = u32::from_be_bytes(len_buf) as usize;
    if length == 0 {
        bail!("invalid zero-length frame");
    }

    let mut frame_buf = vec![0u8; length];
    reader
        .read_exact(&mut frame_buf)
        .context("failed to read message frame")?;

    decode(&frame_buf).map(Some)
}

/// Write one framed message to a writer.
pub fn write_message(writer: &mut impl Write, msg: &Message) -> Result<()> {
    let encoded = encode(msg);
    writer
        .write_all(&encoded)
        .context("failed to write message")?;
    writer.flush().context("failed to flush message")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_pty_output() {
        let msg = Message::PtyOutput(b"hello world".to_vec());
        let encoded = encode(&msg);
        // Skip the 4-byte length prefix for decode
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_pty_input() {
        let msg = Message::PtyInput(b"keystrokes".to_vec());
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_resize() {
        let msg = Message::Resize {
            rows: 24,
            cols: 80,
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn roundtrip_initial_state() {
        let data: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let msg = Message::InitialState(data);
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
    fn decode_empty_fails() {
        let result = decode(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn decode_truncated_fails() {
        // Resize needs 4 bytes of payload but we only give 2
        let buf = [TYPE_RESIZE, 0x00, 0x18];
        let result = decode(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn read_message_from_stream() {
        let msg = Message::PtyOutput(b"stream test".to_vec());
        let encoded = encode(&msg);
        let mut cursor = Cursor::new(encoded);
        let decoded = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn read_message_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let result = read_message(&mut cursor).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_then_read_message() {
        let msg = Message::Resize {
            rows: 50,
            cols: 120,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(msg, decoded);
    }
}
