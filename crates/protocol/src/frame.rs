//! Length-prefixed JSON framing and protocol errors.
//!
//! A frame is `u32` big-endian length followed by that many bytes of UTF-8
//! JSON. The length is the exact payload size; the four-byte prefix is not
//! included. The decoder enforces the caller's limit **before** allocating the
//! payload and never panics on malformed input.
//!
//! All helpers operate on blocking `Read`/`Write` values. Callers own their
//! timeouts: for a [`std::net::TcpStream`], set
//! [`std::net::TcpStream::set_read_timeout`] and
//! [`std::net::TcpStream::set_write_timeout`] before calling them. A read
//! blocks until a complete frame arrives or the underlying read returns an
//! error/timeout.

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest frame accepted by the default helpers (2 MiB).
pub const MAX_FRAME_LEN: usize = 2 * 1024 * 1024;

/// Errors produced by framing, JSON and chunk decoding.
#[derive(Debug)]
pub enum ProtocolError {
    /// The underlying reader or writer failed (including timeouts).
    Io(io::Error),
    /// A message could not be serialised to JSON or decoded from JSON.
    Json(serde_json::Error),
    /// A frame declared or used more bytes than the caller's limit.
    FrameTooLarge {
        /// Observed or requested payload length in bytes.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
    /// A frame declared a zero-length payload.
    EmptyFrame,
    /// The peer closed the connection in the middle of a frame (a partial
    /// length prefix or payload). Distinct from a clean close between frames so
    /// the server can report a machine-readable rejection reason.
    TruncatedFrame,
    /// A requested player name is invalid.
    InvalidName(&'static str),
    /// A chat line is invalid.
    InvalidChat(&'static str),
    /// A chunk payload is malformed or exceeds a protocol bound.
    InvalidChunk(&'static str),
    /// A light-section patch is malformed or exceeds a protocol bound.
    InvalidLight(&'static str),
    /// An encoded chunk would not fit in the protocol's chunk budget.
    ChunkTooLarge {
        /// Encoded payload length in bytes.
        len: usize,
        /// Configured maximum.
        max: usize,
    },
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(error) => write!(formatter, "i/o error: {error}"),
            ProtocolError::Json(error) => write!(formatter, "json error: {error}"),
            ProtocolError::FrameTooLarge { len, max } => {
                write!(formatter, "frame of {len} bytes exceeds the {max}-byte limit")
            }
            ProtocolError::EmptyFrame => write!(formatter, "frame payload is empty"),
            ProtocolError::TruncatedFrame => write!(formatter, "connection closed in the middle of a frame"),
            ProtocolError::InvalidName(reason) => write!(formatter, "invalid player name: {reason}"),
            ProtocolError::InvalidChat(reason) => write!(formatter, "invalid chat text: {reason}"),
            ProtocolError::InvalidChunk(reason) => write!(formatter, "invalid chunk payload: {reason}"),
            ProtocolError::InvalidLight(reason) => write!(formatter, "invalid light patch: {reason}"),
            ProtocolError::ChunkTooLarge { len, max } => {
                write!(formatter, "encoded chunk of {len} bytes exceeds the {max}-byte limit")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProtocolError::Io(error) => Some(error),
            ProtocolError::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        ProtocolError::Io(error)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(error: serde_json::Error) -> Self {
        ProtocolError::Json(error)
    }
}

/// Serialises `message` and prefixes it with a big-endian `u32` length.
///
/// The default limit is [`MAX_FRAME_LEN`].
pub fn encode_frame<T: Serialize + ?Sized>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    encode_frame_with_limit(message, MAX_FRAME_LEN)
}

/// Serialises `message` and prefixes it with a big-endian `u32` length,
/// rejecting payloads longer than `max_len`.
pub fn encode_frame_with_limit<T: Serialize + ?Sized>(message: &T, max_len: usize) -> Result<Vec<u8>, ProtocolError> {
    let payload = serde_json::to_vec(message)?;
    if payload.len() > max_len {
        return Err(ProtocolError::FrameTooLarge {
            len: payload.len(),
            max: max_len,
        });
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    let length = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge {
        len: payload.len(),
        max: max_len,
    })?;
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decodes a complete frame (including its length prefix) into `T`.
///
/// `bytes` must contain exactly one frame; the length prefix is checked
/// against `max_len` before the JSON payload is parsed.
pub fn decode_frame<T: DeserializeOwned>(bytes: &[u8], max_len: usize) -> Result<T, ProtocolError> {
    if bytes.len() < 4 {
        return Err(ProtocolError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "frame header is truncated",
        )));
    }
    let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if declared == 0 {
        return Err(ProtocolError::EmptyFrame);
    }
    if declared > max_len {
        return Err(ProtocolError::FrameTooLarge {
            len: declared,
            max: max_len,
        });
    }
    if bytes.len() != 4 + declared {
        return Err(ProtocolError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "frame payload length does not match the header",
        )));
    }
    let payload = &bytes[4..];
    if payload.iter().all(u8::is_ascii_whitespace) {
        return Err(ProtocolError::EmptyFrame);
    }
    Ok(serde_json::from_slice(payload)?)
}

/// Reads one frame payload from `reader`.
///
/// The returned vector excludes the four-byte prefix. A declared length above
/// `max_len` is rejected before allocating. The caller must have configured a
/// read timeout on blocking sockets.
pub fn read_frame<R: Read>(reader: &mut R, max_len: usize) -> Result<Vec<u8>, ProtocolError> {
    let header = match read_header(reader)? {
        Some(header) => header,
        // A clean EOF between frames is the normal close path; the caller decides
        // whether to log or report it. A partial header is a truncated frame.
        None => {
            return Err(ProtocolError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed between frames",
            )));
        }
    };
    let declared = u32::from_be_bytes(header) as usize;
    if declared == 0 {
        return Err(ProtocolError::EmptyFrame);
    }
    if declared > max_len {
        return Err(ProtocolError::FrameTooLarge {
            len: declared,
            max: max_len,
        });
    }
    let mut payload = vec![0u8; declared];
    if let Err(error) = reader.read_exact(&mut payload) {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            return Err(ProtocolError::TruncatedFrame);
        }
        return Err(ProtocolError::Io(error));
    }
    Ok(payload)
}

