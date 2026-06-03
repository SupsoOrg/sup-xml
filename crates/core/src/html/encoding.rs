#![forbid(unsafe_code)]

//! WHATWG byte-stream encoding sniffing for HTML input.
//!
//! Sniffing precedence per [WHATWG § 12.2.3]:
//!
//! 1. Caller-supplied encoding (HTTP `Content-Type`, manual override
//!    via [`HtmlParseOptions::encoding_override`]).
//! 2. Byte-order mark (UTF-8 `EF BB BF`, UTF-16BE `FE FF`, UTF-16LE
//!    `FF FE`).
//! 3. Pre-scan up to [`HtmlParseOptions::encoding_sniff_window`]
//!    bytes (default 1024) for a `<meta charset>` or `<meta
//!    http-equiv="Content-Type">` declaration.
//! 4. Fall back to **Windows-1252** — explicitly *not* Latin-1.
//!    The HTML5 spec mandates this for backward compatibility with
//!    legacy web content.
//!
//! [WHATWG § 12.2.3]: https://html.spec.whatwg.org/multipage/parsing.html#determining-the-character-encoding
//!
//! # What's *not* implemented (deferred to v2 if a user asks)
//!
//! - The full WHATWG prescan algorithm for `<meta>` is approximated
//!   here.  We handle the two common forms (charset attribute and
//!   http-equiv content-type) with reasonable attribute parsing,
//!   but not every WHATWG edge case (deeply nested comments inside
//!   the head, weird whitespace, etc.).
//! - Encoding switches *during* parsing (the spec allows the parser
//!   to detect a different encoding in a later `<meta>` and restart).
//!   Our sniffer reads up to the configured window once and then
//!   commits to that encoding for the whole document.

use std::borrow::Cow;

use crate::encoding::{transcode_to_utf8_as, Encoding};
use crate::error::Result;

use super::options::HtmlParseOptions;

/// Sniff the encoding of an HTML byte stream per the WHATWG
/// algorithm.  Always returns *some* encoding — falls back to
/// Windows-1252 when no signal is found.
pub fn sniff_html_encoding(bytes: &[u8], opts: &HtmlParseOptions) -> Encoding {
    // 1. Caller-supplied encoding wins.
    if let Some(label) = opts.encoding_override.as_deref() {
        return label_to_encoding(label);
    }

    // 2. BOM.
    if let Some(enc) = sniff_bom(bytes) {
        return enc;
    }

    // 3. Pre-scan for meta charset.
    let window = opts.encoding_sniff_window.max(64).min(bytes.len());
    if let Some(label) = prescan_meta_charset(&bytes[..window]) {
        return label_to_encoding(&label);
    }

    // 4. Fall back to Windows-1252 (NOT Latin-1; spec is explicit).
    Encoding::Windows1252
}

/// Sniff + transcode in one step.  Returns UTF-8 bytes plus the
/// encoding that was used so the caller can record it on the
/// resulting `Document`.
pub fn decode_html_input<'a>(
    bytes: &'a [u8],
    opts: &HtmlParseOptions,
) -> Result<(Cow<'a, [u8]>, Encoding)> {
    let enc = sniff_html_encoding(bytes, opts);
    let decoded = transcode_to_utf8_as(bytes, enc.clone())?;
    Ok((decoded, enc))
}

/// Detect a byte-order mark.  Returns the implied encoding or
/// `None` if no BOM is present.
fn sniff_bom(bytes: &[u8]) -> Option<Encoding> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(Encoding::Utf8);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Some(Encoding::Utf16Be);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Some(Encoding::Utf16Le);
    }
    None
}

