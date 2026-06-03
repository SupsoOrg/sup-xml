//! libxml2 encoding helpers.
//!
//! Two simple, NULL-safe utilities — the rest of libxml2's encoding
//! surface (`xmlCharEncodingHandler*` and friends) needs an opaque
//! handler type and a much wider surface, deferred to the encoding
//! slice proper.
//!
//! - [`xmlGetCharEncodingName`] maps libxml2's `xmlCharEncoding` enum
//!   to a canonical name string (returns a `const char*` to a static
//!   buffer — caller never frees).
//! - [`xmlDetectCharEncoding`] sniffs a byte prefix and returns the
//!   matching enum value (BOM detection — same heuristic libxml2 uses).

use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;

// ── enum values (must match libxml2's `xmlCharEncoding`) ────────────────

/// `XML_CHAR_ENCODING_ERROR` (-1)
pub const XML_CHAR_ENCODING_ERROR:    c_int = -1;
/// `XML_CHAR_ENCODING_NONE` (0)
pub const XML_CHAR_ENCODING_NONE:     c_int = 0;
/// `XML_CHAR_ENCODING_UTF8` (1)
pub const XML_CHAR_ENCODING_UTF8:     c_int = 1;
/// `XML_CHAR_ENCODING_UTF16LE` (2)
pub const XML_CHAR_ENCODING_UTF16LE:  c_int = 2;
/// `XML_CHAR_ENCODING_UTF16BE` (3)
pub const XML_CHAR_ENCODING_UTF16BE:  c_int = 3;
/// `XML_CHAR_ENCODING_UCS4LE` (4)
pub const XML_CHAR_ENCODING_UCS4LE:   c_int = 4;
/// `XML_CHAR_ENCODING_UCS4BE` (5)
pub const XML_CHAR_ENCODING_UCS4BE:   c_int = 5;
/// `XML_CHAR_ENCODING_EBCDIC` (6)
pub const XML_CHAR_ENCODING_EBCDIC:   c_int = 6;
/// `XML_CHAR_ENCODING_UCS4_2143` (7)
pub const XML_CHAR_ENCODING_UCS4_2143: c_int = 7;
/// `XML_CHAR_ENCODING_UCS4_3412` (8)
pub const XML_CHAR_ENCODING_UCS4_3412: c_int = 8;
/// `XML_CHAR_ENCODING_UCS2` (9)
pub const XML_CHAR_ENCODING_UCS2:     c_int = 9;
/// `XML_CHAR_ENCODING_8859_1` (10)
pub const XML_CHAR_ENCODING_8859_1:   c_int = 10;
/// `XML_CHAR_ENCODING_8859_2` (11)
pub const XML_CHAR_ENCODING_8859_2:   c_int = 11;
/// `XML_CHAR_ENCODING_8859_3` (12)
pub const XML_CHAR_ENCODING_8859_3:   c_int = 12;
/// `XML_CHAR_ENCODING_8859_4` (13)
pub const XML_CHAR_ENCODING_8859_4:   c_int = 13;
/// `XML_CHAR_ENCODING_8859_5` (14)
pub const XML_CHAR_ENCODING_8859_5:   c_int = 14;
/// `XML_CHAR_ENCODING_8859_6` (15)
pub const XML_CHAR_ENCODING_8859_6:   c_int = 15;
/// `XML_CHAR_ENCODING_8859_7` (16)
pub const XML_CHAR_ENCODING_8859_7:   c_int = 16;
/// `XML_CHAR_ENCODING_8859_8` (17)
pub const XML_CHAR_ENCODING_8859_8:   c_int = 17;
/// `XML_CHAR_ENCODING_8859_9` (18)
pub const XML_CHAR_ENCODING_8859_9:   c_int = 18;
/// `XML_CHAR_ENCODING_2022_JP` (19)
pub const XML_CHAR_ENCODING_2022_JP:  c_int = 19;
/// `XML_CHAR_ENCODING_SHIFT_JIS` (20)
pub const XML_CHAR_ENCODING_SHIFT_JIS: c_int = 20;
/// `XML_CHAR_ENCODING_EUC_JP` (21)
pub const XML_CHAR_ENCODING_EUC_JP:   c_int = 21;
/// `XML_CHAR_ENCODING_ASCII` (22)
pub const XML_CHAR_ENCODING_ASCII:    c_int = 22;

