//! Windows `CF_HTML` conversion.
//!
//! Windows stores HTML on the clipboard as a byte blob with a textual header:
//!
//! ```text
//! Version:0.9\r\n
//! StartHTML:0000000105\r\n
//! EndHTML:0000000200\r\n
//! StartFragment:0000000145\r\n
//! EndFragment:0000000180\r\n
//! <html><body><!--StartFragment-->...</body></html>
//! ```
//!
//! The offsets are **byte** offsets from the start of the blob, including the
//! header. Rebuilding the blob therefore requires recomputing them; that is
//! the whole point of this module. It is pure Rust so it can be unit-tested on
//! every platform, not only on Windows.

const HEADER_VERSION: &str = "Version:0.9\r\n";
const START_HTML: &str = "StartHTML:";
const END_HTML: &str = "EndHTML:";
const START_FRAGMENT: &str = "StartFragment:";
const END_FRAGMENT: &str = "EndFragment:";
const HTML_PREFIX: &str = "<html><body><!--StartFragment-->";
const HTML_SUFFIX: &str = "<!--EndFragment--></body></html>";

/// Extract the HTML fragment from a `CF_HTML` blob, falling back to the raw
/// bytes when the header is missing or malformed.
pub fn parse(blob: &[u8]) -> Vec<u8> {
    let text = match std::str::from_utf8(&blob[..blob.len().min(1024)]) {
        Ok(text) => text,
        Err(_) => return blob.to_vec(),
    };
    let fragment = |key: &str| -> Option<usize> {
        text.find(key)
            .and_then(|pos| text[pos + key.len()..].split("\r\n").next())
            .and_then(|value| value.trim().parse::<usize>().ok())
    };
    let (Some(start), Some(end)) = (fragment(START_FRAGMENT), fragment(END_FRAGMENT)) else {
        return blob.to_vec();
    };
    if start <= end && end <= blob.len() {
        blob[start..end].to_vec()
    } else {
        blob.to_vec()
    }
}

/// Build a `CF_HTML` blob around a raw HTML fragment.
pub fn build(fragment: &[u8]) -> Vec<u8> {
    // Fixed-width offsets keep the header length constant, so the values can
    // be computed without a fixed-point iteration.
    let number = |value: usize| format!("{value:010}\r\n");
    let header_len = HEADER_VERSION.len()
        + START_HTML.len()
        + 10
        + 2
        + END_HTML.len()
        + 10
        + 2
        + START_FRAGMENT.len()
        + 10
        + 2
        + END_FRAGMENT.len()
        + 10
        + 2;
    let start_html = header_len;
    let start_fragment = start_html + HTML_PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + HTML_SUFFIX.len();

    let mut out = Vec::with_capacity(end_html);
    out.extend_from_slice(HEADER_VERSION.as_bytes());
    out.extend_from_slice(START_HTML.as_bytes());
    out.extend_from_slice(number(start_html).as_bytes());
    out.extend_from_slice(END_HTML.as_bytes());
    out.extend_from_slice(number(end_html).as_bytes());
    out.extend_from_slice(START_FRAGMENT.as_bytes());
    out.extend_from_slice(number(start_fragment).as_bytes());
    out.extend_from_slice(END_FRAGMENT.as_bytes());
    out.extend_from_slice(number(end_fragment).as_bytes());
    out.extend_from_slice(HTML_PREFIX.as_bytes());
    out.extend_from_slice(fragment);
    out.extend_from_slice(HTML_SUFFIX.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_then_parse_roundtrips() {
        let fragment = b"<p>Ol\xc3\xa1, <b>mundo</b>!</p>";
        let blob = build(fragment);
        assert_eq!(parse(&blob), fragment);
    }

    #[test]
    fn offsets_are_valid_byte_offsets() {
        let fragment = "ação".repeat(10);
        let blob = build(fragment.as_bytes());
        let header = std::str::from_utf8(&blob).expect("utf8 header");
        let value = |key: &str| {
            header[header.find(key).unwrap() + key.len()..]
                .split("\r\n")
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        };
        assert_eq!(value(START_HTML), 105, "standard header length");
        assert_eq!(value(END_HTML), blob.len());
        assert!(value(START_FRAGMENT) > value(START_HTML));
        assert!(value(END_FRAGMENT) > value(START_FRAGMENT));
        assert_eq!(parse(&blob), fragment.as_bytes());
    }

    #[test]
    fn parse_falls_back_to_raw_bytes_without_header() {
        let raw = b"<b>not a cf_html blob</b>";
        assert_eq!(parse(raw), raw);
    }

    #[test]
    fn parse_rejects_out_of_bounds_offsets() {
        let mut blob = build(b"<b>x</b>");
        let header = std::str::from_utf8(&blob).expect("utf8");
        if let Some(pos) = header.find(END_FRAGMENT) {
            let value_start = pos + END_FRAGMENT.len();
            let value_end = value_start + 10;
            blob[value_start..value_end].copy_from_slice(b"9999999999");
        }
        // Must not panic and must not slice out of bounds.
        let _ = parse(&blob);
    }

    #[test]
    fn empty_fragment_roundtrips() {
        assert_eq!(parse(&build(b"")), b"");
    }
}
