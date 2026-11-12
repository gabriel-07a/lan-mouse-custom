//! Clipboard framing.
//!
//! Clipboard payloads do not fit in [`ProtoEvent`] (which is a small, `Copy`
//! enum) and DTLS does not retransmit application data. This module defines a
//! separate frame format that shares the DTLS association but has its own
//! first-byte range, chunking and acknowledgement.
//!
//! Frame types start at 12, above every [`crate::EventType`] discriminant, so
//! the receive loops can tell clipboard frames and input events apart before
//! decoding either of them.

use std::fmt::Display;

use thiserror::Error;

/// Clipboard format identifiers carried on the wire.
pub const MIME_TEXT: u8 = 1;
pub const MIME_HTML: u8 = 2;
pub const MIME_PNG: u8 = 3;

/// Payload bytes per chunk. Kept well below the path MTU so the DTLS record,
/// UDP and IP headers still fit on Wi-Fi and VPN links.
pub const CHUNK_SIZE: usize = 1024;

/// Largest possible clipboard frame: chunk header + payload.
pub const MAX_CLIPBOARD_FRAME: usize = 16 + CHUNK_SIZE;

/// Number of chunks covered by one acknowledgement bitmap.
pub const ACK_WINDOW: u32 = 32;

/// Frame type discriminants. Deliberately outside [`crate::EventType`].
pub const FRAME_OFFER: u8 = 12;
pub const FRAME_CHUNK: u8 = 13;
pub const FRAME_ACK: u8 = 14;
pub const FRAME_CANCEL: u8 = 15;

/// Whether `first` starts a clipboard frame.
pub const fn is_clipboard_frame(first: u8) -> bool {
    first >= FRAME_OFFER && first <= FRAME_CANCEL
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardFrame {
    /// Announces a clipboard snapshot. `items` lists `(mime_id, size)` for the
    /// representations the sender has.
    Offer {
        serial: u32,
        origin: u64,
        hash: [u8; 32],
        items: Vec<(u8, u32)>,
    },
    /// A slice of one representation.
    Chunk {
        serial: u32,
        mime: u8,
        offset: u32,
        total: u32,
        data: Vec<u8>,
    },
    /// Cumulative acknowledgement for one representation: `bitmap` bit `i`
    /// means chunk `base + i` of `mime` was received.
    Ack {
        serial: u32,
        mime: u8,
        base: u32,
        bitmap: u32,
    },
    /// Aborts an in-flight transfer.
    Cancel { serial: u32 },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("truncated clipboard frame")]
    Truncated,
    #[error("unknown clipboard frame type: {0}")]
    UnknownType(u8),
    #[error("clipboard chunk is too large: {0} bytes")]
    ChunkTooLarge(usize),
    #[error("invalid clipboard offer: item count {0}")]
    InvalidItemCount(usize),
    #[error("invalid clipboard mime id: {0}")]
    InvalidMime(u8),
}

impl ClipboardFrame {
    pub fn type_id(&self) -> u8 {
        match self {
            Self::Offer { .. } => FRAME_OFFER,
            Self::Chunk { .. } => FRAME_CHUNK,
            Self::Ack { .. } => FRAME_ACK,
            Self::Cancel { .. } => FRAME_CANCEL,
        }
    }

    pub fn serial(&self) -> u32 {
        match self {
            Self::Offer { serial, .. }
            | Self::Chunk { serial, .. }
            | Self::Ack { serial, .. }
            | Self::Cancel { serial } => *serial,
        }
    }