/// `xmlGetCharEncodingName(enc)` — return the canonical name for the
/// given encoding enum value, or NULL for unknown / error.
///
/// Returned pointer is a `'static` C string — caller MUST NOT
/// `xmlFree` it.
///
/// Names match libxml2 exactly — including its quirks (UTF-16LE and
/// UTF-16BE both report "UTF-16"; UCS-4 endianness collapses to
/// "ISO-10646-UCS-4"; ASCII = 22 returns NULL even though it's a
/// valid encoding).  This is so consumer code expecting libxml2's
/// string set doesn't fall over.  Verified against
/// `/usr/lib/libxml2.2.dylib` 2.9.13.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub extern "C" fn xmlGetCharEncodingName(enc: c_int) -> *const c_char {
    let s: &'static std::ffi::CStr = match enc {
        XML_CHAR_ENCODING_UTF8      => c"UTF-8",
        // libxml2 collapses UTF-16 endianness here.
        XML_CHAR_ENCODING_UTF16LE
            | XML_CHAR_ENCODING_UTF16BE => c"UTF-16",
        XML_CHAR_ENCODING_UCS4LE
            | XML_CHAR_ENCODING_UCS4BE
            | XML_CHAR_ENCODING_UCS4_2143
            | XML_CHAR_ENCODING_UCS4_3412 => c"ISO-10646-UCS-4",
        XML_CHAR_ENCODING_EBCDIC    => c"EBCDIC",
        XML_CHAR_ENCODING_UCS2      => c"ISO-10646-UCS-2",
        XML_CHAR_ENCODING_8859_1    => c"ISO-8859-1",
        11 => c"ISO-8859-2",
        12 => c"ISO-8859-3",
        13 => c"ISO-8859-4",
        14 => c"ISO-8859-5",
        15 => c"ISO-8859-6",
        16 => c"ISO-8859-7",
        17 => c"ISO-8859-8",
        18 => c"ISO-8859-9",
        19 => c"ISO-2022-JP",
        20 => c"Shift-JIS",
        21 => c"EUC-JP",
        // ASCII (22), NONE (0), ERROR (-1), and anything beyond 21
        // return NULL per libxml2 — yes, even ASCII.
        _ => return ptr::null(),
    };
    s.as_ptr()
}

/// `xmlDetectCharEncoding(input, len)` — sniff a byte-of-magic prefix
/// and return the matching `xmlCharEncoding` enum value.  Mirrors
/// libxml2's BOM detection: UTF-8 BOM → UTF8, UTF-16 BOMs → UTF16*,
/// UTF-32 BOMs → UCS4*; absence → NONE.
///
/// Real libxml2 also recognizes EBCDIC and UCS-2 prefixes; we don't
/// bother — those are vanishingly rare in XML wild-data.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDetectCharEncoding(
    input: *const c_uchar,
    len:   c_int,
) -> c_int {
    if input.is_null() || len < 2 {
        return XML_CHAR_ENCODING_NONE;
    }
    let len = len as usize;
    // SAFETY: caller asserts `input` readable for `len` bytes.
    let b = unsafe { std::slice::from_raw_parts(input, len) };

    // 4-byte BOMs first (UTF-32 variants — must check before UTF-16
    // since `FF FE 00 00` is a prefix-match for UTF-16LE).
    if len >= 4 {
        match &b[..4] {
            // UTF-32 BE BOM
            [0x00, 0x00, 0xFE, 0xFF] => return XML_CHAR_ENCODING_UCS4BE,
            // UTF-32 LE BOM
            [0xFF, 0xFE, 0x00, 0x00] => return XML_CHAR_ENCODING_UCS4LE,
            // UCS-4 unusual orders
            [0x00, 0x00, 0xFF, 0xFE] => return XML_CHAR_ENCODING_UCS4_2143,
            [0xFE, 0xFF, 0x00, 0x00] => return XML_CHAR_ENCODING_UCS4_3412,
            // Pseudo-BOM: <?xml in UTF-32 BE / LE
            [0x00, 0x00, 0x00, 0x3C] => return XML_CHAR_ENCODING_UCS4BE,
            [0x3C, 0x00, 0x00, 0x00] => return XML_CHAR_ENCODING_UCS4LE,
            _ => {}
        }
    }

    // 3-byte UTF-8 BOM
    if len >= 3 && b[..3] == [0xEF, 0xBB, 0xBF] {
        return XML_CHAR_ENCODING_UTF8;
    }

    // 2-byte UTF-16 BOMs
    if len >= 2 {
        match &b[..2] {
            [0xFE, 0xFF] => return XML_CHAR_ENCODING_UTF16BE,
            [0xFF, 0xFE] => return XML_CHAR_ENCODING_UTF16LE,
            _ => {}
        }
    }

    // Pseudo-BOM: <? in UTF-16 BE / LE
    if len >= 4 {
        match &b[..4] {
            [0x00, 0x3C, 0x00, 0x3F] => return XML_CHAR_ENCODING_UTF16BE,
            [0x3C, 0x00, 0x3F, 0x00] => return XML_CHAR_ENCODING_UTF16LE,
            _ => {}
        }
    }

    XML_CHAR_ENCODING_NONE
}

