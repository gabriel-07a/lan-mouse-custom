//! Line-ending normalization helpers.
//!
//! Windows applications expect `\r\n` in clipboard text, while the canonical
//! representation uses `\n`. Converting on the boundary keeps the same text
//! on both platforms without corrupting embedded newlines.

/// Convert Windows `\r\n` to the canonical `\n`.
pub fn crlf_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Convert canonical `\n` to Windows `\r\n`, without doubling existing CRLF.
pub fn lf_to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                out.push('\r');
                if chars.peek() == Some(&'\n') {
                    out.push('\n');
                    chars.next();
                }
            }
            '\n' => out.push_str("\r\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_to_lf_converts_only_windows_newlines() {
        assert_eq!(crlf_to_lf("a\r\nb"), "a\nb");
        assert_eq!(crlf_to_lf("a\nb"), "a\nb");
        assert_eq!(crlf_to_lf("a\rb"), "a\rb", "lone CR is preserved");
    }

    #[test]
    fn lf_to_crlf_does_not_double_existing_crlf() {
        assert_eq!(lf_to_crlf("a\nb"), "a\r\nb");
        assert_eq!(lf_to_crlf("a\r\nb"), "a\r\nb");
        assert_eq!(lf_to_crlf("a\rb"), "a\rb");
    }

    #[test]
    fn conversions_roundtrip_for_canonical_text() {
        let text = "linha 1\nlinha 2\nlinha 3\n";
        assert_eq!(crlf_to_lf(&lf_to_crlf(text)), text);
    }

    #[test]
    fn mixed_input_is_canonicalized() {
        assert_eq!(crlf_to_lf("a\r\nb\nc\r\n"), "a\nb\nc\n");
    }
}
