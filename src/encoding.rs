//! Minimal percent-encoding for URL path segments and query values.

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub(crate) fn encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn keeps_unreserved() {
        assert_eq!(encode("aZ09-._~"), "aZ09-._~");
    }

    #[test]
    fn escapes_reserved_and_utf8() {
        assert_eq!(encode("a/b c"), "a%2Fb%20c");
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode("https://x?y=1"), "https%3A%2F%2Fx%3Fy%3D1");
    }
}