/// Reads a four-byte length prefix, distinguishing a clean close from a
/// truncated prefix.
///
/// Returns `Ok(None)` only when the peer closed the connection before the first
/// header byte. Returning a typed error for a partial header (rather than an
/// `io::Error`) lets the server send a `Disconnect` reason.
fn read_header<R: Read>(reader: &mut R) -> Result<Option<[u8; 4]>, ProtocolError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ProtocolError::TruncatedFrame),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ProtocolError::Io(error)),
        }
    }
    Ok(Some(header))
}

/// Decodes one JSON payload (the bytes after the `u32` length prefix).
///
/// Use this after [`read_frame`]; [`decode_frame`] expects the length prefix
/// to still be present. The payload is rejected when empty or larger than
/// `max_len` before JSON parsing.
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8], max_len: usize) -> Result<T, ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::EmptyFrame);
    }
    if payload.len() > max_len {
        return Err(ProtocolError::FrameTooLarge {
            len: payload.len(),
            max: max_len,
        });
    }
    Ok(serde_json::from_slice(payload)?)
}

/// Reads one framed JSON message from `reader`.
pub fn read_message<R: Read, T: DeserializeOwned>(reader: &mut R, max_len: usize) -> Result<T, ProtocolError> {
    let payload = read_frame(reader, max_len)?;
    decode_payload(&payload, max_len)
}

/// Writes one framed JSON message to `writer` using [`MAX_FRAME_LEN`].
///
/// The writer is not flushed; flush explicitly when using a buffered writer.
pub fn write_message<W: Write, T: Serialize + ?Sized>(writer: &mut W, message: &T) -> Result<(), ProtocolError> {
    write_message_with_limit(writer, message, MAX_FRAME_LEN)
}

/// Writes one framed JSON message, rejecting payloads longer than `max_len`.
pub fn write_message_with_limit<W: Write, T: Serialize + ?Sized>(
    writer: &mut W,
    message: &T,
    max_len: usize,
) -> Result<(), ProtocolError> {
    let frame = encode_frame_with_limit(message, max_len)?;
    writer.write_all(&frame)?;
    Ok(())
}