    /// Whether this frame carries clipboard content (and is therefore governed
    /// by the peer's clipboard permissions).
    ///
    /// `Ack`/`Cancel` are protocol control for a transfer the peer already
    /// sent: they carry no clipboard data, so a read/write policy gate must
    /// never drop them (that would make the opposite-direction transfer time
    /// out even though the content itself was allowed).
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Offer { .. } | Self::Chunk { .. })
    }

    /// Encode into a freshly allocated buffer of exactly the right size.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        match self {
            Self::Offer {
                serial,
                origin,
                hash,
                items,
            } => {
                out.push(FRAME_OFFER);
                out.extend_from_slice(&serial.to_be_bytes());
                out.extend_from_slice(&origin.to_be_bytes());
                out.extend_from_slice(hash);
                out.push(items.len() as u8);
                for (mime, size) in items {
                    out.push(*mime);
                    out.extend_from_slice(&size.to_be_bytes());
                }
            }
            Self::Chunk {
                serial,
                mime,
                offset,
                total,
                data,
            } => {
                out.push(FRAME_CHUNK);
                out.extend_from_slice(&serial.to_be_bytes());
                out.push(*mime);
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(&total.to_be_bytes());
                out.extend_from_slice(&(data.len() as u16).to_be_bytes());
                out.extend_from_slice(data);
            }
            Self::Ack {
                serial,
                mime,
                base,
                bitmap,
            } => {
                out.push(FRAME_ACK);
                out.extend_from_slice(&serial.to_be_bytes());
                out.push(*mime);
                out.extend_from_slice(&base.to_be_bytes());
                out.extend_from_slice(&bitmap.to_be_bytes());
            }
            Self::Cancel { serial } => {
                out.push(FRAME_CANCEL);
                out.extend_from_slice(&serial.to_be_bytes());
            }
        }
        out
    }

    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Offer { items, .. } => 1 + 4 + 8 + 32 + 1 + items.len() * 5,
            Self::Chunk { data, .. } => 16 + data.len(),
            Self::Ack { .. } => 14,
            Self::Cancel { .. } => 5,
        }
    }

    pub fn decode(buf: &[u8]) -> Result<Self, FrameError> {
        let mut reader = Reader { buf };
        let kind = reader.u8()?;
        let frame = match kind {
            FRAME_OFFER => {
                let serial = reader.u32()?;
                let origin = reader.u64()?;
                let mut hash = [0u8; 32];
                hash.copy_from_slice(reader.bytes(32)?);
                let count = reader.u8()? as usize;
                if count > 3 {
                    return Err(FrameError::InvalidItemCount(count));
                }
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    let mime = reader.u8()?;
                    if !matches!(mime, MIME_TEXT | MIME_HTML | MIME_PNG) {
                        return Err(FrameError::InvalidMime(mime));
                    }
                    let size = reader.u32()?;
                    items.push((mime, size));
                }
                Self::Offer {
                    serial,
                    origin,
                    hash,
                    items,
                }
            }
            FRAME_CHUNK => {
                let serial = reader.u32()?;
                let mime = reader.u8()?;
                if !matches!(mime, MIME_TEXT | MIME_HTML | MIME_PNG) {
                    return Err(FrameError::InvalidMime(mime));
                }
                let offset = reader.u32()?;
                let total = reader.u32()?;
                let len = reader.u16()? as usize;
                if len > CHUNK_SIZE {
                    return Err(FrameError::ChunkTooLarge(len));
                }
                let data = reader.bytes(len)?.to_vec();
                Self::Chunk {
                    serial,
                    mime,
                    offset,
                    total,
                    data,
                }
            }
            FRAME_ACK => {
                let serial = reader.u32()?;
                let mime = reader.u8()?;
                if !matches!(mime, MIME_TEXT | MIME_HTML | MIME_PNG) {
                    return Err(FrameError::InvalidMime(mime));
                }
                Self::Ack {
                    serial,
                    mime,
                    base: reader.u32()?,
                    bitmap: reader.u32()?,
                }
            }
            FRAME_CANCEL => Self::Cancel {
                serial: reader.u32()?,
            },
            other => return Err(FrameError::UnknownType(other)),
        };
        Ok(frame)
    }
}

