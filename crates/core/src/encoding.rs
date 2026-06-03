//! Encoding detection and transcoding to UTF-8.
//!
//! Tier 1 support: UTF-8, US-ASCII, ISO-8859-1, Windows-1252.  These cover the
//! large majority of legacy Western XML documents.  Other encodings (UTF-16,
//! Shift-JIS, etc.) are reported as [`Encoding::Other`] and produce a clear
//! error if you try to transcode them — Tier 2/3 work to follow.
//!
//! # Why composable
//!
//! Detection and transcoding live behind small, separate functions so callers
//! can use them however suits their pipeline:
//!
//! ```no_run
//! # use sup_xml_core::encoding::{detect, transcode_to_utf8};
//! # use sup_xml_core::{parse_bytes, ParseOptions};
//! # let bytes: &[u8] = b"";
//! // Auto-detect + transcode in one call, then parse the resulting UTF-8.
//! let utf8 = transcode_to_utf8(bytes)?;
//! let doc  = parse_bytes(&utf8, &ParseOptions::default())?;
//! # Ok::<(), sup_xml_core::XmlError>(())
//! ```
//!
//! For UTF-8 inputs the transcode step is **zero-copy** (returns `Cow::Borrowed`)
//! and only adds ~100 bytes of detection work to the parse path.

use std::borrow::Cow;

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};

/// A character encoding the parser may encounter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 — variable-length, the modern default.
    Utf8,
    /// US-ASCII — 7-bit subset of UTF-8.  Transcoding is a no-op.
    Ascii,
    /// ISO-8859-1, also known as Latin-1.  Byte X maps directly to U+00XX.
    Latin1,
    /// Windows-1252 (codepage 1252).  Identical to Latin-1 outside the
    /// 0x80–0x9F range, which Windows-1252 uses for printable characters
    /// (curly quotes, em dash, ellipsis, €, etc.).
    Windows1252,
    /// UTF-16 little-endian.  Detected via the FF FE BOM or an explicit
    /// `encoding="UTF-16LE"` declaration.
    Utf16Le,
    /// UTF-16 big-endian.  Detected via the FE FF BOM or an explicit
    /// `encoding="UTF-16BE"` declaration.
    Utf16Be,
    /// UTF-32 little-endian.  Detected via the FF FE 00 00 BOM, the XML
    /// Appendix F autodetect signature 3C 00 00 00, or an explicit
    /// `encoding="UTF-32LE"` / `encoding="UCS-4LE"` declaration.
    Utf32Le,
    /// UTF-32 big-endian.  Detected via the 00 00 FE FF BOM, the XML
    /// Appendix F autodetect signature 00 00 00 3C, or an explicit
    /// `encoding="UTF-32BE"` / `encoding="UCS-4BE"` declaration.
    Utf32Be,
    /// IBM037 (CCSID 37 / CP037), the EBCDIC US/Canada Latin code page.
    /// Detected via the XML spec's Appendix F autodetection signature
    /// `4C 6F A7 94` (= "<?xm" in EBCDIC) — which all IBM-family variants
    /// share — or an explicit `encoding="IBM037"` declaration.
    Ebcdic037,
    /// IBM500 (CCSID 500), International EBCDIC.  Same control-character
    /// layout as IBM037 with seven ASCII-region punctuation positions
    /// rearranged.  Detected via the IBM037 autodetect signature plus an
    /// explicit `encoding="IBM500"` declaration.
    Ebcdic500,
    /// IBM1047 (CCSID 1047), EBCDIC Open Systems / z/OS Unix Services
    /// Latin-1.  Differs from IBM037 in the IBM500 ASCII-region punctuation
    /// plus the LF/NEL swap that makes EBCDIC text behave under Unix
    /// line-handling code.
    Ebcdic1047,
    /// IBM1140 (CCSID 1140), EBCDIC US/Canada Latin with the Euro sign
    /// update.  Identical to IBM037 except byte 0x9F maps to U+20AC (€)
    /// instead of U+00A4 (¤).
    Ebcdic1140,
    /// A recognized encoding name we do not yet know how to transcode.
    /// Stored as the name as written in the document's XML declaration.
    Other(String),
}

impl Encoding {
    /// Canonical name used in XML declarations.
    pub fn name(&self) -> &str {
        match self {
            Encoding::Utf8        => "UTF-8",
            Encoding::Ascii       => "US-ASCII",
            Encoding::Latin1      => "ISO-8859-1",
            Encoding::Windows1252 => "windows-1252",
            Encoding::Utf16Le     => "UTF-16LE",
            Encoding::Utf16Be     => "UTF-16BE",
            Encoding::Utf32Le     => "UTF-32LE",
            Encoding::Utf32Be     => "UTF-32BE",
            Encoding::Ebcdic037   => "IBM037",
            Encoding::Ebcdic500   => "IBM500",
            Encoding::Ebcdic1047  => "IBM1047",
            Encoding::Ebcdic1140  => "IBM1140",
            Encoding::Other(s)    => s,
        }
    }
}

// ── detection ─────────────────────────────────────────────────────────────────

/// Sniff the encoding of an XML document from its first bytes.
///
/// The algorithm:
/// 1. If a BOM is present, use it (currently only UTF-8 BOM is handled in
///    Tier 1; UTF-16 BOMs return [`Encoding::Other`] until we add UTF-16).
/// 2. Otherwise, look for a `<?xml ... encoding="..."?>` declaration in the
///    first ~200 bytes.  The XML spec guarantees the declaration is in an
///    ASCII-compatible form for every encoding the spec mentions, so this
///    lookahead works without knowing the encoding yet.
/// 3. If neither is found, default to UTF-8.
pub fn detect(bytes: &[u8]) -> Encoding {
    // 1. BOMs.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Encoding::Utf8;
    }
    // UTF-32 BOMs must be checked before UTF-16 BOMs — the UTF-32LE BOM
    // `FF FE 00 00` starts with the UTF-16LE BOM bytes.
    if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
        return Encoding::Utf32Be;
    }
    if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
        return Encoding::Utf32Le;
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Encoding::Utf16Be;
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Encoding::Utf16Le;
    }

    // 2a. XML 1.0 Appendix F auto-detection for EBCDIC.
    //
    // EBCDIC documents begin with `<?xm`, which is byte sequence
    // `4C 6F A7 94` in every IBM-family EBCDIC code page we support
    // (037, 500, 1047, 1140 all share the same Latin-letter positions).
    // No other Tier 1/2/3 encoding produces this pattern at the document
    // start.
    //
    // Once the signature matches we need to pick the *variant*.  Because
    // all four variants encode the characters used in an XML declaration
    // (`<?xml ... encoding="..." ?>`) the same way, we tentatively
    // transcode the head with IBM037 and read the actual variant out of
    // the resulting UTF-8 declaration.  Documents with no declaration —
    // or a declaration naming something non-EBCDIC — fall back to
    // IBM037, the most common variant.
    if bytes.starts_with(&[0x4C, 0x6F, 0xA7, 0x94]) {
        let head_len = bytes.len().min(200);
        let head_utf8 = transcode_single_byte(&bytes[..head_len], &IBM037_TO_UTF8);
        if let Some(name) = read_xml_decl_encoding(&head_utf8) {
            let parsed = encoding_from_name(&name);
            if matches!(parsed,
                Encoding::Ebcdic037 | Encoding::Ebcdic500
                | Encoding::Ebcdic1047 | Encoding::Ebcdic1140)
            {
                return parsed;
            }
        }
        return Encoding::Ebcdic037;
    }

    // 2b. XML 1.0 Appendix F auto-detection for UTF-32 / UTF-16 without a BOM.
    //
    // Every well-formed XML document must begin with `<` (byte 0x3C) — either
    // the start of `<?xml`, a comment `<!--`, a PI `<?`, or the root element.
    // In UTF-32 the opening `<` gives a unique 4-byte signature with three
    // NUL bytes; in UTF-16 the `<` plus the next ASCII char gives:
    //   UTF-32BE: 00 00 00 3C
    //   UTF-32LE: 3C 00 00 00
    //   UTF-16BE: 00 3C 00 X   (where X is the ASCII byte after `<`)
    //   UTF-16LE: 3C 00 X 00
    // The UTF-16 patterns require byte[1]=0x3C or byte[2] non-zero, so they
    // don't overlap with UTF-32 — but check UTF-32 first to make intent clear.
    if bytes.len() >= 4 {
        if bytes[0] == 0x00 && bytes[1] == 0x00 && bytes[2] == 0x00 && bytes[3] == 0x3C {
            return Encoding::Utf32Be;
        }
        if bytes[0] == 0x3C && bytes[1] == 0x00 && bytes[2] == 0x00 && bytes[3] == 0x00 {
            return Encoding::Utf32Le;
        }
        if bytes[0] == 0x00 && bytes[1] == 0x3C && bytes[2] == 0x00 && bytes[3] != 0x00 {
            return Encoding::Utf16Be;
        }
        if bytes[0] == 0x3C && bytes[1] == 0x00 && bytes[2] != 0x00 && bytes[3] == 0x00 {
            return Encoding::Utf16Le;
        }
    }

    // 3. XML declaration (works for any ASCII-compatible byte encoding).
    let head = &bytes[..bytes.len().min(200)];
    if let Some(name) = read_xml_decl_encoding(head) {
        return encoding_from_name(&name);
    }

    // 4. Default.
    Encoding::Utf8
}

