use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

pub mod clipboard;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// Wire protocol version implemented by this build. Bump when a change is
/// not representable as an additive, backward-compatible extension.
pub const PROTO_VERSION: u8 = 1;

/// Optional protocol features advertised in [`ProtoEvent::Hello`].
///
/// Bits are reserved for features that require both peers to opt in. A
/// peer that does not understand a bit simply ignores it.
pub mod capabilities {
    /// Reserved: batching several input events into one datagram.
    pub const BATCHED_INPUT: u32 = 1 << 0;
    /// Reserved: clock synchronisation for end-to-end latency measurement.
    pub const TIME_SYNC: u32 = 1 << 1;
    /// Reserved: keyboard layout negotiation.
    pub const KEYMAP_V1: u32 = 1 << 2;
    /// The peer echoes `Enter` serials back in [`super::ProtoEvent::Ack`] and
    /// drops duplicate serials. Peers without this bit predate the serial
    /// field: they ignore `Ack` and never acknowledge an `Enter`, so the
    /// sender must not wait for one (that would tear down a healthy grab).
    pub const ENTER_ACK: u32 = 1 << 3;
    /// The peer speaks the clipboard sync extension (frames 12-15).
    /// Legacy peers cannot decode those frames, so nothing must be offered
    /// to them.
    pub const CLIPBOARD: u32 = 1 << 4;
    /// The peer understands the permission negotiation events
    /// ([`super::ProtoEvent::AuthRequest`], [`super::ProtoEvent::AuthResponse`]
    /// and [`super::ProtoEvent::AuthRevoke`]). Legacy peers close the
    /// connection on an unknown event id, so these must only be sent when the
    /// peer advertised this bit.
    pub const PERMISSIONS: u32 = 1 << 5;
}

/// Features this build can use when the peer also advertises them.
pub const CAPABILITIES: u32 =
    capabilities::ENTER_ACK | capabilities::CLIPBOARD | capabilities::PERMISSIONS;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// the buffer ended before the event was fully decoded
    #[error("truncated event: not enough bytes to decode")]
    Truncated,
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position.
    /// `serial` is echoed back in [`ProtoEvent::Ack`] so the sender can
    /// retransmit a lost `Enter` and the receiver can drop duplicates.
    /// Peers predating the serial field send only the position; the decoder
    /// accepts that (serial `0`) for backward compatibility.
    Enter { pos: Position, serial: u32 },
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`]. `alive` is true if emulation is
    /// enabled / available.
    ///
    /// Peers from this fork append the optional capabilities they support.
    /// Legacy peers send only the bool, and their decoders ignore the
    /// trailing bytes, which is why this extension cannot break them (unlike
    /// a new event type: releases before `Hello` existed close the
    /// connection when they see an unknown event id).
    Pong {
        alive: bool,
        capabilities: Option<u32>,
    },
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`.
    ///
    /// `proto_version` and `capabilities` are only present on peers
    /// that support the extended handshake; a legacy 8-byte `Hello`
    /// decodes with version `0` and no capabilities.
    Hello {
        commit: [u8; 8],
        proto_version: u8,
        capabilities: u32,
    },
    /// Announces or updates the permissions this side grants to the peer and
    /// expects from it. Only sent when the peer advertises
    /// [`capabilities::PERMISSIONS`]. Idempotent: a new request replaces the
    /// previous grants for that session.
    AuthRequest {
        /// per-connection identifier so stale updates can be ignored
        session_id: u128,
        /// capabilities this side grants the peer in the incoming direction
        incoming_caps: u8,
        /// capabilities this side expects/uses in the outgoing direction
        outgoing_caps: u8,
    },
    /// Reciprocal grant, sent in reply to [`ProtoEvent::AuthRequest`].
    AuthResponse {
        session_id: u128,
        incoming_caps: u8,
        outgoing_caps: u8,
    },
    /// Tears down the permission grants of a session (device revoked, link
    /// released). `reason` is a small enum: 0 = revoked, 1 = released,
    /// 2 = policy changed to deny.
    AuthRevoke { session_id: u128, reason: u8 },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter { pos, serial } => write!(f, "Enter({pos}, {serial})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong {
                alive,
                capabilities,
            } => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )?;
                if let Some(capabilities) = capabilities {
                    write!(f, " (caps={capabilities:#x})")?;
                }
                Ok(())
            }
            ProtoEvent::Hello {
                commit,
                proto_version,
                capabilities,
            } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s}, v{proto_version}, caps={capabilities:#x})")
            }
            ProtoEvent::AuthRequest {
                session_id,
                incoming_caps,
                outgoing_caps,
            } => write!(
                f,
                "AuthRequest(session={session_id:#x}, in={incoming_caps:#x}, out={outgoing_caps:#x})"
            ),
            ProtoEvent::AuthResponse {
                session_id,
                incoming_caps,
                outgoing_caps,
            } => write!(
                f,
                "AuthResponse(session={session_id:#x}, in={incoming_caps:#x}, out={outgoing_caps:#x})"
            ),
            ProtoEvent::AuthRevoke { session_id, reason } => {
                write!(f, "AuthRevoke(session={session_id:#x}, reason={reason})")
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    // Ids 12-15 are reserved for clipboard frames; new protocol events start
    // at 16 so the clipboard detection in the read loops keeps working.
    AuthRequest = 16,
    AuthResponse = 17,
    AuthRevoke = 18,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong { .. } => EventType::Pong,
            ProtoEvent::Enter { .. } => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::AuthRequest { .. } => EventType::AuthRequest,
            ProtoEvent::AuthResponse { .. } => EventType::AuthResponse,
            ProtoEvent::AuthRevoke { .. } => EventType::AuthRevoke,
        }
    }
}

