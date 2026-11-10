//! Fuzz the wire decoder: arbitrary datagrams must never panic, and any event
//! that decodes must re-encode without overrunning `MAX_EVENT_SIZE`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};

fuzz_target!(|data: &[u8]| {
    if let Ok(event) = ProtoEvent::try_from(data) {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        assert!(len <= MAX_EVENT_SIZE);
        let _ = &buf[..len];
    }
});