/// Map an encoding name (case-insensitive, with common aliases) onto an
/// [`Encoding`] variant.  Unknown names become [`Encoding::Other`].
pub fn encoding_from_name(name: &str) -> Encoding {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "utf-8" | "utf8"                              => Encoding::Utf8,
        "us-ascii" | "ascii" | "ansi_x3.4-1968"       => Encoding::Ascii,
        "iso-8859-1" | "iso_8859-1" | "latin1" | "latin-1" | "l1" | "8859-1"
                                                      => Encoding::Latin1,
        "windows-1252" | "cp1252" | "cp-1252" | "1252"
                                                      => Encoding::Windows1252,
        "utf-16le" | "utf16le" | "utf-16 le"          => Encoding::Utf16Le,
        "utf-16be" | "utf16be" | "utf-16 be"          => Encoding::Utf16Be,
        "utf-32le" | "utf32le" | "utf-32 le" | "ucs-4le" | "ucs4le" | "ucs-4-le"
                                                      => Encoding::Utf32Le,
        "utf-32be" | "utf32be" | "utf-32 be" | "ucs-4be" | "ucs4be" | "ucs-4-be"
                                                      => Encoding::Utf32Be,
        "ibm037" | "ibm-037" | "cp037" | "cp-037"
            | "037" | "csibm037"
            | "ebcdic-cp-us" | "ebcdic-cp-ca"
            | "ebcdic-cp-wt" | "ebcdic-cp-nl"         => Encoding::Ebcdic037,
        "ibm500" | "ibm-500" | "cp500" | "cp-500"
            | "500" | "csibm500"
            | "ebcdic-cp-be" | "ebcdic-cp-ch"         => Encoding::Ebcdic500,
        "ibm1047" | "ibm-1047" | "cp1047" | "cp-1047"
            | "1047" | "csibm1047"                    => Encoding::Ebcdic1047,
        "ibm1140" | "ibm-1140" | "cp1140" | "cp-1140"
            | "1140" | "csibm1140"
            | "ibm01140" | "cp01140"
            | "ebcdic-us-37+euro"                     => Encoding::Ebcdic1140,
        // Generic "UTF-16" without an explicit endianness MUST come with a
        // BOM per the XML spec; if a doc says encoding="UTF-16" but has no
        // BOM we can't decode it, so we route this through Other and let the
        // transcoder report a clear error.
        _                                             => Encoding::Other(name.to_string()),
    }
}

/// Return the encoding name as written in the document's `<?xml ...
/// encoding="X"?>` declaration, if present.  Works on the raw bytes
/// before any transcoding, so it reports the name a consumer-supplied
/// converter should key on (e.g. `"EUC-JP"`), unnormalized.
pub fn declared_encoding_name(bytes: &[u8]) -> Option<String> {
    read_xml_decl_encoding(bytes)
}

/// Extract the `encoding="..."` value from an `<?xml ...?>` declaration if
/// present, otherwise `None`.  Bytes-level so it works regardless of the
/// document's actual encoding.
fn read_xml_decl_encoding(head: &[u8]) -> Option<String> {
    let start = find_bytes(head, b"<?xml")?;
    let after = &head[start + 5..];
    // First char after `<?xml` must be whitespace per the XML spec.
    if !matches!(after.first()?, b' ' | b'\t' | b'\r' | b'\n') {
        return None;
    }
    // Now find the `encoding` keyword somewhere inside the declaration.
    let end = find_bytes(after, b"?>").unwrap_or(after.len());
    let decl = &after[..end];
    let kw = find_bytes(decl, b"encoding")?;
    let mut i = kw + 8;
    while i < decl.len() && matches!(decl[i], b' ' | b'\t' | b'\r' | b'\n') { i += 1; }
    if i >= decl.len() || decl[i] != b'=' { return None; }
    i += 1;
    while i < decl.len() && matches!(decl[i], b' ' | b'\t' | b'\r' | b'\n') { i += 1; }
    let quote = match decl.get(i)? {
        b'"' | b'\'' => decl[i],
        _ => return None,
    };
    i += 1;
    let val_start = i;
    while i < decl.len() && decl[i] != quote { i += 1; }
    if i >= decl.len() { return None; }
    // Encoding names are always ASCII per XML spec, so from_utf8 is safe here
    // even for non-UTF-8 documents.
    std::str::from_utf8(&decl[val_start..i]).ok().map(|s| s.to_string())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() { return None; }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ── transcoding ───────────────────────────────────────────────────────────────

/// Detect the encoding of `bytes` and transcode them into UTF-8.
///
/// For UTF-8 / US-ASCII input the result is [`Cow::Borrowed`] (zero copy).
/// For Latin-1 / Windows-1252 it is [`Cow::Owned`] — these encodings produce
/// UTF-8 about 1.0–1.5× the input size.
///
/// **Does not verify** that the inner `<?xml ... encoding="X" ?>` declaration
/// agrees with the encoding detected from the bytes — for that, use
/// [`transcode_to_utf8_strict`].
pub fn transcode_to_utf8(bytes: &[u8]) -> Result<Cow<'_, [u8]>> {
    let enc = detect(bytes);
    transcode_to_utf8_as(bytes, enc)
}

/// Like [`transcode_to_utf8`] but **also verifies** that the inner XML
/// declaration's `encoding="X"` attribute agrees with the encoding detected
/// from the byte stream.  Catches malformed documents like a UTF-8 BOM
/// paired with `encoding="ISO-8859-1"` or a UTF-16 BOM paired with
/// `encoding="UTF-8"`.
///
/// Use this when consuming untrusted XML from external sources.  Synthetic
/// re-encoded fixtures (e.g. a UTF-8 doc re-encoded to UTF-16 keeping its
/// `encoding="UTF-8"` declaration) will fail this check — use the lenient
/// [`transcode_to_utf8`] for those.
pub fn transcode_to_utf8_strict(bytes: &[u8]) -> Result<Cow<'_, [u8]>> {
    let enc = detect(bytes);
    let transcoded = transcode_to_utf8_as(bytes, enc.clone())?;
    verify_declaration_matches(&enc, &transcoded)?;
    Ok(transcoded)
}

/// Read the inner `<?xml ... encoding="X" ?>` declaration from already-UTF-8
/// `transcoded` bytes and verify that `X` is consistent with `detected`.
fn verify_declaration_matches(detected: &Encoding, transcoded: &[u8]) -> Result<()> {
    let head = &transcoded[..transcoded.len().min(200)];
    let declared = match read_xml_decl_encoding(head) {
        Some(s) => s,
        None    => return Ok(()), // no declaration to check
    };
    if encodings_match(detected, &declared) {
        return Ok(());
    }
    Err(XmlError::new(
        ErrorDomain::Encoding,
        ErrorLevel::Fatal,
        format!(
            "encoding declaration {declared:?} contradicts the encoding detected \
             from the byte stream ({})",
            detected.name(),
        ),
    ))
}

/// Whether the encoding `detected` from the bytes and the encoding name in
/// the XML declaration mean the same thing.
fn encodings_match(detected: &Encoding, declared: &str) -> bool {
    let parsed = encoding_from_name(declared);
    match (detected, &parsed) {
        (a, b) if a == b => true,
        // ASCII bytes can validly be labelled UTF-8 (and vice versa).
        (Encoding::Utf8,  Encoding::Ascii) | (Encoding::Ascii, Encoding::Utf8) => true,
        // Generic "UTF-16" (without endianness suffix) parses as `Other`;
        // accept it as a match for either detected endianness.
        (Encoding::Utf16Le | Encoding::Utf16Be, Encoding::Other(s))
            if s.eq_ignore_ascii_case("UTF-16") || s.eq_ignore_ascii_case("UTF16") => true,
        // Generic "UTF-32" / "UCS-4" (without endianness suffix) parses as
        // `Other`; accept it as a match for either detected endianness.
        (Encoding::Utf32Le | Encoding::Utf32Be, Encoding::Other(s))
            if s.eq_ignore_ascii_case("UTF-32") || s.eq_ignore_ascii_case("UTF32")
                || s.eq_ignore_ascii_case("UCS-4") || s.eq_ignore_ascii_case("UCS4") => true,
        // Unknown labels — compare by name, case-insensitive.
        (Encoding::Other(a), Encoding::Other(b)) if a.eq_ignore_ascii_case(b) => true,
        _ => false,
    }
}

