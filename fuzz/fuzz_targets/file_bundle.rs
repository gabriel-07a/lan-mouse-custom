//! Fuzz the copied-files bundle decoder: a peer sends any bytes as a file
//! bundle. Decoding must never panic, and whatever it accepts must only name
//! safe relative paths (no `..`, nothing absolute) and re-encode to the same
//! bytes.
#![no_main]

use input_clipboard::files::{decode, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(entries) = decode(data) {
        for entry in &entries {
            assert!(!entry.path.starts_with('/'));
            assert!(entry.path.split('/').all(|part| part != ".." && part != "." && !part.is_empty()));
            assert!(!entry.path.contains('\\') && !entry.path.contains(':'));
        }
        assert_eq!(encode(&entries), data);
    }
});
