//! Wire-format guarantees the network relies on: every event and clipboard
//! frame survives an encode/decode round trip, and no byte sequence a peer
//! can send - truncated, unknown, random - panics the decoder.

use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};

use crate::{
    AppVersion, MAX_EVENT_SIZE, Position, ProtoEvent,
    clipboard::{
        CHUNK_SIZE, ClipboardFrame, FRAME_CANCEL, FRAME_OFFER, MIME_FILES, MIME_HTML, MIME_PNG,
        MIME_TEXT, is_clipboard_frame,
    },
};

fn every_event() -> Vec<ProtoEvent> {
    let mut events = vec![
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
            time: u32::MAX,
            dx: -12.5,
            dy: 1e-9,
        })),
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Button {
            time: 7,
            button: 0x110,
            state: 1,
        })),
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Axis {
            time: 0,
            axis: 1,
            value: -3.25,
        })),
        ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: i32::MIN,
        })),
        ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Key {
            time: 1,
            key: 30,
            state: 2,
        })),
        ProtoEvent::Input(InputEvent::Keyboard(KeyboardEvent::Modifiers {
            depressed: 1,
            latched: 2,
            locked: 4,
            group: 8,
        })),
        ProtoEvent::Ping,
        ProtoEvent::Pong {
            alive: true,
            capabilities: Some(u32::MAX),
        },
        ProtoEvent::Pong {
            alive: false,
            capabilities: None,
        },
        ProtoEvent::Leave(u32::MAX),
        ProtoEvent::LeaveAt {
            serial: 3,
            along: 0.75,
        },
        ProtoEvent::Ack(0),
        ProtoEvent::Hello {
            commit: *b"deadbeef",
            proto_version: 255,
            capabilities: 0x1234_5678,
            app_version: Some(AppVersion {
                major: 0,
                minor: 2,
                patch: 21,
            }),
        },
        ProtoEvent::Hello {
            commit: [0; 8],
            proto_version: 0,
            capabilities: 0,
            app_version: None,
        },
        ProtoEvent::AuthRequest {
            session_id: u128::MAX,
            incoming_caps: 0xff,
            outgoing_caps: 0,
        },
        ProtoEvent::AuthResponse {
            session_id: 1,
            incoming_caps: 3,
            outgoing_caps: 7,
        },
        ProtoEvent::AuthRevoke {
            session_id: 42,
            reason: 1,
        },
        ProtoEvent::LatencyProbe(9),
        ProtoEvent::LatencyReply(u32::MAX),
        ProtoEvent::LockState { known: 7, on: 5 },
    ];
    for pos in [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ] {
        events.push(ProtoEvent::Enter { pos, serial: 11 });
        events.push(ProtoEvent::EnterAt {
            pos,
            serial: 12,
            along: 0.5,
        });
    }
    events
}

fn every_frame() -> Vec<ClipboardFrame> {
    vec![
        ClipboardFrame::Offer {
            serial: 1,
            origin: u64::MAX,
            hash: [9; 32],
            items: vec![(MIME_TEXT, 10), (MIME_HTML, 0), (MIME_PNG, u32::MAX)],
        },
        ClipboardFrame::Offer {
            serial: 2,
            origin: 0,
            hash: [0; 32],
            items: vec![(MIME_FILES, 1)],
        },
        ClipboardFrame::Chunk {
            serial: 3,
            mime: MIME_TEXT,
            offset: 0,
            total: CHUNK_SIZE as u32,
            data: vec![0xab; CHUNK_SIZE],
        },
        ClipboardFrame::Chunk {
            serial: 4,
            mime: MIME_PNG,
            offset: CHUNK_SIZE as u32,
            total: CHUNK_SIZE as u32 + 1,
            data: vec![1],
        },
        ClipboardFrame::Ack {
            serial: 5,
            mime: MIME_HTML,
            base: 7,
            bitmap: 0x8000_0001,
        },
        ClipboardFrame::Cancel { serial: u32::MAX },
    ]
}

fn encode(event: ProtoEvent) -> Vec<u8> {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
    buf[..len].to_vec()
}

/// A tiny deterministic generator, so a failure always reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// Every event survives encode -> decode -> encode bit for bit.
#[test]
fn every_event_round_trips_exactly() {
    for event in every_event() {
        let name = event.to_string();
        let bytes = encode(event);
        assert!(
            bytes.len() <= MAX_EVENT_SIZE,
            "{name} exceeds MAX_EVENT_SIZE"
        );
        let decoded = ProtoEvent::try_from(bytes.as_slice())
            .unwrap_or_else(|e| panic!("{name} does not decode: {e}"));
        assert_eq!(encode(decoded), bytes, "{name} changed in a round trip");
    }
}

/// No event can be mistaken for a clipboard frame (the two share the
/// first byte as their type).
#[test]
fn no_event_is_taken_for_a_clipboard_frame() {
    for event in every_event() {
        let name = event.to_string();
        let bytes = encode(event);
        assert!(
            !is_clipboard_frame(bytes[0]),
            "{name} looks like a clipboard frame"
        );
    }
    for frame in every_frame() {
        assert!(is_clipboard_frame(frame.encode()[0]));
    }
}

