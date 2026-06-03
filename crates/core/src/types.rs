#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

/// libxml2-compatible character unit. XML is processed as UTF-8; a single
/// `XmlChar` is one byte of that stream, not one Unicode scalar.
pub type XmlChar = u8;

/// Interpret a `&[XmlChar]` slice as a UTF-8 `&str`.
///
/// Returns `None` if the bytes are not valid UTF-8.
pub fn xml_str(bytes: &[XmlChar]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

/// Convert a Rust `&str` to an owned `Vec<XmlChar>` (null-terminated for C
/// interop when needed).
pub fn to_xml_chars(s: &str) -> Vec<XmlChar> {
    s.bytes().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_ascii() {
        let s = "hello";
        let bytes = to_xml_chars(s);
        assert_eq!(xml_str(&bytes), Some(s));
    }

    #[test]
    fn round_trip_unicode() {
        let s = "héllo wörld";
        let bytes = to_xml_chars(s);
        assert_eq!(xml_str(&bytes), Some(s));
    }
}
