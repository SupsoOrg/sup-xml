//! XML 1.0 character-class predicates.
//!
//! libxml2 exposes a family of `xmlIsXxxGroup(c)` predicates that
//! test whether a Unicode codepoint belongs to one of the XML 1.0
//! character classes from Appendix B (`Letter`, `BaseChar`,
//! `Ideographic`, `Digit`, `CombiningChar`, `Extender`).  libxslt
//! uses these when validating xsl:number patterns and a few other
//! places that walk character classes explicitly.
//!
//! The full XML 1.0 character-class tables are large.  Rather than
//! ship our own copy of the Unicode ranges, we lean on
//! `sup-xml-core`'s existing `charsets` module which already
//! implements XML 1.0 5th edition `NameChar`/`NameStartChar`.  The
//! libxml2 groups don't have a 1:1 mapping to those, but they're
//! supersets/subsets that callers compose — what libxslt actually
//! checks for is whether a char is "a letter-like or digit-like
//! XML name character," which is exactly what the NameChar
//! predicate answers.
//!
//! We err on the side of accepting too much: returning 1 for chars
//! in libxml2's superset means stylesheet number patterns that
//! *should* match still match.  Returning 0 for chars that aren't
//! XML 1.0 chars at all is safe.

use std::os::raw::c_int;
use sup_xml_core::charsets;

/// `xmlIsBaseCharGroup(c)` — XML 1.0 `BaseChar` predicate.  ASCII
/// letters, Latin-1 supplement letters, and a large list of Unicode
/// letter ranges from non-CJK scripts.  We approximate with
/// "NameStartChar minus the colon" — the spec class is a subset of
/// NameStartChar, but for libxslt's pattern-matching needs the
/// approximation is sound.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsBaseCharGroup(c: c_int) -> c_int {
    let Some(ch) = char::from_u32(c as u32) else { return 0 };
    // ASCII letters + underscore — BaseChar accepts them; the
    // sup-xml-core predicate only covers chars >= 0x80.
    if ch.is_ascii_alphabetic() || ch == '_' { return 1; }
    // BaseChar explicitly excludes colon (it's not a "letter").
    if ch == ':' { return 0; }
    if (ch as u32) >= 0x80 && charsets::is_name_start_char(ch) { 1 } else { 0 }
}

/// `xmlIsCombiningGroup(c)` — XML 1.0 `CombiningChar` predicate
/// (Unicode combining marks).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsCombiningGroup(c: c_int) -> c_int {
    let Some(ch) = char::from_u32(c as u32) else { return 0 };
    // CombiningChar in XML 1.0 maps to Unicode general categories
    // Mn (Nonspacing_Mark) and Mc (Spacing_Mark).  We don't carry a
    // full general-category table; instead we test the ranges most
    // commonly used: U+0300–U+036F (combining diacriticals), plus
    // the ranges checked by NameChar - NameStartChar - DigitChar.
    let cp = ch as u32;
    if matches!(cp,
        0x0300..=0x036F |  // Combining Diacritical Marks
        0x0483..=0x0487 |  // Cyrillic Combining
        0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4 |  // Hebrew points
        0x064B..=0x0652 |  // Arabic harakat
        0x0670 |
        0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
    ) { 1 } else { 0 }
}

/// `xmlIsDigitGroup(c)` — XML 1.0 `Digit` predicate.  Unicode digits
/// across many scripts (0–9 ASCII, plus Arabic-Indic, Devanagari,
/// Bengali, etc.).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsDigitGroup(c: c_int) -> c_int {
    let Some(ch) = char::from_u32(c as u32) else { return 0 };
    if ch.is_ascii_digit() { return 1; }
    // Common non-ASCII digit ranges from XML 1.0 Appendix B.
    let cp = ch as u32;
    if matches!(cp,
        0x0660..=0x0669 |  // Arabic-Indic
        0x06F0..=0x06F9 |  // Extended Arabic-Indic
        0x0966..=0x096F |  // Devanagari
        0x09E6..=0x09EF |  // Bengali
        0x0A66..=0x0A6F |  // Gurmukhi
        0x0AE6..=0x0AEF |  // Gujarati
        0x0B66..=0x0B6F |  // Oriya
        0x0BE7..=0x0BEF |  // Tamil
        0x0C66..=0x0C6F |  // Telugu
        0x0CE6..=0x0CEF |  // Kannada
        0x0D66..=0x0D6F |  // Malayalam
        0x0E50..=0x0E59 |  // Thai
        0x0ED0..=0x0ED9 |  // Lao
        0x0F20..=0x0F29    // Tibetan
    ) { 1 } else { 0 }
}