/// A cut-off event (a lost tail, a hostile peer) is an error or, for the
/// fields older peers never sent, a shorter valid event - never a panic,
/// never a read past the end.
#[test]
fn every_prefix_of_every_event_decodes_or_fails_cleanly() {
    for event in every_event() {
        let bytes = encode(event);
        for cut in 0..bytes.len() {
            let _ = ProtoEvent::try_from(&bytes[..cut]);
        }
    }
    assert!(
        ProtoEvent::try_from(&[][..]).is_err(),
        "an empty datagram decoded"
    );
}

/// Bytes after a complete event (a newer peer appending fields) do not
/// change what an older build reads.
#[test]
fn trailing_bytes_do_not_change_the_event() {
    for event in every_event() {
        let name = event.to_string();
        let bytes = encode(event);
        let expected = encode(ProtoEvent::try_from(bytes.as_slice()).unwrap());
        let mut longer = bytes.clone();
        longer.extend_from_slice(&[0xee; 16]);
        if let Ok(decoded) = ProtoEvent::try_from(longer.as_slice()) {
            let reencoded = encode(decoded);
            // optional trailing fields may pick the extra bytes up; the
            // required part must stay what was sent
            assert_eq!(
                &reencoded[..expected.len().min(reencoded.len())],
                &expected[..expected.len().min(reencoded.len())],
                "{name} changed with trailing bytes"
            );
        }
    }
}

/// Every possible type byte, followed by enough payload for any event:
/// known types decode, unknown ones are errors.
#[test]
fn every_type_byte_is_handled() {
    let mut rng = Lcg(7);
    for first in 0..=u8::MAX {
        let mut bytes = vec![first];
        bytes.extend(rng.bytes(64));
        let _ = ProtoEvent::try_from(bytes.as_slice());
        if is_clipboard_frame(first) {
            let _ = ClipboardFrame::decode(&bytes);
        }
    }
    for unknown in [24u8, 100, 200, 255] {
        assert!(ProtoEvent::try_from(&[unknown, 0, 0, 0, 0][..]).is_err());
    }
}

/// Random datagrams of every size up to a full frame never panic either
/// decoder.
#[test]
fn random_datagrams_never_panic() {
    let mut rng = Lcg(0x5eed);
    for round in 0..20_000 {
        let len = (rng.next() as usize) % (CHUNK_SIZE + 64);
        let mut bytes = rng.bytes(len);
        if !bytes.is_empty() && round % 2 == 0 {
            // steer half of them into the clipboard decoder
            bytes[0] = FRAME_OFFER + (rng.next() as u8 % (FRAME_CANCEL - FRAME_OFFER + 1));
        }
        let _ = ProtoEvent::try_from(bytes.as_slice());
        let _ = ClipboardFrame::decode(&bytes);
    }
}

/// Every clipboard frame round trips, and every cut-off one is an error.
#[test]
fn clipboard_frames_round_trip_and_reject_every_prefix() {
    for frame in every_frame() {
        let bytes = frame.encode();
        assert_eq!(bytes.len(), frame.encoded_len(), "{frame}");
        assert_eq!(ClipboardFrame::decode(&bytes).as_ref(), Ok(&frame));
        for cut in 0..bytes.len() {
            assert!(
                ClipboardFrame::decode(&bytes[..cut]).is_err(),
                "{frame} cut at {cut} decoded"
            );
        }
    }
}

/// A chunk whose length field claims more bytes than the datagram holds is
/// truncated, not read past its end; one claiming more than a chunk may
/// carry is refused.
#[test]
fn a_chunk_cannot_claim_more_data_than_it_carries() {
    let frame = ClipboardFrame::Chunk {
        serial: 1,
        mime: MIME_TEXT,
        offset: 0,
        total: 10,
        data: vec![1, 2, 3],
    };
    let mut bytes = frame.encode();
    // the u16 length sits right before the data
    let at = bytes.len() - 3 - 2;
    bytes[at..at + 2].copy_from_slice(&200u16.to_be_bytes());
    assert!(ClipboardFrame::decode(&bytes).is_err());
    bytes[at..at + 2].copy_from_slice(&(CHUNK_SIZE as u16 + 1).to_be_bytes());
    bytes.extend(vec![0; CHUNK_SIZE]);
    assert!(ClipboardFrame::decode(&bytes).is_err());
}

/// An offer with more representations than exist, or an unknown one, is
/// refused instead of allocating for it.
#[test]
fn an_offer_with_too_many_or_unknown_items_is_refused() {
    let mut too_many = ClipboardFrame::Offer {
        serial: 1,
        origin: 1,
        hash: [0; 32],
        items: vec![(MIME_TEXT, 1); 4],
    }
    .encode();
    assert!(ClipboardFrame::decode(&too_many).is_err());
    too_many[45] = 255; // item count
    assert!(ClipboardFrame::decode(&too_many).is_err());

    let unknown = ClipboardFrame::Offer {
        serial: 1,
        origin: 1,
        hash: [0; 32],
        items: vec![(99, 1)],
    }
    .encode();
    assert!(ClipboardFrame::decode(&unknown).is_err());
}