/// Transcode `bytes` into UTF-8 assuming they are in the given `encoding`.
///
/// Use this when you already know the encoding (e.g. from an HTTP
/// `Content-Type` header) and want to skip detection.  Returns an error for
/// encodings not yet supported (currently anything beyond Tier 1).
pub fn transcode_to_utf8_as(bytes: &[u8], encoding: Encoding) -> Result<Cow<'_, [u8]>> {
    match encoding {
        Encoding::Utf8 | Encoding::Ascii => Ok(Cow::Borrowed(strip_bom(bytes))),
        Encoding::Latin1                 => Ok(Cow::Owned(transcode_latin1(strip_bom(bytes)))),
        Encoding::Windows1252            => Ok(Cow::Owned(transcode_windows1252(strip_bom(bytes)))),
        Encoding::Utf16Le                => Ok(Cow::Owned(transcode_utf16(strip_utf16_bom(bytes, false), false)?)),
        Encoding::Utf16Be                => Ok(Cow::Owned(transcode_utf16(strip_utf16_bom(bytes, true), true)?)),
        Encoding::Utf32Le                => Ok(Cow::Owned(transcode_utf32(strip_utf32_bom(bytes, false), false)?)),
        Encoding::Utf32Be                => Ok(Cow::Owned(transcode_utf32(strip_utf32_bom(bytes, true), true)?)),
        Encoding::Ebcdic037              => Ok(Cow::Owned(transcode_single_byte(bytes, &IBM037_TO_UTF8))),
        Encoding::Ebcdic500              => Ok(Cow::Owned(transcode_single_byte(bytes, &IBM500_TO_UTF8))),
        Encoding::Ebcdic1047             => Ok(Cow::Owned(transcode_single_byte(bytes, &IBM1047_TO_UTF8))),
        Encoding::Ebcdic1140             => Ok(Cow::Owned(transcode_single_byte(bytes, &IBM1140_TO_UTF8))),
        Encoding::Other(name)            => Ok(Cow::Owned(transcode_other(bytes, &name)?)),
    }
}

/// IBM037 (EBCDIC US/Canada Latin) to Unicode mapping, one entry per byte.
///
/// Standard CCSID 37 / CP037 table.  Printable chars in the ASCII range and
/// extended Latin-1 letters live in the 0x40–0xFE range; 0x00–0x3F are the
/// EBCDIC control-character positions (NUL, SOH, etc.).
///
/// Exposed publicly so external tooling (test fixture generators, benchmark
/// harnesses) can build the inverse map for UTF-8 → IBM037 round-tripping.
pub const IBM037_TO_UNICODE: [u16; 256] = [
    // 0x00-0x0F: NUL SOH STX ETX SEL HT  RNL DEL GE  SPS RPT VT  FF  CR  SO  SI
    0x0000, 0x0001, 0x0002, 0x0003, 0x009C, 0x0009, 0x0086, 0x007F,
    0x0097, 0x008D, 0x008E, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F,
    // 0x10-0x1F: DLE DC1 DC2 DC3 RES NEL BS  POC CAN EM  UBS CU1 IFS IGS IRS IUS
    0x0010, 0x0011, 0x0012, 0x0013, 0x009D, 0x0085, 0x0008, 0x0087,
    0x0018, 0x0019, 0x0092, 0x008F, 0x001C, 0x001D, 0x001E, 0x001F,
    // 0x20-0x2F: DS  SOS FS  WUS BYP LF  ETB ESC SA  SFE SM  CSP MFA ENQ ACK BEL
    0x0080, 0x0081, 0x0082, 0x0083, 0x0084, 0x000A, 0x0017, 0x001B,
    0x0088, 0x0089, 0x008A, 0x008B, 0x008C, 0x0005, 0x0006, 0x0007,
    // 0x30-0x3F:                 SYN     PP  TRN NBS EOT SBS IT  RFF CU3 DC4 NAK     SUB
    0x0090, 0x0091, 0x0016, 0x0093, 0x0094, 0x0095, 0x0096, 0x0004,
    0x0098, 0x0099, 0x009A, 0x009B, 0x0014, 0x0015, 0x009E, 0x001A,
    // 0x40-0x4F: SP   NBSP â     ä     à     á     ã     å     ç     ñ     ¢   .     <     (     +     |
    0x0020, 0x00A0, 0x00E2, 0x00E4, 0x00E0, 0x00E1, 0x00E3, 0x00E5,
    0x00E7, 0x00F1, 0x00A2, 0x002E, 0x003C, 0x0028, 0x002B, 0x007C,
    // 0x50-0x5F: &    é     ê     ë     è     í     î     ï     ì     ß    !    $     *     )     ;     ¬
    0x0026, 0x00E9, 0x00EA, 0x00EB, 0x00E8, 0x00ED, 0x00EE, 0x00EF,
    0x00EC, 0x00DF, 0x0021, 0x0024, 0x002A, 0x0029, 0x003B, 0x00AC,
    // 0x60-0x6F: -    /     Â     Ä     À     Á     Ã     Å     Ç     Ñ     ¦   ,     %     _     >     ?
    0x002D, 0x002F, 0x00C2, 0x00C4, 0x00C0, 0x00C1, 0x00C3, 0x00C5,
    0x00C7, 0x00D1, 0x00A6, 0x002C, 0x0025, 0x005F, 0x003E, 0x003F,
    // 0x70-0x7F: ø    É     Ê     Ë     È     Í     Î     Ï     Ì     `    :    #     @     '     =     "
    0x00F8, 0x00C9, 0x00CA, 0x00CB, 0x00C8, 0x00CD, 0x00CE, 0x00CF,
    0x00CC, 0x0060, 0x003A, 0x0023, 0x0040, 0x0027, 0x003D, 0x0022,
    // 0x80-0x8F: Ø    a     b     c     d     e     f     g     h     i     «    »     ð     ý     þ     ±
    0x00D8, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067,
    0x0068, 0x0069, 0x00AB, 0x00BB, 0x00F0, 0x00FD, 0x00FE, 0x00B1,
    // 0x90-0x9F: °    j     k     l     m     n     o     p     q     r     ª    º     æ     ¸     Æ     ¤
    0x00B0, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070,
    0x0071, 0x0072, 0x00AA, 0x00BA, 0x00E6, 0x00B8, 0x00C6, 0x00A4,
    // 0xA0-0xAF: µ    ~     s     t     u     v     w     x     y     z     ¡    ¿     Ð     Ý     Þ     ®
    0x00B5, 0x007E, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078,
    0x0079, 0x007A, 0x00A1, 0x00BF, 0x00D0, 0x00DD, 0x00DE, 0x00AE,
    // 0xB0-0xBF: ^    £     ¥     ·     ©     §     ¶     ¼     ½     ¾     [    ]     ¯     ¨     ´     ×
    0x005E, 0x00A3, 0x00A5, 0x00B7, 0x00A9, 0x00A7, 0x00B6, 0x00BC,
    0x00BD, 0x00BE, 0x005B, 0x005D, 0x00AF, 0x00A8, 0x00B4, 0x00D7,
    // 0xC0-0xCF: {    A     B     C     D     E     F     G     H     I     SHY  ô     ö     ò     ó     õ
    0x007B, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047,
    0x0048, 0x0049, 0x00AD, 0x00F4, 0x00F6, 0x00F2, 0x00F3, 0x00F5,
    // 0xD0-0xDF: }    J     K     L     M     N     O     P     Q     R     ¹    û     ü     ù     ú     ÿ
    0x007D, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050,
    0x0051, 0x0052, 0x00B9, 0x00FB, 0x00FC, 0x00F9, 0x00FA, 0x00FF,
    // 0xE0-0xEF: \    ÷     S     T     U     V     W     X     Y     Z     ²    Ô     Ö     Ò     Ó     Õ
    0x005C, 0x00F7, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058,
    0x0059, 0x005A, 0x00B2, 0x00D4, 0x00D6, 0x00D2, 0x00D3, 0x00D5,
    // 0xF0-0xFF: 0    1     2     3     4     5     6     7     8     9     ³    Û     Ü     Ù     Ú     EO
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037,
    0x0038, 0x0039, 0x00B3, 0x00DB, 0x00DC, 0x00D9, 0x00DA, 0x009F,
];

/// IBM1140 (EBCDIC US/Canada Latin with Euro update) to Unicode mapping.
///
/// CCSID 1140 is CCSID 37 (IBM037) with **one** byte position updated to
/// carry the Euro sign — adopted around 2000 when the EU adopted the euro.
/// The change is at byte 0x9F: U+00A4 (currency symbol ¤) → U+20AC (€).
///
/// Reference: IBM CCSID 1140; Unicode Consortium mapping
/// `MAPPINGS/VENDORS/MICSFT/EBCDIC/CP1140.TXT`.
pub const IBM1140_TO_UNICODE: [u16; 256] = {
    let mut t = IBM037_TO_UNICODE;
    t[0x9F] = 0x20AC; // ¤ → € (Euro sign — CCSID 1140 update of CCSID 37)
    t
};