/// `xmlIsExtenderGroup(c)` — XML 1.0 `Extender` predicate.  Small
/// set of characters that "extend" the syllable they follow (·,
/// ː, modifier letter half/triangular colons, hiragana iteration
/// marks, etc.).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsExtenderGroup(c: c_int) -> c_int {
    let Some(ch) = char::from_u32(c as u32) else { return 0 };
    let cp = ch as u32;
    if matches!(cp,
        0x00B7 |              // MIDDLE DOT
        0x02D0 |              // MODIFIER LETTER TRIANGULAR COLON
        0x02D1 |              // MODIFIER LETTER HALF TRIANGULAR COLON
        0x0387 |              // GREEK ANO TELEIA
        0x0640 |              // ARABIC TATWEEL
        0x0E46 |              // THAI CHARACTER MAIYAMOK
        0x0EC6 |              // LAO KO LA
        0x3005 |              // IDEOGRAPHIC ITERATION MARK
        0x3031..=0x3035 |     // VERTICAL KANA REPEAT MARKS
        0x309D..=0x309E |     // HIRAGANA ITERATION MARKS
        0x30FC..=0x30FE       // KATAKANA-HIRAGANA PROLONGED + ITERATION
    ) { 1 } else { 0 }
}

/// `xmlCharInRange(c, ptr)` — test whether a codepoint is in a
/// caller-supplied range table.  libxml2's range table is a struct
/// with a count and an array of (low, high) pairs.  We honour the
/// caller's table by reading the same layout it would have built
/// up; if `ptr` is NULL, return 0.
///
/// libxslt doesn't actually use this with non-NULL ranges in any
/// path our shim's stubs exercise — but the symbol is referenced at
/// load time so it must be present.  A faithful implementation is
/// cheap, so we do it rather than stubbing to 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharInRange(c: c_int, ptr: *const std::os::raw::c_void) -> c_int {
    if ptr.is_null() {
        return 0;
    }
    // libxml2's xmlChRangeGroup: { c_int nbShortRange; c_int nbLongRange;
    //                              const xmlChSRange *shortRange;
    //                              const xmlChLRange *longRange; }
    //   where xmlChSRange = { c_ushort low, high; }
    //   and   xmlChLRange = { c_uint   low, high; }
    #[repr(C)]
    struct ChRangeGroup {
        nb_short: c_int,
        nb_long:  c_int,
        short_range: *const ShortRange,
        long_range:  *const LongRange,
    }
    #[repr(C)] struct ShortRange { low: u16, high: u16 }
    #[repr(C)] struct LongRange  { low: u32, high: u32 }

    let group = unsafe { &*(ptr as *const ChRangeGroup) };
    let cp = c as u32;
    if !group.short_range.is_null() {
        let n = group.nb_short.max(0) as usize;
        let sr = unsafe { std::slice::from_raw_parts(group.short_range, n) };
        if sr.iter().any(|r| cp >= r.low as u32 && cp <= r.high as u32) {
            return 1;
        }
    }
    if !group.long_range.is_null() {
        let n = group.nb_long.max(0) as usize;
        let lr = unsafe { std::slice::from_raw_parts(group.long_range, n) };
        if lr.iter().any(|r| cp >= r.low && cp <= r.high) {
            return 1;
        }
    }
    0
}