impl TryFrom<&[u8]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: &[u8]) -> Result<Self, Self::Error> {
        let mut buf = buf;
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => {
                let alive = decode_u8(&mut buf)? != 0;
                // Peers from this fork append their capabilities; legacy
                // peers send only the flag.
                let capabilities = if buf.len() >= size_of::<u32>() {
                    Some(decode_u32(&mut buf)?)
                } else {
                    None
                };
                Ok(Self::Pong {
                    alive,
                    capabilities,
                })
            }
            EventType::Enter => {
                let pos = decode_u8(&mut buf)?.try_into()?;
                // Legacy peers only send the position; newer peers append
                // the serial. Absence of the field means serial 0.
                let serial = if buf.len() >= size_of::<u32>() {
                    decode_u32(&mut buf)?
                } else {
                    0
                };
                Ok(Self::Enter { pos, serial })
            }
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                let proto_version = if buf.len() >= size_of::<u8>() {
                    decode_u8(&mut buf)?
                } else {
                    0
                };
                let capabilities = if buf.len() >= size_of::<u32>() {
                    decode_u32(&mut buf)?
                } else {
                    0
                };
                Ok(Self::Hello {
                    commit,
                    proto_version,
                    capabilities,
                })
            }
            EventType::AuthRequest => Ok(Self::AuthRequest {
                session_id: decode_u128(&mut buf)?,
                incoming_caps: decode_u8(&mut buf)?,
                outgoing_caps: decode_u8(&mut buf)?,
            }),
            EventType::AuthResponse => Ok(Self::AuthResponse {
                session_id: decode_u128(&mut buf)?,
                incoming_caps: decode_u8(&mut buf)?,
                outgoing_caps: decode_u8(&mut buf)?,
            }),
            EventType::AuthRevoke => Ok(Self::AuthRevoke {
                session_id: decode_u128(&mut buf)?,
                reason: decode_u8(&mut buf)?,
            }),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong {
                    alive,
                    capabilities,
                } => {
                    encode_u8(buf, len, alive as u8);
                    if let Some(capabilities) = capabilities {
                        encode_u32(buf, len, capabilities);
                    }
                }
                ProtoEvent::Enter { pos, serial } => {
                    encode_u8(buf, len, pos as u8);
                    encode_u32(buf, len, serial);
                }
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello {
                    commit,
                    proto_version,
                    capabilities,
                } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                    encode_u8(buf, len, proto_version);
                    encode_u32(buf, len, capabilities);
                }
                ProtoEvent::AuthRequest {
                    session_id,
                    incoming_caps,
                    outgoing_caps,
                } => {
                    encode_u128(buf, len, session_id);
                    encode_u8(buf, len, incoming_caps);
                    encode_u8(buf, len, outgoing_caps);
                }
                ProtoEvent::AuthResponse {
                    session_id,
                    incoming_caps,
                    outgoing_caps,
                } => {
                    encode_u128(buf, len, session_id);
                    encode_u8(buf, len, incoming_caps);
                    encode_u8(buf, len, outgoing_caps);
                }
                ProtoEvent::AuthRevoke { session_id, reason } => {
                    encode_u128(buf, len, session_id);
                    encode_u8(buf, len, reason);
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data
                    .split_at_checked(size_of::<$t>())
                    .ok_or(ProtocolError::Truncated)?;
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u128);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u128);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(event: ProtoEvent) -> Vec<u8> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        buf[..len].to_vec()
    }

    fn roundtrip(event: ProtoEvent) {
        let bytes = encode(event);
        let decoded = ProtoEvent::try_from(bytes.as_slice()).expect("decode failed");
        assert_eq!(encode(decoded), bytes, "roundtrip mismatch for {event}");
    }

    fn sample_events() -> Vec<ProtoEvent> {
        vec![
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: 42,
                dx: 1.5,
                dy: -2.25,
            })),
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
                time: 7,
                button: 0x110,
                state: 1,
            })),
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: 9,
                axis: 0,
                value: -120.0,
            })),
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 1,
                value: 3,
            })),
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: 11,
                key: 30,
                state: 1,
            })),
            ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: 1,
                latched: 2,
                locked: 3,
                group: 4,
            })),
            ProtoEvent::Ping,
            ProtoEvent::Pong {
                alive: true,
                capabilities: None,
            },
            ProtoEvent::Enter {
                pos: Position::Right,
                serial: 7,
            },
            ProtoEvent::Leave(0x0102_0304),
            ProtoEvent::Ack(0x0a0b_0c0d),
            ProtoEvent::Hello {
                commit: *b"abcdefgh",
                proto_version: PROTO_VERSION,
                capabilities: CAPABILITIES,
            },
            ProtoEvent::AuthRequest {
                session_id: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10,
                incoming_caps: 0b101,
                outgoing_caps: 0b011,
            },
            ProtoEvent::AuthResponse {
                session_id: 7,
                incoming_caps: 0,
                outgoing_caps: 0xff,
            },
            ProtoEvent::AuthRevoke {
                session_id: u128::MAX,
                reason: 2,
            },
        ]
    }

    #[test]
    fn roundtrip_all_events() {
        for event in sample_events() {
            roundtrip(event);
        }
    }

    #[test]
    fn golden_vectors() {
        assert_eq!(encode(ProtoEvent::Ping), vec![6]);
        assert_eq!(
            encode(ProtoEvent::Pong {
                alive: false,
                capabilities: None
            }),
            vec![7, 0]
        );
        assert_eq!(
            encode(ProtoEvent::Pong {
                alive: true,
                capabilities: None
            }),
            vec![7, 1]
        );
        // A peer from this fork appends its capabilities; legacy peers
        // ignore the trailing bytes.
        assert_eq!(
            encode(ProtoEvent::Pong {
                alive: true,
                capabilities: Some(0x11)
            }),
            vec![7, 1, 0, 0, 0, 0x11]
        );
        assert_eq!(
            encode(ProtoEvent::Enter {
                pos: Position::Top,
                serial: 0x0102_0304,
            }),
            vec![8, 2, 1, 2, 3, 4]
        );
        assert_eq!(encode(ProtoEvent::Leave(0x0102_0304)), vec![9, 1, 2, 3, 4]);
        assert_eq!(
            encode(ProtoEvent::Ack(0x0a0b_0c0d)),
            vec![10, 0x0a, 0x0b, 0x0c, 0x0d]
        );
        assert_eq!(
            encode(ProtoEvent::Hello {
                commit: *b"abcdefgh",
                proto_version: 1,
                capabilities: 0x0102_0304,
            }),
            vec![
                11, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h', 1, 1, 2, 3, 4
            ]
        );
    }

    #[test]
    fn truncated_input_is_an_error_never_a_panic() {
        for event in sample_events() {
            let bytes = encode(event);
            // `Enter` and `Hello` accept legacy short forms, so only
            // require that no prefix ever panics for them.
            if matches!(event, ProtoEvent::Enter { .. } | ProtoEvent::Hello { .. }) {
                for n in 0..=bytes.len() {
                    let _ = ProtoEvent::try_from(&bytes[..n]);
                }
                continue;
            }
            for n in 0..bytes.len() {
                assert!(
                    ProtoEvent::try_from(&bytes[..n]).is_err(),
                    "expected truncation error for {event} at {n}/{}",
                    bytes.len()
                );
            }
        }
        assert!(matches!(
            ProtoEvent::try_from(&[][..]),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn legacy_enter_without_serial_decodes_as_serial_zero() {
        let decoded = ProtoEvent::try_from(&[8u8, 1][..]).expect("legacy enter");
        assert_eq!(
            encode(decoded),
            encode(ProtoEvent::Enter {
                pos: Position::Right,
                serial: 0
            })
        );
    }

    #[test]
    fn legacy_hello_decodes_with_zero_version_and_capabilities() {
        let mut bytes = vec![11u8];
        bytes.extend_from_slice(b"abcdefgh");
        let decoded = ProtoEvent::try_from(bytes.as_slice()).expect("legacy hello");
        assert_eq!(
            encode(decoded),
            encode(ProtoEvent::Hello {
                commit: *b"abcdefgh",
                proto_version: 0,
                capabilities: 0,
            })
        );
    }

    #[test]
    fn unknown_event_id_is_rejected() {
        for id in [12u8, 13, 200, 255] {
            match ProtoEvent::try_from(&[id][..]) {
                Err(ProtocolError::InvalidEventId(_)) => {}
                other => panic!("expected InvalidEventId for {id}, got {other:?}"),
            }
        }
    }

    #[test]
    fn trailing_bytes_are_ignored() {
        let enter = ProtoEvent::Enter {
            pos: Position::Left,
            serial: 9,
        };
        let mut bytes = encode(enter);
        bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let decoded = ProtoEvent::try_from(bytes.as_slice()).expect("decode failed");
        assert_eq!(encode(decoded), encode(enter));
    }
}

#[cfg(test)]
mod extra_tests {
    use super::*;

    fn encode(event: ProtoEvent) -> Vec<u8> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        buf[..len].to_vec()
    }

    /// Small deterministic xorshift PRNG so the randomized test has no
    /// external dependency and reproduces on failure.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }

        fn next_f64(&mut self) -> f64 {
            f64::from_bits(self.next_u64())
        }

        fn position(&mut self) -> Position {
            match self.next_u32() % 4 {
                0 => Position::Left,
                1 => Position::Right,
                2 => Position::Top,
                _ => Position::Bottom,
            }
        }
    }

    fn random_event(rng: &mut Rng) -> ProtoEvent {
        match rng.next_u32() % 12 {
            0 => ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time: rng.next_u32(),
                dx: rng.next_f64(),
                dy: rng.next_f64(),
            })),
            1 => ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
                time: rng.next_u32(),
                button: rng.next_u32(),
                state: rng.next_u32(),
            })),
            2 => ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: rng.next_u32(),
                axis: (rng.next_u32() % 4) as u8,
                value: rng.next_f64(),
            })),
            3 => ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                axis: (rng.next_u32() % 4) as u8,
                value: rng.next_u32() as i32,
            })),
            4 => ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: rng.next_u32(),
                key: rng.next_u32(),
                state: (rng.next_u32() % 2) as u8,
            })),
            5 => ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
                depressed: rng.next_u32(),
                latched: rng.next_u32(),
                locked: rng.next_u32(),
                group: rng.next_u32(),
            })),
            6 => ProtoEvent::Ping,
            7 => ProtoEvent::Pong {
                alive: rng.next_u32() % 2 == 1,
                capabilities: None,
            },
            8 => ProtoEvent::Enter {
                pos: rng.position(),
                serial: rng.next_u32(),
            },
            9 => ProtoEvent::Leave(rng.next_u32()),
            10 => ProtoEvent::Ack(rng.next_u32()),
            _ => {
                let mut commit = [0u8; 8];
                for byte in commit.iter_mut() {
                    *byte = (rng.next_u32() % 256) as u8;
                }
                ProtoEvent::Hello {
                    commit,
                    proto_version: (rng.next_u32() % 256) as u8,
                    capabilities: rng.next_u32(),
                }
            }
        }
    }

    #[test]
    fn event_type_discriminants_are_stable() {
        // These values are on the wire. Changing them breaks every peer.
        assert_eq!(EventType::PointerMotion as u8, 0);
        assert_eq!(EventType::PointerButton as u8, 1);
        assert_eq!(EventType::PointerAxis as u8, 2);
        assert_eq!(EventType::PointerAxisValue120 as u8, 3);
        assert_eq!(EventType::KeyboardKey as u8, 4);
        assert_eq!(EventType::KeyboardModifiers as u8, 5);
        assert_eq!(EventType::Ping as u8, 6);
        assert_eq!(EventType::Pong as u8, 7);
        assert_eq!(EventType::Enter as u8, 8);
        assert_eq!(EventType::Leave as u8, 9);
        assert_eq!(EventType::Ack as u8, 10);
        assert_eq!(EventType::Hello as u8, 11);
        // Clipboard frames reserve 12-15; new events must start at 16.
        assert_eq!(EventType::AuthRequest as u8, 16);
        assert_eq!(EventType::AuthResponse as u8, 17);
        assert_eq!(EventType::AuthRevoke as u8, 18);
    }

    #[test]
    fn randomized_roundtrip_is_stable() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            let event = random_event(&mut rng);
            let bytes = encode(event);
            let decoded = ProtoEvent::try_from(bytes.as_slice())
                .unwrap_or_else(|e| panic!("decode failed for {event}: {e}"));
            assert_eq!(
                encode(decoded),
                bytes,
                "re-encoding changed the bytes for {event}"
            );
        }
    }

    #[test]
    fn boundary_serials_roundtrip() {
        for serial in [0u32, 1, u32::MAX - 1, u32::MAX] {
            let enter = ProtoEvent::Enter {
                pos: Position::Left,
                serial,
            };
            let decoded = ProtoEvent::try_from(encode(enter).as_slice()).expect("decode");
            assert_eq!(encode(decoded), encode(enter));
        }
    }

    #[test]
    fn protocol_version_and_capabilities_are_sane() {
        assert_eq!(PROTO_VERSION, 1);
        let bits = [
            capabilities::BATCHED_INPUT,
            capabilities::TIME_SYNC,
            capabilities::KEYMAP_V1,
            capabilities::ENTER_ACK,
            capabilities::CLIPBOARD,
            capabilities::PERMISSIONS,
        ];
        for bit in bits {
            assert_ne!(bit, 0, "capability bits must be non-zero");
            assert_eq!(bit.count_ones(), 1, "each capability must be one bit");
        }
        for (i, a) in bits.iter().enumerate() {
            for b in &bits[i + 1..] {
                assert_eq!(a & b, 0, "capability bits must be distinct");
            }
        }
        let known = bits.iter().fold(0, |acc, bit| acc | bit);
        assert_eq!(CAPABILITIES & !known, 0);
    }

    #[test]
    fn display_never_panics_and_mentions_the_event() {
        let cases = [
            (ProtoEvent::Ping, "ping"),
            (
                ProtoEvent::Enter {
                    pos: Position::Right,
                    serial: 3,
                },
                "Enter",
            ),
            (ProtoEvent::Leave(1), "Leave"),
            (ProtoEvent::Ack(2), "Ack"),
            (
                ProtoEvent::Pong {
                    alive: true,
                    capabilities: None,
                },
                "pong",
            ),
        ];
        for (event, needle) in cases {
            let rendered = format!("{event}");
            assert!(
                rendered.contains(needle),
                "{rendered:?} does not contain {needle:?}"
            );
        }
    }
}