/// IBM500 (International EBCDIC) to Unicode mapping.
///
/// CCSID 500 differs from CCSID 37 (IBM037) in exactly seven byte
/// positions — the ASCII-region punctuation is rearranged so `[`, `]`,
/// `!`, `^`, and `|` live where European national-variant EBCDIC pages
/// historically placed them.  This is the "International" EBCDIC layout.
///
/// Reference: IBM CCSID 500; Unicode Consortium mapping
/// `MAPPINGS/VENDORS/MICSFT/EBCDIC/CP500.TXT`.
pub const IBM500_TO_UNICODE: [u16; 256] = {
    let mut t = IBM037_TO_UNICODE;
    // 7-byte delta vs IBM037:
    t[0x4A] = 0x005B; // ¢ → [
    t[0x4F] = 0x0021; // | → !
    t[0x5A] = 0x005D; // ! → ]
    t[0x5F] = 0x005E; // ¬ → ^
    t[0xB0] = 0x00A2; // ^ → ¢
    t[0xBA] = 0x00AC; // [ → ¬
    t[0xBB] = 0x007C; // ] → |
    t
};

/// IBM1047 (EBCDIC Open Systems / z/OS Unix Services Latin-1) to Unicode.
///
/// CCSID 1047 differs from CCSID 37 (IBM037) in two groups: it shares the
/// IBM500 punctuation rearrangement (7 bytes), *plus* it swaps the LF and
/// NEL code points at bytes 0x15 and 0x25 — the famous "z/OS Unix
/// Services LF convention" that makes EBCDIC text behave correctly with
/// Unix line-handling code.
///
/// Reference: IBM CCSID 1047; Unicode Consortium mapping
/// `MAPPINGS/VENDORS/MICSFT/EBCDIC/CP1047.TXT`.
pub const IBM1047_TO_UNICODE: [u16; 256] = {
    let mut t = IBM037_TO_UNICODE;
    // LF/NEL swap (z/OS Unix Services line-ending convention):
    t[0x15] = 0x000A; // NEL → LF
    t[0x25] = 0x0085; // LF  → NEL
    // Same 7-byte ASCII-region rearrangement as IBM500:
    t[0x4A] = 0x005B; // ¢ → [
    t[0x4F] = 0x0021; // | → !
    t[0x5A] = 0x005D; // ! → ]
    t[0x5F] = 0x005E; // ¬ → ^
    t[0xB0] = 0x00A2; // ^ → ¢
    t[0xBA] = 0x00AC; // [ → ¬
    t[0xBB] = 0x007C; // ] → |
    t
};


/// Packed UTF-8 encoding for one byte of a single-byte legacy encoding.
///
/// Each entry is `[length, byte0, byte1, byte2]` — the first byte is the
/// UTF-8 length (1, 2, or 3) and the next three are the UTF-8 bytes
/// themselves, zero-padded for shorter sequences.  All EBCDIC variants we
/// support emit BMP-only code points, so 3 bytes is the maximum (e.g., the
/// Euro sign U+20AC in IBM1140 encodes to `E2 82 AC`).
type SingleByteUtf8Table = [[u8; 4]; 256];

/// Build the packed UTF-8 table from a byte-to-Unicode mapping.
///
/// Evaluated at compile time — each variant table costs zero runtime work
/// to construct.
const fn build_utf8_table(unicode_table: &[u16; 256]) -> SingleByteUtf8Table {
    let mut t = [[0u8; 4]; 256];
    let mut i = 0;
    while i < 256 {
        let cp = unicode_table[i] as u32;
        if cp < 0x80 {
            t[i] = [1, cp as u8, 0, 0];
        } else if cp < 0x800 {
            t[i] = [
                2,
                0xC0 | (cp >> 6) as u8,
                0x80 | (cp & 0x3F) as u8,
                0,
            ];
        } else {
            t[i] = [
                3,
                0xE0 | (cp >> 12) as u8,
                0x80 | ((cp >> 6) & 0x3F) as u8,
                0x80 | (cp & 0x3F) as u8,
            ];
        }
        i += 1;
    }
    t
}

const IBM037_TO_UTF8:  SingleByteUtf8Table = build_utf8_table(&IBM037_TO_UNICODE);
const IBM500_TO_UTF8:  SingleByteUtf8Table = build_utf8_table(&IBM500_TO_UNICODE);
const IBM1047_TO_UTF8: SingleByteUtf8Table = build_utf8_table(&IBM1047_TO_UNICODE);
const IBM1140_TO_UTF8: SingleByteUtf8Table = build_utf8_table(&IBM1140_TO_UNICODE);

/// Transcode bytes from a single-byte legacy encoding into UTF-8.
///
/// Hot loop is three unconditional memory writes + one branchless length
/// update per input byte.  Each iteration writes 3 bytes (even when the
/// UTF-8 sequence is shorter); `len` advances by the actual length stored
/// in the table's first byte, so the unused bytes are overwritten by the
/// next iteration.
fn transcode_single_byte(bytes: &[u8], table: &SingleByteUtf8Table) -> Vec<u8> {
    // Worst case: every input byte expands to 3 UTF-8 bytes (BMP triple-byte).
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() * 3);
    let ptr = out.as_mut_ptr();
    let mut len = 0usize;

    for &b in bytes {
        let entry = table[b as usize];
        // SAFETY: `out` has reserved capacity `bytes.len() * 3`.  At iteration
        // start, `len ≤ 3 * (iterations_so_far)`, and we write at offsets
        // `len`, `len + 1`, `len + 2` — the last is at most
        // `3 * bytes.len() - 1`, within capacity.
        unsafe {
            ptr.add(len    ).write(entry[1]);
            ptr.add(len + 1).write(entry[2]);
            ptr.add(len + 2).write(entry[3]);
        }
        len += entry[0] as usize;
    }

    // SAFETY: `len` bytes have been initialized at offsets 0..len, and
    // `len ≤ 3 * bytes.len()` which is the reserved capacity.
    unsafe { out.set_len(len); }
    out
}

/// Transcode an encoding we don't natively support in Tier 1/2.
///
/// With the `full-encodings` feature (default-on), this routes through the
/// `encoding_rs` crate, which knows every encoding the WHATWG spec defines.
/// Without the feature, this returns an `ErrorDomain::Encoding` error
/// referencing the encoding name and telling the user how to enable support.
#[cfg(feature = "full-encodings")]
fn transcode_other(bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    let enc = encoding_rs::Encoding::for_label(name.as_bytes())
        .ok_or_else(|| XmlError::new(
            ErrorDomain::Encoding,
            ErrorLevel::Fatal,
            format!("encoding {name:?} is not recognized by encoding_rs"),
        ))?;
    // Decode without re-doing BOM handling — we've already done it.
    let (decoded, _had_errors) = enc.decode_without_bom_handling(bytes);
    // `decoded` is a Cow<str>; we want UTF-8 bytes out.  Invalid sequences
    // become U+FFFD (Unicode replacement character), which is a legal XML
    // character per XML 1.0 §2.2 (0xE000–0xFFFD), so the parser will accept
    // them — surfacing the issue as data rather than a parse failure.
    Ok(decoded.into_owned().into_bytes())
}

#[cfg(not(feature = "full-encodings"))]
fn transcode_other(_bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    Err(XmlError::new(
        ErrorDomain::Encoding,
        ErrorLevel::Fatal,
        format!(
            "encoding {name:?} is not supported — rebuild sup-xml-core with \
             the default 'full-encodings' feature enabled to pull in encoding_rs"
        ),
    ))
}

/// Strip a UTF-8 BOM (and only a UTF-8 BOM) from the front of `bytes`.
fn strip_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { &bytes[3..] } else { bytes }
}

/// Strip a UTF-16 BOM matching the given endianness, if present.
fn strip_utf16_bom(bytes: &[u8], big_endian: bool) -> &[u8] {
    let bom: [u8; 2] = if big_endian { [0xFE, 0xFF] } else { [0xFF, 0xFE] };
    if bytes.starts_with(&bom) { &bytes[2..] } else { bytes }
}

/// Strip a UTF-32 BOM matching the given endianness, if present.
fn strip_utf32_bom(bytes: &[u8], big_endian: bool) -> &[u8] {
    let bom: [u8; 4] = if big_endian {
        [0x00, 0x00, 0xFE, 0xFF]
    } else {
        [0xFF, 0xFE, 0x00, 0x00]
    };
    if bytes.starts_with(&bom) { &bytes[4..] } else { bytes }
}