/// Pre-scan the first chunk of input for a `<meta>` tag with a
/// charset declaration.  Returns the label string as written, or
/// `None` if no usable declaration is found.
///
/// Pragmatic implementation — handles the two common WHATWG forms:
///   `<meta charset="UTF-8">`
///   `<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">`
/// plus the various quote/whitespace/case variations.  Skips
/// HTML comments so a charset inside `<!-- -->` doesn't trigger.
pub fn prescan_meta_charset(bytes: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos < bytes.len() {
        let b = bytes[pos];

        // Skip HTML comments.
        if starts_with_ascii(&bytes[pos..], b"<!--") {
            pos += 4;
            // Skip until "-->" or end.
            while pos + 2 < bytes.len()
                && !(bytes[pos] == b'-' && bytes[pos + 1] == b'-' && bytes[pos + 2] == b'>')
            {
                pos += 1;
            }
            pos = (pos + 3).min(bytes.len());
            continue;
        }

        // `<meta` followed by a tag-name terminator.
        if starts_with_ascii_ignore_case(&bytes[pos..], b"<meta")
            && bytes
                .get(pos + 5)
                .is_some_and(|&c| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0C | b'/' | b'>'))
        {
            pos += 5;
            if let Some(label) = parse_meta_attrs(bytes, &mut pos) {
                return Some(label);
            }
            // parse_meta_attrs already advanced past the tag.
            continue;
        }

        // Other tag — skip past the closing `>`.
        if b == b'<' {
            // `</`, `<!`, `<?`, or `<letter` — all "skip past `>`".
            if let Some(end) = find_byte(&bytes[pos..], b'>') {
                pos += end + 1;
                continue;
            }
            // No `>` in remaining bytes — bail out.
            return None;
        }

        pos += 1;
    }
    None
}

/// Parse attributes of a `<meta>` tag starting at `*pos` (just past
/// `<meta`).  Returns the discovered charset label if found.
/// Advances `*pos` past the tag's `>` regardless.
fn parse_meta_attrs(bytes: &[u8], pos: &mut usize) -> Option<String> {
    let mut http_equiv: Option<Vec<u8>> = None;
    let mut content: Option<Vec<u8>> = None;
    let mut charset: Option<String> = None;

    loop {
        // Skip whitespace and stray slashes.
        while *pos < bytes.len()
            && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r' | 0x0C | b'/')
        {
            *pos += 1;
        }
        if *pos >= bytes.len() {
            break;
        }
        if bytes[*pos] == b'>' {
            *pos += 1;
            break;
        }

        // Read attribute name (lower-cased) until '=', whitespace, '/', or '>'.
        let mut name = Vec::new();
        while *pos < bytes.len() {
            let c = bytes[*pos];
            if matches!(c, b'=' | b' ' | b'\t' | b'\n' | b'\r' | 0x0C | b'/' | b'>') {
                break;
            }
            name.push(c.to_ascii_lowercase());
            *pos += 1;
        }
        if name.is_empty() {
            // Stray `/` or `>` already advanced; loop again.
            if *pos < bytes.len() && bytes[*pos] == b'>' {
                *pos += 1;
                break;
            }
            *pos += 1;
            continue;
        }

        // Skip whitespace before potential `=`.
        while *pos < bytes.len() && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r' | 0x0C) {
            *pos += 1;
        }

        let mut value: Vec<u8> = Vec::new();
        if *pos < bytes.len() && bytes[*pos] == b'=' {
            *pos += 1;
            // Skip whitespace after `=`.
            while *pos < bytes.len()
                && matches!(bytes[*pos], b' ' | b'\t' | b'\n' | b'\r' | 0x0C)
            {
                *pos += 1;
            }
            if *pos < bytes.len() && (bytes[*pos] == b'"' || bytes[*pos] == b'\'') {
                let quote = bytes[*pos];
                *pos += 1;
                while *pos < bytes.len() && bytes[*pos] != quote {
                    value.push(bytes[*pos]);
                    *pos += 1;
                }
                if *pos < bytes.len() {
                    *pos += 1; // consume closing quote
                }
            } else {
                while *pos < bytes.len() {
                    let c = bytes[*pos];
                    if matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0C | b'>') {
                        break;
                    }
                    value.push(c);
                    *pos += 1;
                }
            }
        }

        // Dispatch on attribute name.
        match name.as_slice() {
            b"charset" => {
                if charset.is_none() {
                    charset = Some(String::from_utf8_lossy(&value).into_owned());
                }
            }
            b"http-equiv" => {
                if http_equiv.is_none() {
                    http_equiv = Some(value);
                }
            }
            b"content" => {
                if content.is_none() {
                    content = Some(value);
                }
            }
            _ => {}
        }
    }

    // Direct charset attribute wins.
    if let Some(c) = charset {
        return Some(c);
    }
    // Otherwise check http-equiv content-type.
    if let (Some(equiv), Some(content_val)) = (http_equiv, content) {
        if ascii_equal_ignore_case(&equiv, b"content-type") {
            return extract_charset_from_content(&content_val);
        }
    }
    None
}