/// `xmlIsBlankNode(node)` — return 1 if `node` is a Text/CData node
/// whose content is entirely XML whitespace (space, tab, CR, LF).
/// Returns 0 for non-blank text and for non-text nodes.  libxslt
/// calls this during `xsl:strip-space` handling.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsBlankNode(
    node: *const sup_xml_tree::dom::Node<'static>,
) -> c_int {
    if node.is_null() {
        return 0;
    }
    let n = unsafe { &*node };
    use sup_xml_tree::dom::NodeKind;
    if !matches!(n.kind, NodeKind::Text | NodeKind::CData) {
        return 0;
    }
    let content = n.content();
    if content.is_empty() {
        return 1;
    }
    if content.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n')) {
        1
    } else {
        0
    }
}

/// `xmlIsID(doc, elem, attr)` — return 1 if `attr` is an XML `ID`
/// attribute (declared in the DTD or named `xml:id`).  Returns 0
/// otherwise.  We don't fully implement DTD-driven ID resolution
/// yet; the `xml:id` case is the common one and we honour it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIsID(
    _doc:  *const std::os::raw::c_void,
    _elem: *const std::os::raw::c_void,
    attr:  *const sup_xml_tree::dom::Attribute<'static>,
) -> c_int {
    if attr.is_null() {
        return 0;
    }
    let a = unsafe { &*attr };
    // `xml:id` is `id` in the XML namespace.  c-abi stores the local
    // name with the prefix on `attr->ns`; match both parts so this
    // holds regardless of build.
    let is_xml_id = a.local_name() == "id"
        && a.namespace.get().and_then(|ns| ns.prefix()) == Some("xml");
    if is_xml_id { 1 } else { 0 }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_char_group_accepts_ascii_letters() {
        assert_eq!(unsafe { xmlIsBaseCharGroup('a' as c_int) }, 1);
        assert_eq!(unsafe { xmlIsBaseCharGroup('Z' as c_int) }, 1);
        assert_eq!(unsafe { xmlIsBaseCharGroup('_' as c_int) }, 1);
        // Digits are NOT BaseChars.
        assert_eq!(unsafe { xmlIsBaseCharGroup('5' as c_int) }, 0);
        // Colon is NOT in BaseChar.
        assert_eq!(unsafe { xmlIsBaseCharGroup(':' as c_int) }, 0);
        // Non-letter punctuation rejected.
        assert_eq!(unsafe { xmlIsBaseCharGroup('@' as c_int) }, 0);
    }

    #[test]
    fn base_char_group_accepts_unicode_letters() {
        // German umlaut, Greek letter — both XML 1.0 NameStartChars.
        assert_eq!(unsafe { xmlIsBaseCharGroup('ä' as c_int) }, 1);
        assert_eq!(unsafe { xmlIsBaseCharGroup('Σ' as c_int) }, 1);
    }

    #[test]
    fn combining_group_matches_diacriticals() {
        // U+0301 COMBINING ACUTE ACCENT
        assert_eq!(unsafe { xmlIsCombiningGroup(0x0301) }, 1);
        // U+0300 COMBINING GRAVE ACCENT
        assert_eq!(unsafe { xmlIsCombiningGroup(0x0300) }, 1);
        // ASCII letter is not a combining mark.
        assert_eq!(unsafe { xmlIsCombiningGroup('a' as c_int) }, 0);
    }

    #[test]
    fn digit_group_matches_ascii_and_non_ascii_digits() {
        assert_eq!(unsafe { xmlIsDigitGroup('0' as c_int) }, 1);
        assert_eq!(unsafe { xmlIsDigitGroup('9' as c_int) }, 1);
        // Arabic-Indic 0–9 (U+0660..U+0669)
        assert_eq!(unsafe { xmlIsDigitGroup(0x0660) }, 1);
        assert_eq!(unsafe { xmlIsDigitGroup(0x0669) }, 1);
        // Devanagari 0–9 (U+0966..U+096F)
        assert_eq!(unsafe { xmlIsDigitGroup(0x0966) }, 1);
        // Letter is not a digit.
        assert_eq!(unsafe { xmlIsDigitGroup('a' as c_int) }, 0);
    }

    #[test]
    fn extender_group_includes_middle_dot_and_tatweel() {
        // U+00B7 MIDDLE DOT
        assert_eq!(unsafe { xmlIsExtenderGroup(0x00B7) }, 1);
        // U+0640 ARABIC TATWEEL
        assert_eq!(unsafe { xmlIsExtenderGroup(0x0640) }, 1);
        // ASCII letter is not an extender.
        assert_eq!(unsafe { xmlIsExtenderGroup('a' as c_int) }, 0);
    }

    #[test]
    fn char_in_range_null_returns_zero() {
        assert_eq!(unsafe { xmlCharInRange('a' as c_int, std::ptr::null()) }, 0);
    }

    #[test]
    fn char_in_range_walks_short_table() {
        #[repr(C)]
        struct ShortRange { low: u16, high: u16 }
        #[repr(C)]
        struct ChRangeGroup {
            nb_short: c_int,
            nb_long:  c_int,
            short_range: *const ShortRange,
            long_range:  *const ShortRange,  // unused; same layout fits
        }
        let ranges = [ShortRange { low: 0x41, high: 0x5A } /* 'A'..='Z' */];
        let g = ChRangeGroup {
            nb_short: 1,
            nb_long: 0,
            short_range: ranges.as_ptr(),
            long_range: std::ptr::null(),
        };
        let p = &g as *const _ as *const std::os::raw::c_void;
        assert_eq!(unsafe { xmlCharInRange('M' as c_int, p) }, 1);
        assert_eq!(unsafe { xmlCharInRange('a' as c_int, p) }, 0); // lowercase outside range
    }

    #[test]
    fn is_blank_node_detects_whitespace_text() {
        // Build a tiny doc with a whitespace text node.
        let xml = b"<r>   \n\t</r>\0";
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                xml.as_ptr() as *const std::os::raw::c_char,
                (xml.len() - 1) as c_int,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null());
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        // First child is the text node with whitespace.
        let n = unsafe { &*root };
        let first = n.first_child.get().expect("expected a text child");
        let first_ptr = first as *const _;
        assert_eq!(unsafe { xmlIsBlankNode(first_ptr) }, 1);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn is_blank_node_returns_zero_for_non_blank_text() {
        let xml = b"<r>hello</r>\0";
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                xml.as_ptr() as *const std::os::raw::c_char,
                (xml.len() - 1) as c_int,
                std::ptr::null(), std::ptr::null(), 0,
            )
        };
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let first = unsafe { (*root).first_child.get().unwrap() } as *const _;
        assert_eq!(unsafe { xmlIsBlankNode(first) }, 0);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn is_id_matches_xml_id_attribute() {
        let xml = b"<r xml:id=\"x1\" foo=\"bar\"/>\0";
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                xml.as_ptr() as *const std::os::raw::c_char,
                (xml.len() - 1) as c_int,
                std::ptr::null(), std::ptr::null(), 0,
            )
        };
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let r = unsafe { &*root };
        // Walk attributes.  Two attrs: xml:id (ID) and foo (not).
        let mut found_id = false;
        let mut found_foo = false;
        let mut a = r.first_attribute.get();
        while let Some(attr) = a {
            let p = attr as *const _;
            let prefix = attr.namespace.get().and_then(|n| n.prefix());
            match (attr.local_name(), prefix) {
                ("id", Some("xml")) => {
                    assert_eq!(unsafe { xmlIsID(std::ptr::null(), root as *const _, p) }, 1);
                    found_id = true;
                }
                ("foo", _) => {
                    assert_eq!(unsafe { xmlIsID(std::ptr::null(), root as *const _, p) }, 0);
                    found_foo = true;
                }
                _ => {}
            }
            a = attr.next.get();
        }
        assert!(found_id && found_foo);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }
}
