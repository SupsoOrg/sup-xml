#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

/// The charset the serialized bytes will ultimately be encoded into.
/// Characters the charset cannot represent are emitted as numeric
/// character references (`&#N;`) in text / attribute content, matching
/// libxml2's serializer: a non-UTF-8 output encoding escapes anything
/// outside its repertoire rather than dropping or mis-encoding it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum OutputCharset {
    /// Every Unicode scalar value is representable (UTF-8/16/32 output).
    #[default]
    Utf8,
    /// ISO-8859-1: code points `<= 0xFF` pass through; the rest escape.
    Latin1,
    /// US-ASCII: code points `<= 0x7F` pass through; the rest escape.
    /// Also the effective charset when no output encoding is requested
    /// (libxml2's default serialization escapes non-ASCII).
    Ascii,
}

impl OutputCharset {
    #[inline]
    fn represents(self, c: char) -> bool {
        match self {
            OutputCharset::Utf8   => true,
            OutputCharset::Latin1 => (c as u32) <= 0xFF,
            OutputCharset::Ascii  => (c as u32) <= 0x7F,
        }
    }
}

/// 64-bit clean output buffer. Replaces the libxml2 `xmlBuf` / `xmlBuffer`
/// duality — only one type here, always size_t-indexed.
pub struct XmlBuf {
    inner: Vec<u8>,
    charset: OutputCharset,
}

impl XmlBuf {
    pub fn new() -> Self {
        Self { inner: Vec::new(), charset: OutputCharset::Utf8 }
    }

    pub fn with_capacity(n: usize) -> Self {
        Self { inner: Vec::with_capacity(n), charset: OutputCharset::Utf8 }
    }

    /// Buffer that escapes characters outside `charset` as numeric
    /// character references in text / attribute content.
    pub fn with_charset(n: usize, charset: OutputCharset) -> Self {
        Self { inner: Vec::with_capacity(n), charset }
    }

    /// Emit `c` verbatim (UTF-8) when the target charset can represent
    /// it, otherwise as a numeric character reference.
    #[inline]
    fn push_char_for_charset(&mut self, c: char) {
        if self.charset.represents(c) {
            let mut b = [0u8; 4];
            self.inner.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        } else {
            // Decimal numeric character reference, e.g. `&#248;`.
            self.push_str("&#");
            let mut digits = [0u8; 10];
            let mut i = digits.len();
            let mut n = c as u32;
            loop {
                i -= 1;
                digits[i] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 { break; }
            }
            self.inner.extend_from_slice(&digits[i..]);
            self.push_byte(b';');
        }
    }

    pub fn push_str(&mut self, s: &str) {
        self.inner.extend_from_slice(s.as_bytes());
    }

    pub fn push_byte(&mut self, b: u8) {
        self.inner.push(b);
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.inner
    }

    /// Infallible: the buffer only ever receives valid UTF-8.
    pub fn into_string(self) -> String {
        String::from_utf8(self.inner).expect("XmlBuf is always valid UTF-8")
    }

    /// Write `s` with XML text content escaping (`&`, `<`, `>`).
    ///
    /// Also escapes `\r` (U+000D) as `&#xD;` so that text containing a
    /// literal carriage return survives a parse → serialize → parse
    /// round-trip — without the char-ref, the second parse's §2.11
    /// end-of-line normalization would rewrite the `\r` to `\n`.  The
    /// same reasoning applies to NEL (U+0085) and LS (U+2028) under
    /// XML 1.1, but those are rare enough in text content that we let
    /// them through verbatim; documents written by sup-xml are
    /// version="1.0" by default and so won't trigger NEL/LS
    /// normalization on the receiving end.
    pub fn push_escaped_text(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '&'      => self.push_str("&amp;"),
                '<'      => self.push_str("&lt;"),
                '>'      => self.push_str("&gt;"),
                '\u{D}'  => self.push_str("&#xD;"),
                c        => self.push_char_for_charset(c),
            }
        }
    }

    /// Write `s` with XML attribute value escaping (`&`, `<`, `"`).
    ///
    /// Also escapes `\t` / `\n` / `\r` as `&#x9;` / `&#xA;` / `&#xD;`
    /// so the value survives a parse → serialize → parse round-trip
    /// — XML §3.3.3 attribute-value normalization rewrites literal
    /// tab / LF / CR to a single space on the receiving end, so
    /// preserving them through the wire requires char-ref escaping.
    pub fn push_escaped_attr(&mut self, s: &str) {
        for c in s.chars() {
            match c {
                '&'      => self.push_str("&amp;"),
                '<'      => self.push_str("&lt;"),
                '"'      => self.push_str("&quot;"),
                '\u{9}'  => self.push_str("&#x9;"),
                '\u{A}'  => self.push_str("&#xA;"),
                '\u{D}'  => self.push_str("&#xD;"),
                c        => self.push_char_for_charset(c),
            }
        }
    }
}

impl Default for XmlBuf {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_text() {
        let mut b = XmlBuf::new();
        b.push_escaped_text("a & b < c > d");
        assert_eq!(b.into_string(), "a &amp; b &lt; c &gt; d");
    }

    #[test]
    fn escape_attr() {
        let mut b = XmlBuf::new();
        b.push_escaped_attr(r#"say "hello" & <bye>"#);
        // '>' does not require escaping in attribute values (XML spec § 2.4)
        assert_eq!(b.into_string(), r#"say &quot;hello&quot; &amp; &lt;bye>"#);
    }

    #[test]
    fn unicode_passthrough() {
        let mut b = XmlBuf::new();
        b.push_escaped_text("日本語 🦀");
        assert_eq!(b.into_string(), "日本語 🦀");
    }

    #[test]
    fn new_is_empty() {
        let b = XmlBuf::new();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert_eq!(b.as_bytes(), &[] as &[u8]);
    }

    #[test]
    fn default_matches_new() {
        let b = XmlBuf::default();
        assert!(b.is_empty());
    }

    #[test]
    fn with_capacity_reserves_but_is_empty() {
        let b = XmlBuf::with_capacity(64);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn push_str_and_len() {
        let mut b = XmlBuf::new();
        b.push_str("hi");
        assert_eq!(b.len(), 2);
        assert!(!b.is_empty());
        assert_eq!(b.as_bytes(), b"hi");
    }

    #[test]
    fn push_byte_appends_one_byte() {
        let mut b = XmlBuf::new();
        b.push_byte(b'x');
        b.push_byte(b'y');
        assert_eq!(b.as_bytes(), b"xy");
    }

    #[test]
    fn into_bytes_returns_inner() {
        let mut b = XmlBuf::new();
        b.push_str("abc");
        assert_eq!(b.into_bytes(), b"abc".to_vec());
    }
}