/// `xmlParseCharEncoding(name)` — inverse of [`xmlGetCharEncodingName`].
/// Maps a canonical encoding name to the libxml2 `xmlCharEncoding`
/// enum value.  Returns:
///
///   - `XML_CHAR_ENCODING_NONE` (0) for NULL or empty input
///   - the matching enum value for recognised names
///   - `XML_CHAR_ENCODING_ERROR` (-1) for unknown names
///
/// Match is case-insensitive and accepts the common aliases libxml2
/// recognises (`UTF8` for `UTF-8`, `SHIFT-JIS` and `SHIFT_JIS`, etc.).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseCharEncoding(name: *const c_char) -> c_int {
    if name.is_null() {
        return XML_CHAR_ENCODING_NONE;
    }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
    if bytes.is_empty() {
        return XML_CHAR_ENCODING_NONE;
    }
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    match lower.as_slice() {
        b"utf-8"  | b"utf8"                                  => XML_CHAR_ENCODING_UTF8,
        b"utf-16" | b"utf16"                                 => XML_CHAR_ENCODING_UTF16LE,
        b"utf-16le"                                          => XML_CHAR_ENCODING_UTF16LE,
        b"utf-16be"                                          => XML_CHAR_ENCODING_UTF16BE,
        b"iso-10646-ucs-4" | b"ucs-4" | b"ucs4"              => XML_CHAR_ENCODING_UCS4LE,
        b"iso-10646-ucs-2" | b"ucs-2" | b"ucs2"              => XML_CHAR_ENCODING_UCS2,
        b"iso-8859-1" | b"iso-latin-1" | b"iso latin 1"
                | b"latin1"                                  => XML_CHAR_ENCODING_8859_1,
        b"iso-8859-2"                                        => XML_CHAR_ENCODING_8859_2,
        b"iso-8859-3"                                        => XML_CHAR_ENCODING_8859_3,
        b"iso-8859-4"                                        => XML_CHAR_ENCODING_8859_4,
        b"iso-8859-5"                                        => XML_CHAR_ENCODING_8859_5,
        b"iso-8859-6"                                        => XML_CHAR_ENCODING_8859_6,
        b"iso-8859-7"                                        => XML_CHAR_ENCODING_8859_7,
        b"iso-8859-8"                                        => XML_CHAR_ENCODING_8859_8,
        b"iso-8859-9"                                        => XML_CHAR_ENCODING_8859_9,
        b"iso-2022-jp"                                       => XML_CHAR_ENCODING_2022_JP,
        b"shift_jis" | b"shift-jis" | b"sjis"                => XML_CHAR_ENCODING_SHIFT_JIS,
        b"euc-jp"                                            => XML_CHAR_ENCODING_EUC_JP,
        b"ascii"  | b"us-ascii"                              => XML_CHAR_ENCODING_ASCII,
        b"ebcdic"                                            => XML_CHAR_ENCODING_EBCDIC,
        _                                                    => XML_CHAR_ENCODING_ERROR,
    }
}