/// Transcode UTF-16 bytes into UTF-8, handling surrogate pairs.
fn transcode_utf16(bytes: &[u8], big_endian: bool) -> Result<Vec<u8>> {
    if bytes.len() % 2 != 0 {
        return Err(XmlError::new(
            ErrorDomain::Encoding,
            ErrorLevel::Fatal,
            "UTF-16 input length must be even",
        ));
    }
    let decode_u16 = |i: usize| -> u16 {
        if big_endian {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        }
    };

    // UTF-16 → UTF-8 averages ~1.5× when input is Latin/ASCII heavy but can
    // shrink for CJK (3 UTF-8 bytes for what was 2 UTF-16 bytes — already
    // ~1.5×).  Either way, len() is a decent first-cut capacity.
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let u = decode_u16(i);
        i += 2;

        let cp: u32 = if (0xD800..=0xDBFF).contains(&u) {
            // High surrogate — must be followed by a low surrogate.
            if i + 2 > bytes.len() {
                return Err(XmlError::new(
                    ErrorDomain::Encoding,
                    ErrorLevel::Fatal,
                    "lone UTF-16 high surrogate at end of input",
                ));
            }
            let low = decode_u16(i);
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(XmlError::new(
                    ErrorDomain::Encoding,
                    ErrorLevel::Fatal,
                    format!("UTF-16 high surrogate U+{u:04X} not followed by low surrogate (got U+{low:04X})"),
                ));
            }
            i += 2;
            0x10000 + (((u as u32 - 0xD800) << 10) | (low as u32 - 0xDC00))
        } else if (0xDC00..=0xDFFF).contains(&u) {
            return Err(XmlError::new(
                ErrorDomain::Encoding,
                ErrorLevel::Fatal,
                format!("lone UTF-16 low surrogate U+{u:04X}"),
            ));
        } else {
            u as u32
        };

        encode_utf8_codepoint(cp, &mut out);
    }
    Ok(out)
}

/// Transcode UTF-32 bytes into UTF-8.
///
/// Each 4-byte chunk is a full Unicode scalar value — there are no surrogate
/// pairs in UTF-32.  Validation rejects surrogates (U+D800..U+DFFF, which
/// are not valid scalars) and code points above U+10FFFF (outside the
/// Unicode range).
fn transcode_utf32(bytes: &[u8], big_endian: bool) -> Result<Vec<u8>> {
    if bytes.len() % 4 != 0 {
        return Err(XmlError::new(
            ErrorDomain::Encoding,
            ErrorLevel::Fatal,
            "UTF-32 input length must be a multiple of 4",
        ));
    }
    let decode_u32 = |i: usize| -> u32 {
        if big_endian {
            u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
        } else {
            u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
        }
    };

    // Worst case is 1 UTF-8 byte per UTF-32 byte (a non-BMP scalar encodes
    // to 4 UTF-8 bytes from 4 UTF-32 bytes).  Typical text shrinks well
    // below that — ASCII is 4× — but pre-sizing to bytes.len() avoids any
    // reallocation.
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let cp = decode_u32(i);
        i += 4;

        if cp > 0x10FFFF {
            return Err(XmlError::new(
                ErrorDomain::Encoding,
                ErrorLevel::Fatal,
                format!("UTF-32 code point U+{cp:08X} is outside the Unicode range"),
            ));
        }
        if (0xD800..=0xDFFF).contains(&cp) {
            return Err(XmlError::new(
                ErrorDomain::Encoding,
                ErrorLevel::Fatal,
                format!("UTF-32 code point U+{cp:04X} is a surrogate (not a valid scalar)"),
            ));
        }
        encode_utf8_codepoint(cp, &mut out);
    }
    Ok(out)
}

/// Transcode ISO-8859-1 (Latin-1) bytes into UTF-8.
///
/// Every byte X in the input is the Unicode scalar value U+00XX.  ASCII bytes
/// (< 0x80) are passed through unchanged; 0x80–0xFF expand to 2-byte UTF-8.
///
/// # Implementation
///
/// SWAR (SIMD-within-a-register) scan in 8-byte chunks.  For each chunk we
/// mask the high bits — if the result is zero, the whole chunk is ASCII and
/// we bulk-copy it.  Otherwise the trailing-zeros count points us at the
/// first non-ASCII byte, we copy the ASCII prefix, expand the one byte, and
/// resume scanning from the byte after.  On mostly-ASCII XML this turns a
/// per-byte branchy loop into a series of 8-byte memcpys.
fn transcode_latin1(bytes: &[u8]) -> Vec<u8> {
    const ASCII_MASK: u64 = 0x8080_8080_8080_8080;
    // Worst case: every byte expands to 2 — pre-size for that to avoid reallocs.
    let mut out = Vec::with_capacity(bytes.len() * 2);
    let mut pos = 0;

    while pos + 8 <= bytes.len() {
        // SAFETY: pos + 8 <= bytes.len() guaranteed by the loop bound, and
        // `try_into` on the slice gives us a [u8; 8] which has the right size.
        let chunk = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        let hi = chunk & ASCII_MASK;
        if hi == 0 {
            // All 8 bytes are ASCII — bulk copy.
            out.extend_from_slice(&bytes[pos..pos + 8]);
            pos += 8;
        } else {
            // Position of the first high-bit byte within the chunk.
            let off = (hi.trailing_zeros() / 8) as usize;
            if off > 0 {
                out.extend_from_slice(&bytes[pos..pos + off]);
            }
            let b = bytes[pos + off];
            out.push(0xC0 | (b >> 6));
            out.push(0x80 | (b & 0x3F));
            pos += off + 1;
        }
    }

    // Tail (< 8 bytes left) — fall back to the simple per-byte loop.
    while pos < bytes.len() {
        let b = bytes[pos];
        if b < 0x80 {
            out.push(b);
        } else {
            out.push(0xC0 | (b >> 6));
            out.push(0x80 | (b & 0x3F));
        }
        pos += 1;
    }

    out
}