/// Writes an already encoded frame (length prefix plus payload).
pub fn write_frame<W: Write>(writer: &mut W, frame: &[u8]) -> Result<(), ProtocolError> {
    writer.write_all(frame)?;
    Ok(())
}

/// Convenience wrapper for [`ClientMessage`](crate::ClientMessage).
pub fn encode_client_frame(message: &crate::ClientMessage) -> Result<Vec<u8>, ProtocolError> {
    encode_frame(message)
}

/// Convenience wrapper for [`ClientMessage`](crate::ClientMessage).
pub fn decode_client_frame(bytes: &[u8], max_len: usize) -> Result<crate::ClientMessage, ProtocolError> {
    decode_frame(bytes, max_len)
}

/// Convenience wrapper for [`ServerMessage`](crate::ServerMessage).
pub fn encode_server_frame(message: &crate::ServerMessage) -> Result<Vec<u8>, ProtocolError> {
    encode_frame(message)
}

/// Convenience wrapper for [`ServerMessage`](crate::ServerMessage).
pub fn decode_server_frame(bytes: &[u8], max_len: usize) -> Result<crate::ServerMessage, ProtocolError> {
    decode_frame(bytes, max_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ClientMessage, ServerMessage};

    fn client_messages() -> Vec<ClientMessage> {
        vec![
            ClientMessage::Hello {
                protocol: crate::PROTOCOL_VERSION,
                name: "alice".to_string(),
            },
            ClientMessage::ChunkRequest { cx: -3, cz: 9 },
            ClientMessage::Move {
                pos: [1.5, 64.25, -9.0],
                yaw: 0.5,
                pitch: -0.25,
            },
            ClientMessage::BlockEdit {
                pos: [1, 2, 3],
                block: 4,
            },
            ClientMessage::Chat {
                text: "hello world".to_string(),
            },
            ClientMessage::Ping { id: 0xdead_beef },
            ClientMessage::Disconnect,
        ]
    }

    fn server_messages() -> Vec<ServerMessage> {
        vec![
            ServerMessage::Hello {
                protocol: crate::PROTOCOL_VERSION,
                world_name: "world".to_string(),
                seed: 42,
                spawn: [0.5, 70.0, 0.5],
                content_fingerprint: 0x1234,
                tick: 7,
                day_ticks: crate::DAY_LENGTH_TICKS,
                max_players: 8,
                motd: "hello".to_string(),
                blocks: vec![crate::BlockDescriptor {
                    id: 0,
                    name: "openblock:air".to_string(),
                }],
            },
            ServerMessage::Join {
                id: 1,
                name: "alice".to_string(),
            },
            ServerMessage::Leave { id: 1 },
            ServerMessage::PlayerList {
                players: vec![crate::PlayerSnapshot {
                    id: 1,
                    name: "alice".to_string(),
                    pos: [0.0, 64.0, 0.0],
                    yaw: 0.0,
                    pitch: 0.0,
                }],
            },
            ServerMessage::PlayerState {
                id: 1,
                name: "alice".to_string(),
                pos: [0.0, 64.0, 0.0],
                yaw: 0.0,
                pitch: 0.0,
            },
            ServerMessage::ChunkData {
                cx: -1,
                cz: 2,
                bytes: vec![0, 1, 2, 250, 255],
            },
            ServerMessage::BlockEdit {
                pos: [1, 2, 3],
                block: 4,
                player: 1,
            },
            ServerMessage::Chat {
                from: "alice".to_string(),
                text: "hi".to_string(),
            },
            ServerMessage::Pong { id: 9 },
            ServerMessage::Disconnect {
                reason: "server_shutdown".to_string(),
            },
        ]
    }

    #[test]
    fn every_client_message_round_trips() {
        for message in client_messages() {
            let frame = encode_client_frame(&message).expect("encode");
            assert_eq!(
                frame.len(),
                4 + u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize
            );
            let decoded = decode_client_frame(&frame, MAX_FRAME_LEN).expect("decode");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn every_server_message_round_trips() {
        for message in server_messages() {
            let frame = encode_server_frame(&message).expect("encode");
            let decoded = decode_server_frame(&frame, MAX_FRAME_LEN).expect("decode");
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn chunk_bytes_are_base64_on_the_wire() {
        let message = ServerMessage::ChunkData {
            cx: 0,
            cz: 0,
            bytes: b"hello".to_vec(),
        };
        let frame = encode_server_frame(&message).expect("encode");
        let json = std::str::from_utf8(&frame[4..]).expect("utf8");
        assert!(json.contains("\"bytes\":\"aGVsbG8=\""), "{json}");
    }

    #[test]
    fn oversized_frames_are_rejected_before_parsing() {
        let frame = encode_client_frame(&ClientMessage::Ping { id: 1 }).expect("encode");
        assert!(matches!(
            decode_client_frame(&frame, 4),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
        let mut header = [0u8; 4];
        header.copy_from_slice(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes());
        let mut bytes = header.to_vec();
        bytes.push(b'{');
        assert!(matches!(
            read_frame(&mut bytes.as_slice(), MAX_FRAME_LEN),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn truncated_frames_and_invalid_json_are_errors_not_panics() {
        assert!(matches!(
            decode_client_frame(&[], MAX_FRAME_LEN),
            Err(ProtocolError::Io(_))
        ));
        let truncated = [0u8, 1, 0, 2];
        assert!(matches!(
            decode_client_frame(&truncated, MAX_FRAME_LEN),
            Err(ProtocolError::Io(_))
        ));
        let empty = [0u8, 0, 0, 0];
        assert!(matches!(
            decode_client_frame(&empty, MAX_FRAME_LEN),
            Err(ProtocolError::EmptyFrame)
        ));
        let mut invalid_json = vec![0, 0, 0, 3];
        invalid_json.extend_from_slice(b"not");
        assert!(matches!(
            decode_client_frame(&invalid_json, MAX_FRAME_LEN),
            Err(ProtocolError::Json(_))
        ));
        let mut bad_utf8 = vec![0, 0, 0, 2];
        bad_utf8.extend_from_slice(&[0xff, 0xfe]);
        assert!(matches!(
            decode_client_frame(&bad_utf8, MAX_FRAME_LEN),
            Err(ProtocolError::Json(_))
        ));
    }

    #[test]
    fn partial_reads_are_typed_truncated_frames_and_clean_eof_is_io() {
        // A partial length prefix and a partial payload report TruncatedFrame.
        for partial in [&[0u8][..], &[0u8, 0][..], &[0u8, 0, 0][..]] {
            assert!(matches!(
                read_frame(&mut &*partial, MAX_FRAME_LEN),
                Err(ProtocolError::TruncatedFrame)
            ));
        }
        let mut partial_payload = vec![0u8, 0, 0, 8];
        partial_payload.extend_from_slice(b"{\"type\"");
        assert!(matches!(
            read_frame(&mut partial_payload.as_slice(), MAX_FRAME_LEN),
            Err(ProtocolError::TruncatedFrame)
        ));

        // No bytes at all is a clean close between frames, not a protocol error.
        assert!(matches!(
            read_frame(&mut &[][..], MAX_FRAME_LEN),
            Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));

        // A zero-length declared frame is rejected before any payload read.
        assert!(matches!(
            read_frame(&mut &[0u8, 0, 0, 0][..], MAX_FRAME_LEN),
            Err(ProtocolError::EmptyFrame)
        ));
    }

    #[test]
    fn typed_read_and_write_helpers_round_trip() {
        let message = ServerMessage::Pong { id: 7 };
        let mut bytes = Vec::new();
        write_message(&mut bytes, &message).expect("write");
        let decoded: ServerMessage = read_message(&mut bytes.as_slice(), MAX_FRAME_LEN).expect("read");
        assert_eq!(decoded, message);
    }

    #[test]
    fn deterministic_random_mutations_never_panic() {
        let base = encode_client_frame(&ClientMessage::BlockEdit {
            pos: [1, 2, 3],
            block: 7,
        })
        .expect("encode");
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for iteration in 0..1_000 {
            let mut mutated = base.clone();
            for _ in 0..(iteration % 5 + 1) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let index = (state as usize) % mutated.len();
                mutated[index] = (state >> 32) as u8;
            }
            let result = std::panic::catch_unwind(|| decode_client_frame(&mutated, MAX_FRAME_LEN));
            assert!(result.is_ok(), "decoder panicked on mutation {iteration}");
        }
    }
}