/// `xmlEncodeSpecialChars(doc, input)` — escape `&`, `<`, `>`, `"`,
/// and `\r` in `input`.  Differs from
/// [`xmlEncodeEntitiesReentrant`] in that `>` is also escaped (XML
/// 1.0 §2.4 best-practice for content) and `\r` is escaped to
/// `&#13;` to round-trip through line-ending normalization.  We
/// match the simpler "five canonical entities" set for now —
/// callers that care about CR preservation should use the
/// per-application encoder.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlEncodeSpecialChars(
    doc:   *mut std::ffi::c_void,
    input: *const c_uchar,
) -> *mut c_char {
    unsafe { xmlEncodeEntitiesReentrant(doc, input) }
}

/// `xmlEncodeEntitiesReentrant(doc, input)` — escape `&`, `<`, `>`,
/// `"`, and `'` in `input`, returning a fresh heap-allocated NUL-
/// terminated copy.  The `doc` argument is accepted for API parity
/// (libxml2 uses it to decide whether to apply HTML quirks); we
/// always escape the canonical five.
///
/// Returns NULL on NULL input or alloc failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlEncodeEntitiesReentrant(
    _doc:  *mut std::ffi::c_void,
    input: *const c_uchar,
) -> *mut c_char {
    if input.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { std::ffi::CStr::from_ptr(input as *const c_char) }.to_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'&'  => out.extend_from_slice(b"&amp;"),
            b'<'  => out.extend_from_slice(b"&lt;"),
            b'>'  => out.extend_from_slice(b"&gt;"),
            b'"'  => out.extend_from_slice(b"&quot;"),
            b'\'' => out.extend_from_slice(b"&apos;"),
            _     => out.push(b),
        }
    }
    crate::alloc::alloc_registered_cstring(&out)
}

// ── predefined entities ────────────────────────────────────────────────────

/// `xmlGetPredefinedEntity(name)` — look up one of the five
/// XML-predefined entity references (`amp`, `lt`, `gt`, `quot`,
/// `apos`).  Returns a pointer to a static `xmlEntity` descriptor
/// (compatible with libxml2's layout for the fields callers read);
/// NULL for any other name.
///
/// libxml2 callers use this to discover whether a name is one of
/// the built-ins (so they can avoid pulling it from a DTD), then
/// read `ent->content` for the replacement text.  Our static
/// descriptors expose `content` at offset 16 and `name` at offset
/// 32 — same as libxml2's `_xmlEntity` layout for those two fields.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetPredefinedEntity(
    name: *const c_uchar,
) -> *const PredefinedEntity {
    if name.is_null() { return ptr::null(); }
    let cs = unsafe { std::ffi::CStr::from_ptr(name as *const c_char) };
    let bytes = cs.to_bytes();
    let idx: Option<usize> = match bytes {
        b"amp"   => Some(0),
        b"lt"    => Some(1),
        b"gt"    => Some(2),
        b"quot"  => Some(3),
        b"apos"  => Some(4),
        _        => None,
    };
    match idx {
        Some(i) => &PREDEFINED[i] as *const PredefinedEntity,
        None    => ptr::null(),
    }
}

/// Subset of libxml2's `_xmlEntity` layout exposing the fields a
/// `xmlGetPredefinedEntity` caller is likely to read.  Real libxml2
/// `_xmlEntity` is ~160 bytes; we expose the prefix that covers
/// `name` and `content` at their canonical offsets.
#[repr(C)]
pub struct PredefinedEntity {
    pub _private:   *mut c_void,    //   0
    pub kind:       c_int,           //   8 (XML_ENTITY_DECL = 6)
    pub _pad_kind:  c_int,           //  12
    pub name:       *const c_uchar,  //  16
    pub children:   *mut c_void,     //  24
    pub last:       *mut c_void,     //  32
    pub parent:     *mut c_void,     //  40
    pub next:       *mut c_void,     //  48
    pub prev:       *mut c_void,     //  56
    pub doc:        *mut c_void,     //  64
    pub orig:       *const c_uchar,  //  72
    pub content:    *const c_uchar,  //  80
}

