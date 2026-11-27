//! Wire protocol for the terminal-core C/S split (design doc §4).
//!
//! Frame layout mirrors `easyterm-shared`'s share codec exactly so both
//! protocols share one framing scheme:
//!
//! ```text
//! [u32 LE len][u8 type][payload]
//! len = 1 + payload.len()  (type byte + payload bytes)
//! ```
//!
//! Type namespace: `0x60..=0x6F` is reserved for the core protocol (share
//! protocol uses `0x00..=0x57`). Payload is msgpack (via `rmp_serde`), same
//! as the share codec.

use serde::{Deserialize, Serialize};

/// One-byte frame discriminants for the core protocol (0x60+ reserved range).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CoreFrameType {
    /// Client → server call (request/response quadrants).
    ClientRequest = 0x60,
    /// Server → client reply to a `ClientRequest`.
    ServerReply = 0x61,
    /// Server → client push/event/notification.
    ServerEvent = 0x62,
    /// Client → server acknowledgment of an event (when needed).
    ClientAck = 0x63,
    /// Ping/pong keepalive probe (any direction).
    Ping = 0x64,
    Pong = 0x65,
    /// Server → client question that must be answered (host interaction:
    /// kbdint/passphrase/host-key). Carries a `ServerAsk` payload.
    ServerAsk = 0x66,
    /// Client → server answer to a `ServerAsk` (echoes ask_id).
    ClientAnswer = 0x67,
}

impl TryFrom<u8> for CoreFrameType {
    type Error = u8;
    fn try_from(v: u8) -> Result<Self, u8> {
        use CoreFrameType::*;
        Ok(match v {
            0x60 => ClientRequest,
            0x61 => ServerReply,
            0x62 => ServerEvent,
            0x63 => ClientAck,
            0x64 => Ping,
            0x65 => Pong,
            0x66 => ServerAsk,
            0x67 => ClientAnswer,
            _ => return Err(v),
        })
    }
}

/// Encode one core-protocol frame.
pub fn encode_frame(msg_type: CoreFrameType, payload: &[u8]) -> Vec<u8> {
    let len = 1 + payload.len();
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(msg_type as u8);
    out.extend_from_slice(payload);
    out
}

/// Decode a complete core-protocol frame from the start of `bytes`.
/// Returns the frame type and payload; errors when the buffer holds less than
/// one full frame or an unknown type byte.
pub fn decode_frame(bytes: &[u8]) -> Result<(CoreFrameType, &[u8]), String> {
    if bytes.len() < 4 {
        return Err("frame too short".into());
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if len < 1 {
        return Err("frame length too short".into());
    }
    if bytes.len() < 4 + len {
        return Err("frame incomplete".into());
    }
    let msg_type = CoreFrameType::try_from(bytes[4]).map_err(|v| format!("unknown core frame type {v}"))?;
    let payload = &bytes[5..4 + len];
    Ok((msg_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = encode_frame(CoreFrameType::ClientRequest, b"hello");
        let (t, p) = decode_frame(&frame).unwrap();
        assert_eq!(t, CoreFrameType::ClientRequest);
        assert_eq!(p, b"hello");
    }

    #[test]
    fn empty_payload() {
        let frame = encode_frame(CoreFrameType::Ping, &[]);
        assert_eq!(frame.len(), 5);
        let (t, p) = decode_frame(&frame).unwrap();
        assert_eq!(t, CoreFrameType::Ping);
        assert!(p.is_empty());
    }

    #[test]
    fn incomplete() {
        let frame = encode_frame(CoreFrameType::ServerEvent, &[1, 2, 3]);
        assert!(decode_frame(&frame[..frame.len() - 1]).is_err());
    }

    #[test]
    fn unknown_type() {
        let mut frame = encode_frame(CoreFrameType::Pong, &[]);
        frame[4] = 0x99;
        assert!(decode_frame(&frame).is_err());
    }

    #[test]
    fn type_namespace_does_not_collide_with_share() {
        // Share protocol occupies 0x00..=0x57; core starts at 0x60.
        assert_eq!(CoreFrameType::ClientRequest as u8, 0x60);
    }
}