/// Pull a `charset=NAME` value out of an `http-equiv` `content`
/// attribute string.  Looks for the substring `charset=` (case
/// insensitive) and reads the value (quoted or unquoted, terminated
/// by `;` or whitespace).
fn extract_charset_from_content(content: &[u8]) -> Option<String> {
    let lower: Vec<u8> = content.iter().map(|b| b.to_ascii_lowercase()).collect();
    let needle = b"charset=";
    let pos = lower.windows(needle.len()).position(|w| w == needle)?;
    let mut start = pos + needle.len();
    if start < content.len() && (content[start] == b'"' || content[start] == b'\'') {
        let quote = content[start];
        start += 1;
        let end = content[start..].iter().position(|&c| c == quote)?;
        Some(String::from_utf8_lossy(&content[start..start + end]).into_owned())
    } else {
        let end = content[start..]
            .iter()
            .position(|&c| matches!(c, b';' | b' ' | b'\t' | b'\n' | b'\r' | 0x0C))
            .unwrap_or(content.len() - start);
        Some(String::from_utf8_lossy(&content[start..start + end]).into_owned())
    }
}

/// Map a WHATWG encoding label to our [`Encoding`] enum.  Unknown
/// labels fall through to [`Encoding::Other`] which routes to
/// `encoding_rs` (when the `full-encodings` feature is on) or
/// errors out.
///
/// Common labels covered inline so we don't always go through
/// `encoding_rs`.  Note: WHATWG explicitly maps `iso-8859-1` to
/// Windows-1252 because legacy web content labels Latin-1 documents
/// that actually use Win1252 characters.
pub fn label_to_encoding(label: &str) -> Encoding {
    let trimmed = label.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "utf-8" | "utf8" | "unicode-1-1-utf-8" | "unicode11utf8" | "unicode20utf8"
        | "x-unicode20utf8" => Encoding::Utf8,
        "us-ascii" | "ascii" | "ansi_x3.4-1968" | "iso646-us" | "iso-ir-6"
        | "iso_646.irv:1991" | "csascii" => Encoding::Ascii,
        // WHATWG: iso-8859-1 / latin1 / cp1252 all map to Windows-1252.
        "iso-8859-1" | "latin1" | "iso8859-1" | "iso_8859-1" | "iso_8859-1:1987"
        | "windows-1252" | "cp1252" | "cp819" | "csisolatin1" | "ibm819" | "l1"
        | "x-cp1252" => Encoding::Windows1252,
        "utf-16" | "utf-16le" | "csunicode" | "ucs-2" | "unicode" | "unicodefeff" => {
            Encoding::Utf16Le
        }
        "utf-16be" | "unicodefffe" => Encoding::Utf16Be,
        // Py_UCS4 / wide-unicode source buffers: lxml hands these in as
        // UTF-32 (Python's internal representation for strings whose max
        // codepoint exceeds U+FFFF).  UCS-4 is the historical synonym.
        "utf-32" | "utf-32le" | "ucs-4" | "ucs-4le" | "ucs4" => Encoding::Utf32Le,
        "utf-32be" | "ucs-4be" => Encoding::Utf32Be,
        "ibm037" | "cp037" | "csibm037" => Encoding::Ebcdic037,
        // Anything else: hand to the Other path so encoding_rs can take a swing.
        _ => Encoding::Other(trimmed),
    }
}

// ── small byte-slice helpers ─────────────────────────────────────────────────

fn starts_with_ascii(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.starts_with(needle)
}

fn starts_with_ascii_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    if haystack.len() < needle.len() {
        return false;
    }
    haystack[..needle.len()]
        .iter()
        .zip(needle)
        .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
}