/// Mapping table for Windows-1252 code points in the 0x80–0x9F range.
///
/// Outside this range Windows-1252 == ISO-8859-1.  The five "undefined" slots
/// (0x81, 0x8D, 0x8F, 0x90, 0x9D) are kept as their numeric value, which is
/// what the WHATWG decoding spec and Python's `cp1252` codec both do.
const WIN1252_HI: [u32; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021,
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F,
    0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// Transcode Windows-1252 bytes into UTF-8.
///
/// Same SWAR ASCII-run trick as [`transcode_latin1`].  Non-ASCII bytes split
/// further: 0x80–0x9F go through the [`WIN1252_HI`] table (codepoint can be
/// up to U+20AC → 3-byte UTF-8), while 0xA0–0xFF use the direct Latin-1
/// expansion (always 2-byte UTF-8).
fn transcode_windows1252(bytes: &[u8]) -> Vec<u8> {
    const ASCII_MASK: u64 = 0x8080_8080_8080_8080;
    // Worst case is 3 bytes out per byte in (for the curly-quote / euro range).
    let mut out = Vec::with_capacity(bytes.len() * 2);
    let mut pos = 0;

    #[inline]
    fn emit_non_ascii(out: &mut Vec<u8>, b: u8) {
        if (0x80..0xA0).contains(&b) {
            let cp = WIN1252_HI[(b - 0x80) as usize];
            if cp < 0x80 {
                out.push(cp as u8);
            } else if cp < 0x800 {
                out.push(0xC0 | (cp >> 6) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            } else {
                out.push(0xE0 | (cp >> 12) as u8);
                out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                out.push(0x80 | (cp & 0x3F) as u8);
            }
        } else {
            out.push(0xC0 | (b >> 6));
            out.push(0x80 | (b & 0x3F));
        }
    }

    while pos + 8 <= bytes.len() {
        let chunk = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        let hi = chunk & ASCII_MASK;
        if hi == 0 {
            out.extend_from_slice(&bytes[pos..pos + 8]);
            pos += 8;
        } else {
            let off = (hi.trailing_zeros() / 8) as usize;
            if off > 0 {
                out.extend_from_slice(&bytes[pos..pos + off]);
            }
            emit_non_ascii(&mut out, bytes[pos + off]);
            pos += off + 1;
        }
    }

    while pos < bytes.len() {
        let b = bytes[pos];
        if b < 0x80 {
            out.push(b);
        } else {
            emit_non_ascii(&mut out, b);
        }
        pos += 1;
    }

    out
}

/// Append the UTF-8 encoding of `cp` to `out`.  Generic for future encodings
/// that produce code points outside the Tier 1 range.
fn encode_utf8_codepoint(cp: u32, out: &mut Vec<u8>) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x10000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_defaults_to_utf8() {
        assert_eq!(detect(b"<r/>"), Encoding::Utf8);
    }

    #[test]
    fn detect_utf8_bom() {
        let bytes = [0xEF, 0xBB, 0xBF, b'<', b'r', b'/', b'>'];
        assert_eq!(detect(&bytes), Encoding::Utf8);
    }

    #[test]
    fn detect_utf16_bom() {
        assert_eq!(detect(&[0xFE, 0xFF, 0, b'<']), Encoding::Utf16Be);
        assert_eq!(detect(&[0xFF, 0xFE, b'<', 0]), Encoding::Utf16Le);
    }

    #[test]
    fn detect_utf16_named_in_declaration() {
        let be = br#"<?xml version="1.0" encoding="UTF-16BE"?><r/>"#;
        let le = br#"<?xml version="1.0" encoding="UTF-16LE"?><r/>"#;
        assert_eq!(detect(be), Encoding::Utf16Be);
        assert_eq!(detect(le), Encoding::Utf16Le);
    }

    #[test]
    fn detect_generic_utf16_no_bom_is_other() {
        // "UTF-16" without endianness AND without a BOM is technically invalid;
        // we surface it as Other and let transcode error cleanly.
        let bytes = br#"<?xml version="1.0" encoding="UTF-16"?><r/>"#;
        assert!(matches!(detect(bytes), Encoding::Other(s) if s == "UTF-16"));
    }

    #[test]
    fn transcode_utf16_le_with_bom() {
        // BOM 0xFF 0xFE, then ASCII chars as 2-byte little-endian sequences:
        // '<' 'r' '/' '>' → 3C 00 72 00 2F 00 3E 00
        let bytes: &[u8] = &[
            0xFF, 0xFE,
            0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E, 0x00,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_utf16_be_with_bom() {
        let bytes: &[u8] = &[
            0xFE, 0xFF,
            0x00, 0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_utf16_le_surrogate_pair() {
        // U+1F600 GRINNING FACE — surrogate pair 0xD83D 0xDE00 in UTF-16.
        // LE bytes: 3D D8 00 DE
        let bytes: &[u8] = &[0xFF, 0xFE, 0x3D, 0xD8, 0x00, 0xDE];
        let out = transcode_to_utf8(bytes).unwrap();
        // UTF-8 of U+1F600: F0 9F 98 80
        assert_eq!(&*out, &[0xF0, 0x9F, 0x98, 0x80]);
        assert_eq!(std::str::from_utf8(&out).unwrap(), "😀");
    }

    #[test]
    fn transcode_utf16_lone_high_surrogate_errors() {
        // 0xD83D LE with no following code unit
        let bytes: &[u8] = &[0xFF, 0xFE, 0x3D, 0xD8];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("high surrogate"), "got: {:?}", err.message);
    }

    #[test]
    fn transcode_utf16_lone_low_surrogate_errors() {
        // 0xDE00 LE — low surrogate without a preceding high one
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0xDE];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("low surrogate"), "got: {:?}", err.message);
    }

    #[test]
    fn transcode_utf16_odd_length_errors() {
        let bytes: &[u8] = &[0xFF, 0xFE, 0x3C];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("even"), "got: {:?}", err.message);
    }

    #[test]
    fn detect_utf16_be_without_bom() {
        // "<?xml" in UTF-16BE → 00 3C 00 3F 00 78 ...
        let bytes: &[u8] = &[0x00, 0x3C, 0x00, 0x3F, 0x00, 0x78];
        assert_eq!(detect(bytes), Encoding::Utf16Be);
    }

    #[test]
    fn detect_utf16_le_without_bom() {
        // "<?xml" in UTF-16LE → 3C 00 3F 00 78 00 ...
        let bytes: &[u8] = &[0x3C, 0x00, 0x3F, 0x00, 0x78, 0x00];
        assert_eq!(detect(bytes), Encoding::Utf16Le);
    }

    #[test]
    fn transcode_utf16_be_without_bom_resilient() {
        // Same as detect test but actually transcode end-to-end.  Bytes for
        // "<r/>" in UTF-16BE without BOM.
        let bytes: &[u8] = &[0x00, 0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn detect_utf32_be_bom() {
        let bytes: &[u8] = &[0x00, 0x00, 0xFE, 0xFF, 0x00, 0x00, 0x00, 0x3C];
        assert_eq!(detect(bytes), Encoding::Utf32Be);
    }

    #[test]
    fn detect_utf32_le_bom() {
        // Note: BOM starts with FF FE, which is also the UTF-16LE BOM — the
        // ordering in detect() must check UTF-32LE first.
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00];
        assert_eq!(detect(bytes), Encoding::Utf32Le);
    }

    #[test]
    fn detect_utf32_be_without_bom() {
        // "<?" first chars in UTF-32BE: 00 00 00 3C 00 00 00 3F
        let bytes: &[u8] = &[0x00, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x3F];
        assert_eq!(detect(bytes), Encoding::Utf32Be);
    }

    #[test]
    fn detect_utf32_le_without_bom() {
        // "<?" first chars in UTF-32LE: 3C 00 00 00 3F 00 00 00
        let bytes: &[u8] = &[0x3C, 0x00, 0x00, 0x00, 0x3F, 0x00, 0x00, 0x00];
        assert_eq!(detect(bytes), Encoding::Utf32Le);
    }

    #[test]
    fn detect_utf32_named_in_declaration() {
        let be = br#"<?xml version="1.0" encoding="UTF-32BE"?><r/>"#;
        let le = br#"<?xml version="1.0" encoding="UTF-32LE"?><r/>"#;
        assert_eq!(detect(be), Encoding::Utf32Be);
        assert_eq!(detect(le), Encoding::Utf32Le);
    }

    #[test]
    fn detect_ucs4_aliases() {
        let be = br#"<?xml version="1.0" encoding="UCS-4BE"?><r/>"#;
        let le = br#"<?xml version="1.0" encoding="UCS-4LE"?><r/>"#;
        assert_eq!(detect(be), Encoding::Utf32Be);
        assert_eq!(detect(le), Encoding::Utf32Le);
    }

    #[test]
    fn detect_generic_utf32_no_bom_is_other() {
        // "UTF-32" without endianness AND without a BOM is invalid; surface
        // as Other and let transcode error cleanly.
        let bytes = br#"<?xml version="1.0" encoding="UTF-32"?><r/>"#;
        assert!(matches!(detect(bytes), Encoding::Other(s) if s == "UTF-32"));
    }

    #[test]
    fn transcode_utf32_le_with_bom() {
        // BOM, then '<' 'r' '/' '>' each as a 4-byte LE scalar.
        let bytes: &[u8] = &[
            0xFF, 0xFE, 0x00, 0x00,
            0x3C, 0x00, 0x00, 0x00,
            0x72, 0x00, 0x00, 0x00,
            0x2F, 0x00, 0x00, 0x00,
            0x3E, 0x00, 0x00, 0x00,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_utf32_be_with_bom() {
        let bytes: &[u8] = &[
            0x00, 0x00, 0xFE, 0xFF,
            0x00, 0x00, 0x00, 0x3C,
            0x00, 0x00, 0x00, 0x72,
            0x00, 0x00, 0x00, 0x2F,
            0x00, 0x00, 0x00, 0x3E,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_utf32_be_without_bom_resilient() {
        // "<r/>" in UTF-32BE without BOM.
        let bytes: &[u8] = &[
            0x00, 0x00, 0x00, 0x3C,
            0x00, 0x00, 0x00, 0x72,
            0x00, 0x00, 0x00, 0x2F,
            0x00, 0x00, 0x00, 0x3E,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_utf32_smp_codepoint() {
        // U+1F600 GRINNING FACE — UTF-32 stores it directly, no surrogates.
        // LE bytes: 00 F6 01 00
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x00, 0xF6, 0x01, 0x00];
        let out = transcode_to_utf8(bytes).unwrap();
        // UTF-8 of U+1F600: F0 9F 98 80
        assert_eq!(&*out, &[0xF0, 0x9F, 0x98, 0x80]);
        assert_eq!(std::str::from_utf8(&out).unwrap(), "😀");
    }

    #[test]
    fn transcode_utf32_surrogate_errors() {
        // 0x0000D83D is a high surrogate — invalid as a UTF-32 scalar.
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x3D, 0xD8, 0x00, 0x00];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("surrogate"), "got: {:?}", err.message);
    }

    #[test]
    fn transcode_utf32_out_of_range_errors() {
        // 0x00110000 is one past U+10FFFF (the Unicode maximum).
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x11, 0x00];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("Unicode range"), "got: {:?}", err.message);
    }

    #[test]
    fn transcode_utf32_misaligned_length_errors() {
        // 6 bytes after the BOM — not a multiple of 4.
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x72, 0x00];
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert!(err.message.contains("multiple of 4"), "got: {:?}", err.message);
    }

    #[test]
    fn transcode_utf32_strips_bom_only_once() {
        // BOM, then '<' as LE.  Verify the BOM is consumed and we don't
        // emit a stray U+FEFF in the output.
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(out.as_ref(), b"<");
    }

    #[test]
    fn detect_ebcdic_signature() {
        // "<?xm" in IBM037 → 4C 6F A7 94
        let bytes: &[u8] = &[0x4C, 0x6F, 0xA7, 0x94];
        assert_eq!(detect(bytes), Encoding::Ebcdic037);
    }

    #[test]
    fn detect_ebcdic_named() {
        let bytes = br#"<?xml version="1.0" encoding="IBM037"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Ebcdic037);
        let bytes = br#"<?xml version="1.0" encoding="CP037"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Ebcdic037);
        let bytes = br#"<?xml version="1.0" encoding="EBCDIC-CP-US"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Ebcdic037);
    }

    #[test]
    fn transcode_ebcdic_minimal_via_explicit_encoding() {
        // "<r/>" in IBM037 alone has no autodetect signature (autodetection
        // requires `<?xml` → "4C 6F A7 94").  Use the `_as` variant when you
        // know the encoding rather than relying on detection.
        let bytes: &[u8] = &[0x4C, 0x99, 0x61, 0x6E];
        let out = transcode_to_utf8_as(bytes, Encoding::Ebcdic037).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "<r/>");
    }

    #[test]
    fn transcode_ebcdic_xml_declaration() {
        // "<?xml version='1.0' encoding='IBM037'?><r>café</r>" in IBM037.
        //   '<' 4C   '?' 6F   'x' A7   'm' 94   'l' 93
        //   ' ' 40
        //   'v' A5   'e' 85   'r' 99   's' A2   'i' 89   'o' 96   'n' 95
        //   '=' 7E   '\'' 7D   '1' F1   '.' 4B   '0' F0   '\'' 7D
        //   ' ' 40
        //   'e' 85   'n' 95   'c' 83   'o' 96   'd' 84   'i' 89   'n' 95   'g' 87
        //   '=' 7E   '\'' 7D   'I' C9   'B' C2   'M' D4   '0' F0   '3' F3   '7' F7   '\'' 7D
        //   '?' 6F   '>' 6E
        //   '<' 4C   'r' 99   '>' 6E
        //   'c' 83   'a' 81   'f' 86   'é' 51   (note: 'é' in IBM037 is 0x51!)
        //   '<' 4C   '/' 61   'r' 99   '>' 6E
        let bytes: &[u8] = &[
            0x4C, 0x6F, 0xA7, 0x94, 0x93, 0x40,
            0xA5, 0x85, 0x99, 0xA2, 0x89, 0x96, 0x95, 0x7E, 0x7D, 0xF1, 0x4B, 0xF0, 0x7D, 0x40,
            0x85, 0x95, 0x83, 0x96, 0x84, 0x89, 0x95, 0x87, 0x7E, 0x7D,
            0xC9, 0xC2, 0xD4, 0xF0, 0xF3, 0xF7, 0x7D, 0x6F, 0x6E,
            0x4C, 0x99, 0x6E,
            0x83, 0x81, 0x86, 0x51,
            0x4C, 0x61, 0x99, 0x6E,
        ];
        let out = transcode_to_utf8(bytes).unwrap();
        let s   = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("café"), "expected 'café' in output, got: {s:?}");
        assert!(s.contains("encoding='IBM037'"), "expected encoding decl preserved, got: {s:?}");
    }

    // ── EBCDIC variants: IBM1140, IBM500, IBM1047 ────────────────────────────

    #[test]
    fn ibm1140_differs_from_ibm037_at_byte_9f_only() {
        // CCSID 1140 is CCSID 37 with byte 0x9F updated to carry the Euro
        // sign.  Verify the table is *exactly* the IBM037 table with that
        // one change.
        for i in 0..256 {
            if i == 0x9F {
                assert_eq!(IBM1140_TO_UNICODE[i], 0x20AC,
                           "IBM1140 0x9F must map to Euro sign");
                assert_eq!(IBM037_TO_UNICODE[i], 0x00A4,
                           "IBM037 0x9F must remain currency sign ¤");
            } else {
                assert_eq!(IBM1140_TO_UNICODE[i], IBM037_TO_UNICODE[i],
                           "IBM1140 should match IBM037 at byte 0x{i:02X}");
            }
        }
    }

    #[test]
    fn ibm500_seven_byte_punctuation_swap_from_ibm037() {
        // International EBCDIC moves [, ], !, ^, |, ¬, ¢ around.  Verify
        // each delta and that nothing else changed.
        let deltas: &[(u8, u16, u16)] = &[
            (0x4A, 0x00A2, 0x005B),  // ¢ → [
            (0x4F, 0x007C, 0x0021),  // | → !
            (0x5A, 0x0021, 0x005D),  // ! → ]
            (0x5F, 0x00AC, 0x005E),  // ¬ → ^
            (0xB0, 0x005E, 0x00A2),  // ^ → ¢
            (0xBA, 0x005B, 0x00AC),  // [ → ¬
            (0xBB, 0x005D, 0x007C),  // ] → |
        ];
        for &(byte, ibm037_cp, ibm500_cp) in deltas {
            assert_eq!(IBM037_TO_UNICODE[byte as usize], ibm037_cp);
            assert_eq!(IBM500_TO_UNICODE[byte as usize], ibm500_cp);
        }
        let changed: std::collections::HashSet<u8> = deltas.iter().map(|(b, _, _)| *b).collect();
        for i in 0..=255u8 {
            if !changed.contains(&i) {
                assert_eq!(IBM500_TO_UNICODE[i as usize], IBM037_TO_UNICODE[i as usize],
                           "IBM500 should match IBM037 at byte 0x{i:02X}");
            }
        }
    }

    #[test]
    fn ibm1047_swaps_lf_and_nel_in_addition_to_ibm500_deltas() {
        // IBM1047 = IBM500 + LF/NEL swap.  Cross-check.
        assert_eq!(IBM1047_TO_UNICODE[0x15], 0x000A, "IBM1047 0x15 = LF");
        assert_eq!(IBM1047_TO_UNICODE[0x25], 0x0085, "IBM1047 0x25 = NEL");
        assert_eq!(IBM037_TO_UNICODE [0x15], 0x0085, "IBM037 0x15 = NEL");
        assert_eq!(IBM037_TO_UNICODE [0x25], 0x000A, "IBM037 0x25 = LF");
        // Punctuation rearrangement matches IBM500.
        assert_eq!(IBM1047_TO_UNICODE[0x4A], IBM500_TO_UNICODE[0x4A]);
        assert_eq!(IBM1047_TO_UNICODE[0xBB], IBM500_TO_UNICODE[0xBB]);
        // Outside the deltas, IBM1047 matches IBM037.
        assert_eq!(IBM1047_TO_UNICODE[0xC1], IBM037_TO_UNICODE[0xC1]); // 'A'
        assert_eq!(IBM1047_TO_UNICODE[0xF0], IBM037_TO_UNICODE[0xF0]); // '0'
    }

    #[test]
    fn detect_ibm1140_via_declaration_after_ebcdic_signature() {
        // "<?xml version='1.0' encoding='IBM1140'?><r/>" in IBM1140.
        // IBM1140 encodes ASCII letters identically to IBM037, so we can
        // build the bytes by hand from IBM037 positions.
        let bytes: &[u8] = &[
            0x4C, 0x6F, 0xA7, 0x94, 0x93, 0x40, // <?xml SP
            0xA5, 0x85, 0x99, 0xA2, 0x89, 0x96, 0x95, 0x7E, 0x7D, // version='
            0xF1, 0x4B, 0xF0, 0x7D, 0x40, // 1.0' SP
            0x85, 0x95, 0x83, 0x96, 0x84, 0x89, 0x95, 0x87, 0x7E, 0x7D, // encoding='
            0xC9, 0xC2, 0xD4, 0xF1, 0xF1, 0xF4, 0xF0, 0x7D, // IBM1140'
            0x6F, 0x6E, // ?>
            0x4C, 0x99, 0x61, 0x6E, // <r/>
        ];
        assert_eq!(detect(bytes), Encoding::Ebcdic1140);
    }

    #[test]
    fn detect_ibm500_via_declaration_after_ebcdic_signature() {
        let bytes: &[u8] = &[
            0x4C, 0x6F, 0xA7, 0x94, 0x93, 0x40, // <?xml SP
            0xA5, 0x85, 0x99, 0xA2, 0x89, 0x96, 0x95, 0x7E, 0x7D, // version='
            0xF1, 0x4B, 0xF0, 0x7D, 0x40,
            0x85, 0x95, 0x83, 0x96, 0x84, 0x89, 0x95, 0x87, 0x7E, 0x7D, // encoding='
            0xC9, 0xC2, 0xD4, 0xF5, 0xF0, 0xF0, 0x7D, // IBM500'
            0x6F, 0x6E,
            0x4C, 0x99, 0x61, 0x6E,
        ];
        assert_eq!(detect(bytes), Encoding::Ebcdic500);
    }

    #[test]
    fn detect_ibm1047_via_declaration_after_ebcdic_signature() {
        let bytes: &[u8] = &[
            0x4C, 0x6F, 0xA7, 0x94, 0x93, 0x40,
            0xA5, 0x85, 0x99, 0xA2, 0x89, 0x96, 0x95, 0x7E, 0x7D,
            0xF1, 0x4B, 0xF0, 0x7D, 0x40,
            0x85, 0x95, 0x83, 0x96, 0x84, 0x89, 0x95, 0x87, 0x7E, 0x7D,
            0xC9, 0xC2, 0xD4, 0xF1, 0xF0, 0xF4, 0xF7, 0x7D, // IBM1047'
            0x6F, 0x6E,
            0x4C, 0x99, 0x61, 0x6E,
        ];
        assert_eq!(detect(bytes), Encoding::Ebcdic1047);
    }

    #[test]
    fn ibm1140_euro_sign_round_trips() {
        // Single-element doc with a Euro sign.  IBM1140 byte 0x9F → €.
        let bytes: &[u8] = &[0x4C, 0x99, 0x6E, 0x9F, 0x4C, 0x61, 0x99, 0x6E];
        let out = transcode_to_utf8_as(bytes, Encoding::Ebcdic1140).unwrap();
        let s   = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "<r>€</r>", "got: {s:?}");
    }

    #[test]
    fn ibm1140_byte_9f_is_currency_under_ibm037() {
        // Same input as the Euro test above, but decoded as plain IBM037 —
        // byte 0x9F should come out as ¤ (currency sign), proving the
        // variants really do differ at this position.
        let bytes: &[u8] = &[0x4C, 0x99, 0x6E, 0x9F, 0x4C, 0x61, 0x99, 0x6E];
        let out = transcode_to_utf8_as(bytes, Encoding::Ebcdic037).unwrap();
        let s   = std::str::from_utf8(&out).unwrap();
        assert_eq!(s, "<r>¤</r>", "got: {s:?}");
    }

    #[test]
    fn ibm500_left_bracket_round_trips() {
        // Byte 0x4A in IBM500 is `[`.  Decode and confirm.
        let bytes: &[u8] = &[0x4A]; // '['
        let out = transcode_to_utf8_as(bytes, Encoding::Ebcdic500).unwrap();
        assert_eq!(&*out, b"[", "IBM500 0x4A must decode to '['");
        // Same byte under IBM037 is `¢`.
        let out2 = transcode_to_utf8_as(bytes, Encoding::Ebcdic037).unwrap();
        assert_eq!(std::str::from_utf8(&out2).unwrap(), "¢",
                   "IBM037 0x4A must decode to ¢");
    }

    #[test]
    fn ibm1047_lf_and_punctuation_round_trip() {
        // 0x15 in IBM1047 is LF (U+000A).  Plus 0x4A = '[', 0xBB = '|'
        // (both inherited from the IBM500-style punctuation layout).
        let bytes: &[u8] = &[0x15, 0x4A, 0xBB];
        let out = transcode_to_utf8_as(bytes, Encoding::Ebcdic1047).unwrap();
        assert_eq!(&*out, b"\n[|", "IBM1047 line/bracket/pipe round-trip");
        // The same bytes under IBM037 give NEL + ¢ + ]
        let out2 = transcode_to_utf8_as(bytes, Encoding::Ebcdic037).unwrap();
        let s2 = std::str::from_utf8(&out2).unwrap();
        assert!(s2.starts_with("\u{0085}"), "IBM037 0x15 must be NEL (U+0085)");
        assert!(s2.contains('¢'),           "IBM037 0x4A must be ¢");
        assert!(s2.ends_with(']'),          "IBM037 0xBB must be ]");
    }

    #[test]
    fn ibm1140_aliases_resolve() {
        // Various ways callers might spell IBM1140 in an XML declaration.
        for name in ["IBM1140", "ibm1140", "CP1140", "cp01140", "IBM01140",
                     "csibm1140", "ebcdic-us-37+euro"]
        {
            let bytes = format!("<?xml version=\"1.0\" encoding=\"{name}\"?><r/>");
            assert_eq!(detect(bytes.as_bytes()), Encoding::Ebcdic1140,
                       "{name} should resolve to Ebcdic1140");
        }
    }

    #[test]
    fn detect_xml_decl_iso_8859_1() {
        let bytes = br#"<?xml version="1.0" encoding="ISO-8859-1"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Latin1);
    }

    #[test]
    fn detect_xml_decl_windows_1252() {
        let bytes = br#"<?xml version="1.0" encoding="Windows-1252"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Windows1252);
    }

    #[test]
    fn detect_xml_decl_us_ascii() {
        let bytes = br#"<?xml version="1.0" encoding="US-ASCII"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Ascii);
    }

    #[test]
    fn detect_xml_decl_utf_8_explicit() {
        let bytes = br#"<?xml version="1.0" encoding="UTF-8"?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Utf8);
    }

    #[test]
    fn detect_xml_decl_single_quoted() {
        let bytes = br#"<?xml version='1.0' encoding='ISO-8859-1'?><r/>"#;
        assert_eq!(detect(bytes), Encoding::Latin1);
    }

    #[test]
    fn detect_xml_decl_unknown_encoding_is_other() {
        let bytes = br#"<?xml version="1.0" encoding="ISO-8859-2"?><r/>"#;
        assert!(matches!(detect(bytes), Encoding::Other(s) if s == "ISO-8859-2"));
    }

    #[test]
    fn transcode_utf8_is_zero_copy() {
        let bytes: &[u8] = b"<r>plain ascii</r>";
        let out = transcode_to_utf8(bytes).unwrap();
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), bytes);
    }

    #[test]
    fn transcode_latin1_caf_e_acute() {
        // <r>café</r> in Latin-1: 'é' is byte 0xE9.
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><r>caf\xe9</r>";
        let out = transcode_to_utf8(bytes).unwrap();
        assert!(matches!(out, Cow::Owned(_)));
        let s = std::str::from_utf8(&out).expect("output is valid UTF-8");
        assert!(s.contains("café"), "expected 'café' in output, got: {s:?}");
    }

    #[test]
    fn transcode_windows1252_ellipsis() {
        // Byte 0x85 is the horizontal ellipsis '…' (U+2026) in Windows-1252.
        // (In Latin-1 0x85 is the NEL control character, which is wrong here.)
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"Windows-1252\"?><r>and\x85</r>";
        let out = transcode_to_utf8(bytes).unwrap();
        let s = std::str::from_utf8(&out).expect("output is valid UTF-8");
        assert!(s.contains("and…"), "expected 'and…' (U+2026 ellipsis), got: {s:?}");
    }

    /// With `full-encodings` (default), GB2312 routes through encoding_rs and
    /// decodes cleanly.  Without the feature, it returns an Encoding error.
    #[cfg(feature = "full-encodings")]
    #[test]
    fn transcode_other_encoding_decodes_via_encoding_rs() {
        // "<r>中</r>" in GBK (a superset of GB2312).  '中' is byte pair D6 D0.
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"GB2312\"?><r>\xD6\xD0</r>";
        let out = transcode_to_utf8(bytes).expect("encoding_rs decodes GB2312");
        let s = std::str::from_utf8(&out).expect("decoded bytes are valid UTF-8");
        assert!(s.contains("中"), "expected '中' (U+4E2D) in decoded text, got: {s:?}");
    }

    #[cfg(not(feature = "full-encodings"))]
    #[test]
    fn transcode_other_encoding_errors_without_feature() {
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"GB2312\"?><r/>";
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert_eq!(err.domain, ErrorDomain::Encoding);
        assert!(err.message.contains("GB2312"), "expected error to mention GB2312, got: {:?}", err.message);
    }

    #[cfg(feature = "full-encodings")]
    #[test]
    fn transcode_other_encoding_unknown_label_errors() {
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"not-a-real-encoding-name\"?><r/>";
        let err = transcode_to_utf8(bytes).unwrap_err();
        assert_eq!(err.domain, ErrorDomain::Encoding);
        assert!(
            err.message.contains("not recognized"),
            "expected message to flag the label as unrecognized, got: {:?}", err.message,
        );
    }

    #[cfg(feature = "full-encodings")]
    #[test]
    fn transcode_iso_8859_2_via_encoding_rs() {
        // ISO-8859-2 is Latin-2; we don't have it in Tier 1.  Byte 0xB1
        // is 'ą' (U+0105) in Latin-2.
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"ISO-8859-2\"?><r>\xB1</r>";
        let out = transcode_to_utf8(bytes).expect("encoding_rs decodes Latin-2");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("ą"), "expected 'ą' (U+0105) in decoded text, got: {s:?}");
    }

    #[cfg(feature = "full-encodings")]
    #[test]
    fn transcode_shift_jis_via_encoding_rs() {
        // Shift_JIS: 'あ' (U+3042) is byte pair 0x82 0xA0.
        let bytes: &[u8] = b"<?xml version=\"1.0\" encoding=\"Shift_JIS\"?><r>\x82\xA0</r>";
        let out = transcode_to_utf8(bytes).expect("encoding_rs decodes Shift_JIS");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("あ"), "expected 'あ' (U+3042) in decoded text, got: {s:?}");
    }

    #[test]
    fn transcode_strips_utf8_bom() {
        let bytes: &[u8] = &[0xEF, 0xBB, 0xBF, b'<', b'r', b'/', b'>'];
        let out = transcode_to_utf8(bytes).unwrap();
        assert_eq!(out.as_ref(), b"<r/>");
    }

    #[test]
    fn transcode_to_utf8_as_explicit() {
        let bytes: &[u8] = b"caf\xe9";
        let out = transcode_to_utf8_as(bytes, Encoding::Latin1).unwrap();
        assert_eq!(std::str::from_utf8(&out).unwrap(), "café");
    }
}
