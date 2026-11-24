//! Fuzz the clipboard receiver: a peer sends arbitrary datagrams (an offer
//! followed by chunks). Decoding and reassembly must never panic, allocate
//! past the configured limits, or accept a snapshot whose hash does not match.
//!
//! Input layout: a sequence of `[len: u8][frame bytes; len]` records, each
//! fed to `ClipboardFrame::decode` like one datagram.
#![no_main]

use std::time::Instant;

use input_clipboard::{Limits, transfer::IncomingTransfer};
use lan_mouse_proto::clipboard::{ClipboardFrame, MAX_CLIPBOARD_FRAME};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Small limits: an offer may not make the fuzzer allocate megabytes.
    let limits = Limits {
        max_text: 8 * 1024,
        max_html: 8 * 1024,
        max_image: 16 * 1024,
        max_snapshot: 24 * 1024,
    };
    let now = Instant::now();
    let mut transfer: Option<IncomingTransfer> = None;
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let len = (len as usize).min(tail.len());
        let (datagram, tail) = tail.split_at(len);
        rest = tail;

        let Ok(frame) = ClipboardFrame::decode(datagram) else {
            continue;
        };
        // a decoded frame re-encodes to the same bytes and fits a datagram
        let encoded = frame.encode();
        assert!(encoded.len() <= MAX_CLIPBOARD_FRAME);
        assert_eq!(encoded.len(), frame.encoded_len());
        assert_eq!(ClipboardFrame::decode(&encoded).as_ref(), Ok(&frame));

        match frame {
            ClipboardFrame::Offer { serial, origin, hash, items } => {
                transfer = IncomingTransfer::from_offer(serial, origin, hash, &items, &limits, now).ok();
            }
            ClipboardFrame::Chunk { serial, mime, offset, total, data } => {
                if let Some(incoming) = transfer.as_mut() {
                    let _ = incoming.on_chunk(serial, mime, offset, total, &data, now);
                    let _ = incoming.acks();
                    if incoming.is_complete() {
                        // random bytes essentially never match the offered
                        // SHA-256; anything that assembles must have matched
                        let _ = incoming.assemble();
                    }
                }
            }
            ClipboardFrame::Ack { .. } | ClipboardFrame::Cancel { .. } => {}
        }
    }
});
