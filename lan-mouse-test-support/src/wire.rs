//! Wire-level helpers for protocol tests.
//!
//! The production encoder/decoder are reused; only the legacy byte sequences
//! (which by definition no longer have a production encoder) are spelled out
//! here so compatibility tests cannot drift from the documented layout.

use lan_mouse_proto::{EventType, MAX_EVENT_SIZE, Position, ProtoEvent, ProtocolError};

/// Encode a production event exactly as it would go on the wire.
pub fn encode(event: ProtoEvent) -> Vec<u8> {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    buf[..len].to_vec()
}

/// Decode a wire datagram.
pub fn decode(bytes: &[u8]) -> Result<ProtoEvent, ProtocolError> {
    ProtoEvent::try_from(bytes)
}

/// Legacy `Enter` (pre-serial builds): event id + position, no payload.
pub fn legacy_enter_bytes(pos: Position) -> Vec<u8> {
    vec![EventType::Enter as u8, pos as u8]
}

/// Legate `Enter` with the serial appended (post-serial, pre-coordinate).
pub fn enter_with_serial_bytes(pos: Position, serial: u32) -> Vec<u8> {
    encode(ProtoEvent::Enter { pos, serial })
}

/// Legacy `Pong` (only the `alive` bool, no capability bits).
pub fn legacy_pong_bytes(alive: bool) -> Vec<u8> {
    vec![EventType::Pong as u8, u8::from(alive)]
}

/// Legacy `Hello` (8-byte commit only, no version/capabilities).
pub fn legacy_hello_bytes(commit: [u8; 8]) -> Vec<u8> {
    let mut bytes = vec![EventType::Hello as u8];
    bytes.extend_from_slice(&commit);
    bytes
}

/// Convenience constructors used across tests.
pub fn enter(pos: Position, serial: u32) -> ProtoEvent {
    ProtoEvent::Enter { pos, serial }
}

pub fn enter_at(pos: Position, serial: u32, along: f32) -> ProtoEvent {
    ProtoEvent::EnterAt { pos, serial, along }
}

pub fn leave(serial: u32) -> ProtoEvent {
    ProtoEvent::Leave(serial)
}

pub fn ack(serial: u32) -> ProtoEvent {
    ProtoEvent::Ack(serial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_roundtrip_matches_the_wire() {
        let event = enter_at(Position::Left, 7, 0.25);
        let bytes = encode(event);
        assert_eq!(encode(decode(&bytes).expect("decode")), bytes);
    }

    #[test]
    fn legacy_enter_is_accepted_with_serial_zero() {
        let decoded = decode(&legacy_enter_bytes(Position::Top)).expect("legacy enter");
        assert_eq!(encode(decoded), encode(enter(Position::Top, 0)));
    }

    #[test]
    fn legacy_hello_is_accepted_without_version_or_caps() {
        let decoded = decode(&legacy_hello_bytes(*b"legacy00")).expect("legacy hello");
        match decoded {
            ProtoEvent::Hello {
                commit,
                proto_version,
                capabilities,
                ..
            } => {
                assert_eq!(commit, *b"legacy00");
                assert_eq!(proto_version, 0);
                assert_eq!(capabilities, 0);
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[test]
    fn enter_with_serial_roundtrips() {
        let decoded = decode(&enter_with_serial_bytes(Position::Right, 42)).expect("decode");
        assert_eq!(encode(decoded), encode(enter(Position::Right, 42)));
    }
}