fn ascii_equal_ignore_case(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.to_ascii_lowercase() == y.to_ascii_lowercase())
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    memchr::memchr(needle, haystack)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> HtmlParseOptions {
        HtmlParseOptions::default()
    }

    #[test]
    fn bom_utf8() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"<html></html>");
        assert_eq!(sniff_html_encoding(&bytes, &opts()), Encoding::Utf8);
    }

    #[test]
    fn bom_utf16le() {
        let bytes = [0xFF, 0xFE, b'<', 0, b'a', 0];
        assert_eq!(sniff_html_encoding(&bytes, &opts()), Encoding::Utf16Le);
    }

    #[test]
    fn bom_utf16be() {
        let bytes = [0xFE, 0xFF, 0, b'<', 0, b'a'];
        assert_eq!(sniff_html_encoding(&bytes, &opts()), Encoding::Utf16Be);
    }

    #[test]
    fn meta_charset_simple() {
        let html = br#"<!DOCTYPE html><html><head><meta charset="UTF-8"></head></html>"#;
        assert_eq!(prescan_meta_charset(html), Some("UTF-8".into()));
    }

    #[test]
    fn meta_charset_unquoted() {
        let html = br"<meta charset=UTF-8>";
        assert_eq!(prescan_meta_charset(html), Some("UTF-8".into()));
    }

    #[test]
    fn meta_charset_single_quotes() {
        let html = br"<meta charset='windows-1252'>";
        assert_eq!(prescan_meta_charset(html), Some("windows-1252".into()));
    }

    #[test]
    fn meta_charset_case_insensitive() {
        let html = br#"<META CHARSET="ISO-8859-1">"#;
        assert_eq!(prescan_meta_charset(html), Some("ISO-8859-1".into()));
    }

    #[test]
    fn meta_http_equiv_content_type() {
        let html = br#"<meta http-equiv="Content-Type" content="text/html; charset=Shift_JIS">"#;
        assert_eq!(prescan_meta_charset(html), Some("Shift_JIS".into()));
    }

    #[test]
    fn meta_http_equiv_quoted_charset() {
        let html =
            br#"<meta http-equiv="content-type" content='text/html;charset="EUC-KR"'>"#;
        assert_eq!(prescan_meta_charset(html), Some("EUC-KR".into()));
    }

    #[test]
    fn meta_inside_comment_ignored() {
        let html =
            br#"<!-- <meta charset="UTF-8"> --><meta charset="ISO-8859-1">"#;
        assert_eq!(prescan_meta_charset(html), Some("ISO-8859-1".into()));
    }

    #[test]
    fn no_meta_falls_back_to_windows1252() {
        let html = br"<html><head><title>x</title></head><body>x</body></html>";
        assert_eq!(sniff_html_encoding(html, &opts()), Encoding::Windows1252);
    }

    #[test]
    fn override_wins_over_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"<html></html>");
        let mut o = opts();
        o.encoding_override = Some("ISO-8859-1".into());
        assert_eq!(sniff_html_encoding(&bytes, &o), Encoding::Windows1252);
    }

    #[test]
    fn label_iso88591_is_windows1252() {
        assert_eq!(label_to_encoding("iso-8859-1"), Encoding::Windows1252);
        assert_eq!(label_to_encoding("ISO-8859-1"), Encoding::Windows1252);
        assert_eq!(label_to_encoding(" Latin1 "), Encoding::Windows1252);
    }

    #[test]
    fn label_unknown_routes_to_other() {
        match label_to_encoding("shift_jis") {
            Encoding::Other(name) => assert_eq!(name, "shift_jis"),
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn decode_with_meta_windows1252_content() {
        // Windows-1252 byte 0x85 = ellipsis (U+2026), which is illegal
        // in Latin-1.  Verify we treat it as Windows-1252 per the meta
        // tag and decode correctly.
        let mut bytes = b"<meta charset=\"windows-1252\"><body>".to_vec();
        bytes.push(0x85);
        bytes.extend_from_slice(b"</body>");
        let (decoded, enc) = decode_html_input(&bytes, &opts()).unwrap();
        assert_eq!(enc, Encoding::Windows1252);
        let s = std::str::from_utf8(&decoded).expect("decoded must be UTF-8");
        assert!(s.contains('\u{2026}'), "ellipsis should be decoded: {s:?}");
    }

    #[test]
    fn decode_no_signal_uses_windows1252() {
        // No BOM, no meta charset — fall back to Windows-1252.
        let bytes = b"<html><body>plain</body></html>";
        let (_, enc) = decode_html_input(bytes, &opts()).unwrap();
        assert_eq!(enc, Encoding::Windows1252);
    }
}