impl Display for ClipboardFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offer {
                serial,
                items,
                hash,
                ..
            } => write!(
                f,
                "ClipboardOffer(serial={serial}, items={}, hash={:02x}{:02x}..)",
                items.len(),
                hash[0],
                hash[1]
            ),
            Self::Chunk {
                serial,
                mime,
                offset,
                total,
                data,
            } => write!(
                f,
                "ClipboardChunk(serial={serial}, mime={mime}, {}/{total}, {} bytes)",
                offset,
                data.len()
            ),
            Self::Ack {
                serial,
                mime,
                base,
                bitmap,
            } => write!(
                f,
                "ClipboardAck(serial={serial}, mime={mime}, base={base}, bitmap={bitmap:#010x})"
            ),
            Self::Cancel { serial } => write!(f, "ClipboardCancel(serial={serial})"),
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], FrameError> {
        let (head, rest) = self.buf.split_at_checked(n).ok_or(FrameError::Truncated)?;
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, FrameError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, FrameError> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().expect("len")))
    }

    fn u32(&mut self) -> Result<u32, FrameError> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().expect("len")))
    }

    fn u64(&mut self) -> Result<u64, FrameError> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().expect("len")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> ClipboardFrame {
        ClipboardFrame::Offer {
            serial: 7,
            origin: 0x1122_3344_5566_7788,
            hash: [0xab; 32],
            items: vec![(MIME_TEXT, 12), (MIME_HTML, 34), (MIME_PNG, 56)],
        }
    }

    fn frames() -> Vec<ClipboardFrame> {
        vec![
            offer(),
            ClipboardFrame::Chunk {
                serial: 7,
                mime: MIME_PNG,
                offset: 2 * CHUNK_SIZE as u32,
                total: 4096,
                data: vec![0x5a; CHUNK_SIZE],
            },
            ClipboardFrame::Ack {
                serial: 7,
                mime: MIME_TEXT,
                base: 32,
                bitmap: 0xdead_beef,
            },
            ClipboardFrame::Cancel { serial: 7 },
        ]
    }

    #[test]
    fn roundtrip_all_frames() {
        for frame in frames() {
            let encoded = frame.encode();
            assert_eq!(encoded.len(), frame.encoded_len());
            assert_eq!(ClipboardFrame::decode(&encoded).expect("decode"), frame);
        }
    }

    #[test]
    fn first_byte_identifies_clipboard_frames() {
        for frame in frames() {
            let encoded = frame.encode();
            assert!(is_clipboard_frame(encoded[0]));
            assert_eq!(encoded[0], frame.type_id());
        }
        assert!(!is_clipboard_frame(0));
        assert!(!is_clipboard_frame(11));
        assert!(!is_clipboard_frame(16));
    }

    #[test]
    fn every_truncation_is_an_error_not_a_panic() {
        for frame in frames() {
            let encoded = frame.encode();
            for n in 0..encoded.len() {
                let _ = ClipboardFrame::decode(&encoded[..n]);
                assert!(
                    ClipboardFrame::decode(&encoded[..n]).is_err(),
                    "prefix {n}/{} of {frame} should be truncated",
                    encoded.len()
                );
            }
        }
    }

    #[test]
    fn unknown_type_is_rejected() {
        assert_eq!(
            ClipboardFrame::decode(&[99]),
            Err(FrameError::UnknownType(99))
        );
    }

    #[test]
    fn oversized_chunk_is_rejected() {
        let mut encoded = ClipboardFrame::Chunk {
            serial: 1,
            mime: MIME_TEXT,
            offset: 0,
            total: 1,
            data: vec![0; CHUNK_SIZE],
        }
        .encode();
        // claim one more byte than the payload carries / than allowed
        let len_pos = encoded.len() - CHUNK_SIZE - 2;
        encoded[len_pos..len_pos + 2].copy_from_slice(&((CHUNK_SIZE + 1) as u16).to_be_bytes());
        encoded.push(0);
        assert_eq!(
            ClipboardFrame::decode(&encoded),
            Err(FrameError::ChunkTooLarge(CHUNK_SIZE + 1))
        );
    }

    #[test]
    fn invalid_mime_is_rejected() {
        let encoded = vec![FRAME_CHUNK, 0, 0, 0, 1, 200, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0];
        assert_eq!(
            ClipboardFrame::decode(&encoded),
            Err(FrameError::InvalidMime(200))
        );
    }

    #[test]
    fn serial_and_type_accessors() {
        for frame in frames() {
            assert_eq!(frame.serial(), 7);
        }
        assert_eq!(offer().type_id(), FRAME_OFFER);
    }

    #[test]
    fn only_data_frames_are_content() {
        assert!(offer().is_content());
        assert!(
            ClipboardFrame::Chunk {
                serial: 1,
                mime: MIME_TEXT,
                offset: 0,
                total: 1,
                data: vec![0],
            }
            .is_content()
        );
        // Control replies for a transfer the peer already started are not
        // governed by a clipboard-content permission gate.
        assert!(
            !ClipboardFrame::Ack {
                serial: 1,
                mime: MIME_TEXT,
                base: 0,
                bitmap: 0,
            }
            .is_content()
        );
        assert!(!ClipboardFrame::Cancel { serial: 1 }.is_content());
    }

    #[test]
    fn arbitrary_frames_never_panic() {
        // Fuzz-style regression over the clipboard decoder: random bytes
        // (any length up to a little above a full frame) must produce an
        // error or a value, never a panic.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        };
        for len in 0..=(MAX_CLIPBOARD_FRAME + 8) {
            for _ in 0..32 {
                let mut bytes = vec![0u8; len];
                for byte in bytes.iter_mut() {
                    *byte = next();
                }
                if let Ok(frame) = ClipboardFrame::decode(&bytes) {
                    // Decoded frames must re-encode consistently.
                    let encoded = frame.encode();
                    assert_eq!(encoded.len(), frame.encoded_len());
                    assert_eq!(ClipboardFrame::decode(&encoded), Ok(frame));
                }
            }
        }
    }

    #[test]
    fn maximum_chunk_and_empty_offer_roundtrip() {
        let full = ClipboardFrame::Chunk {
            serial: u32::MAX,
            mime: MIME_TEXT,
            offset: u32::MAX - CHUNK_SIZE as u32,
            total: u32::MAX,
            data: vec![0xff; CHUNK_SIZE],
        };
        let encoded = full.encode();
        assert!(encoded.len() <= MAX_CLIPBOARD_FRAME);
        assert_eq!(ClipboardFrame::decode(&encoded), Ok(full));

        let empty = ClipboardFrame::Offer {
            serial: 1,
            origin: 0,
            hash: [0u8; 32],
            items: Vec::new(),
        };
        let encoded = empty.encode();
        assert_eq!(ClipboardFrame::decode(&encoded), Ok(empty));
    }

    #[test]
    fn over_maximum_item_count_is_rejected() {
        // Manual offer with a fourth item: the decoder must reject it before
        // allocating (the wire count is a single byte).
        let mut encoded = vec![FRAME_OFFER];
        encoded.extend_from_slice(&1u32.to_be_bytes()); // serial
        encoded.extend_from_slice(&0u64.to_be_bytes()); // origin
        encoded.extend_from_slice(&[0u8; 32]); // hash
        encoded.push(4); // item count above the accepted maximum
        for _ in 0..4 {
            encoded.push(MIME_TEXT);
            encoded.extend_from_slice(&1u32.to_be_bytes());
        }
        assert_eq!(
            ClipboardFrame::decode(&encoded),
            Err(FrameError::InvalidItemCount(4))
        );
    }
}