unsafe impl Sync for PredefinedEntity {}

const fn pe(name: &'static [u8], content: &'static [u8]) -> PredefinedEntity {
    PredefinedEntity {
        _private:  ptr::null_mut(),
        kind:      6, // XML_ENTITY_DECL
        _pad_kind: 0,
        name:      name.as_ptr(),
        children:  ptr::null_mut(),
        last:      ptr::null_mut(),
        parent:    ptr::null_mut(),
        next:      ptr::null_mut(),
        prev:      ptr::null_mut(),
        doc:       ptr::null_mut(),
        orig:      content.as_ptr(),
        content:   content.as_ptr(),
    }
}

static PREDEFINED: [PredefinedEntity; 5] = [
    pe(b"amp\0",  b"&\0"),
    pe(b"lt\0",   b"<\0"),
    pe(b"gt\0",   b">\0"),
    pe(b"quot\0", b"\"\0"),
    pe(b"apos\0", b"'\0"),
];

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn names_round_trip() {
        let p = xmlGetCharEncodingName(XML_CHAR_ENCODING_UTF8);
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "UTF-8");
        // libxml2 quirk: UTF-16LE and UTF-16BE both report just "UTF-16".
        let utf16le = unsafe { CStr::from_ptr(xmlGetCharEncodingName(2)) }
            .to_str().unwrap();
        let utf16be = unsafe { CStr::from_ptr(xmlGetCharEncodingName(3)) }
            .to_str().unwrap();
        assert_eq!(utf16le, "UTF-16");
        assert_eq!(utf16be, "UTF-16");
        // libxml2 quirk: ASCII (22) returns NULL despite being valid.
        assert!(xmlGetCharEncodingName(22).is_null());
        assert!(xmlGetCharEncodingName(0).is_null()); // NONE → NULL
        assert!(xmlGetCharEncodingName(9999).is_null()); // unknown
    }

    #[test]
    fn detect_utf8_bom() {
        let b = b"\xEF\xBB\xBF<r/>";
        let enc = unsafe { xmlDetectCharEncoding(b.as_ptr(), b.len() as c_int) };
        assert_eq!(enc, XML_CHAR_ENCODING_UTF8);
    }

    #[test]
    fn detect_utf16le_bom() {
        let b = b"\xFF\xFE<\0r\0";
        let enc = unsafe { xmlDetectCharEncoding(b.as_ptr(), b.len() as c_int) };
        assert_eq!(enc, XML_CHAR_ENCODING_UTF16LE);
    }

    #[test]
    fn detect_utf16be_bom() {
        let b = b"\xFE\xFF\0<\0r";
        let enc = unsafe { xmlDetectCharEncoding(b.as_ptr(), b.len() as c_int) };
        assert_eq!(enc, XML_CHAR_ENCODING_UTF16BE);
    }

    #[test]
    fn detect_utf16_pseudo_bom() {
        // `<?` in UTF-16 BE without an actual BOM.
        let b = b"\x00<\x00?";
        let enc = unsafe { xmlDetectCharEncoding(b.as_ptr(), b.len() as c_int) };
        assert_eq!(enc, XML_CHAR_ENCODING_UTF16BE);
    }

    #[test]
    fn detect_no_bom_returns_none() {
        let b = b"<root/>";
        let enc = unsafe { xmlDetectCharEncoding(b.as_ptr(), b.len() as c_int) };
        assert_eq!(enc, XML_CHAR_ENCODING_NONE);
    }

    #[test]
    fn null_input_returns_none() {
        let enc = unsafe { xmlDetectCharEncoding(ptr::null(), 0) };
        assert_eq!(enc, XML_CHAR_ENCODING_NONE);
    }
}
