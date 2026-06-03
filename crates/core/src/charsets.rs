#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

//! Character classification for XML 1.0 names.
//!
//! # ASCII lookup tables
//!
//! Two 256-entry tables let callers classify ASCII bytes with a single array
//! access.  Bytes ≥ 0x80 are always zero in the table; callers must fall back
//! to the Unicode range functions for those.
//!
//! - [`ASCII_XML_NAME`] — XML `Name` (§2.3): `:` counts as a name-start char.
//! - [`ASCII_NCNAME`]   — XPath `NCName`: `:` is a separator, not part of a name.
//!
//! # Unicode range functions
//!
//! Non-ASCII characters are checked with `is_name_start_char` / `is_name_char_unicode`
//! (5th edition, the current default) or `is_name_start_char_4e` / `is_name_char_4e`
//! (4th edition, enabled via `ParseOptions::xml10_fourth_edition`).
//!
//! Both editions agree on all ASCII characters; the tables above handle those
//! uniformly.  The non-ASCII functions cover ≥ 0x80 only.

// ── ASCII lookup tables ───────────────────────────────────────────────────────

/// Byte is an ASCII name-start character (A–Z, a–z, `_`, and for XML Name, `:`).
pub const NS: u8 = 0x01;
/// Byte is an ASCII name character but NOT a name-start (`0`–`9`, `-`, `.`).
pub const NC: u8 = 0x02;

/// XML `Name` lookup table.  Includes `:` as a name-start char (§2.3).
/// Bytes ≥ 0x80 are `0`; use [`is_name_start_char`] / [`is_name_char_unicode`]
/// (or their 4th-edition counterparts) for those.
pub const ASCII_XML_NAME: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = b'A' as usize; while b <= b'Z' as usize { t[b] = NS; b += 1; }
    let mut b = b'a' as usize; while b <= b'z' as usize { t[b] = NS; b += 1; }
    t[b'_' as usize] = NS;
    t[b':' as usize] = NS;
    let mut b = b'0' as usize; while b <= b'9' as usize { t[b] = NC; b += 1; }
    t[b'-' as usize] = NC;
    t[b'.' as usize] = NC;
    t
};

/// Boolean fast-path: 1 iff the byte is an ASCII `NameChar` (`NS|NC`) per
/// XML 1.0 § 2.3 production [4a].  Non-ASCII bytes (≥ 0x80) are `0` so the
/// tight inner loop in `scan_name_raw` exits and falls into the explicit
/// UTF-8 path.  Layout matches `ASCII_XML_NAME` for `NS|NC`.
///
/// Kept as `u8` (not `bool`) so a single `if ASCII_XML_NAME_CHAR[b] != 0`
/// compiles to a cmp-against-zero rather than a load-and-zext.  In a tight
/// 8-wide chunk this lets LLVM vectorize via SIMD compares / pshufb.
pub const ASCII_XML_NAME_CHAR: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = b'A' as usize; while b <= b'Z' as usize { t[b] = 1; b += 1; }
    let mut b = b'a' as usize; while b <= b'z' as usize { t[b] = 1; b += 1; }
    let mut b = b'0' as usize; while b <= b'9' as usize { t[b] = 1; b += 1; }
    t[b'_' as usize] = 1;
    t[b':' as usize] = 1;
    t[b'-' as usize] = 1;
    t[b'.' as usize] = 1;
    t
};

/// XPath `NCName` lookup table.  Excludes `:` (it is a separator in XPath).
/// Bytes ≥ 0x80 are `0`; use [`is_name_start_char`] / [`is_name_char_unicode`]
/// for those.
pub const ASCII_NCNAME: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut b = b'A' as usize; while b <= b'Z' as usize { t[b] = NS; b += 1; }
    let mut b = b'a' as usize; while b <= b'z' as usize { t[b] = NS; b += 1; }
    t[b'_' as usize] = NS;
    // ':' intentionally omitted
    let mut b = b'0' as usize; while b <= b'9' as usize { t[b] = NC; b += 1; }
    t[b'-' as usize] = NC;
    t[b'.' as usize] = NC;
    t
};

// ── 5th-edition non-ASCII ranges (XML 1.0 §2.3, 2008 revision) ───────────────
//
// These functions cover only bytes ≥ 0x80.  ASCII characters are handled by
// the lookup tables above.

/// Returns `true` if `c` (≥ U+0080) is a valid XML 1.0 5th-edition name-start
/// character.  The `:` and `_` cases are ASCII and handled by the tables.
pub fn is_name_start_char(c: char) -> bool {
    matches!(c,
        '\u{C0}'..='\u{D6}'
        | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}'
        | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}'
        | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}'
    )
}

/// Returns `true` if `c` (≥ U+0080) is a valid XML 1.0 5th-edition name
/// character.  ASCII name chars (`0`–`9`, `-`, `.`) are handled by the tables.
pub fn is_name_char_unicode(c: char) -> bool {
    is_name_start_char(c)
        || matches!(c,
            '\u{B7}'
            | '\u{0300}'..='\u{036F}'
            | '\u{203F}'..='\u{2040}'
        )
}

// ── 4th-edition tables (XML 1.0 Appendix B, pre-2008) ────────────────────────
//
// These productions were removed in the 5th edition.  Enabled via
// `ParseOptions::xml10_fourth_edition` (equivalent to libxml2 `XML_PARSE_OLD10`).
//
// Source: https://www.w3.org/TR/2006/REC-xml-20060816/#CharClasses

/// XML 1.0 §2.3 NameStartChar — 4th edition (`Letter | '_' | ':'`).
pub fn is_name_start_char_4e(c: char) -> bool {
    matches!(c, '_' | ':') || is_base_char(c) || is_ideographic(c)
}

/// XML 1.0 §2.3 NameChar — 4th edition.
pub fn is_name_char_4e(c: char) -> bool {
    is_name_start_char_4e(c)
        || is_xml_digit(c)
        || matches!(c, '.' | '-')
        || is_combining_char(c)
        || is_extender(c)
}

fn is_base_char(c: char) -> bool {
    matches!(c,
        '\u{0041}'..='\u{005A}' | '\u{0061}'..='\u{007A}' |
        '\u{00C0}'..='\u{00D6}' | '\u{00D8}'..='\u{00F6}' |
        '\u{00F8}'..='\u{00FF}' |
        '\u{0100}'..='\u{0131}' | '\u{0134}'..='\u{013E}' |
        '\u{0141}'..='\u{0148}' | '\u{014A}'..='\u{017E}' |
        '\u{0180}'..='\u{01C3}' | '\u{01CD}'..='\u{01F0}' |
        '\u{01F4}'..='\u{01F5}' | '\u{01FA}'..='\u{0217}' |
        '\u{0250}'..='\u{02A8}' | '\u{02BB}'..='\u{02C1}' |
        '\u{0386}' |
        '\u{0388}'..='\u{038A}' | '\u{038C}' | '\u{038E}'..='\u{03A1}' |
        '\u{03A3}'..='\u{03CE}' | '\u{03D0}'..='\u{03D6}' |
        '\u{03DA}' | '\u{03DC}' | '\u{03DE}' | '\u{03E0}' |
        '\u{03E2}'..='\u{03F3}' |
        '\u{0401}'..='\u{040C}' | '\u{040E}'..='\u{044F}' |
        '\u{0451}'..='\u{045C}' | '\u{045E}'..='\u{0481}' |
        '\u{0490}'..='\u{04C4}' | '\u{04C7}'..='\u{04C8}' |
        '\u{04CB}'..='\u{04CC}' | '\u{04D0}'..='\u{04EB}' |
        '\u{04EE}'..='\u{04F5}' | '\u{04F8}'..='\u{04F9}' |
        '\u{0531}'..='\u{0556}' | '\u{0559}' |
        '\u{0561}'..='\u{0586}' |
        '\u{05D0}'..='\u{05EA}' | '\u{05F0}'..='\u{05F2}' |
        '\u{0621}'..='\u{063A}' | '\u{0641}'..='\u{064A}' |
        '\u{0671}'..='\u{06B7}' | '\u{06BA}'..='\u{06BE}' |
        '\u{06C0}'..='\u{06CE}' | '\u{06D0}'..='\u{06D3}' | '\u{06D5}' |
        '\u{06E5}'..='\u{06E6}' |
        '\u{0905}'..='\u{0939}' | '\u{093D}' |
        '\u{0958}'..='\u{0961}' |
        '\u{0985}'..='\u{098C}' | '\u{098F}'..='\u{0990}' |
        '\u{0993}'..='\u{09A8}' | '\u{09AA}'..='\u{09B0}' | '\u{09B2}' |
        '\u{09B6}'..='\u{09B9}' | '\u{09DC}'..='\u{09DD}' |
        '\u{09DF}'..='\u{09E1}' | '\u{09F0}'..='\u{09F1}' |
        '\u{0A05}'..='\u{0A0A}' | '\u{0A0F}'..='\u{0A10}' |
        '\u{0A13}'..='\u{0A28}' | '\u{0A2A}'..='\u{0A30}' |
        '\u{0A32}'..='\u{0A33}' | '\u{0A35}'..='\u{0A36}' |
        '\u{0A38}'..='\u{0A39}' |
        '\u{0A59}'..='\u{0A5C}' | '\u{0A5E}' |
        '\u{0A72}'..='\u{0A74}' |
        '\u{0A85}'..='\u{0A8B}' | '\u{0A8D}' |
        '\u{0A8F}'..='\u{0A91}' | '\u{0A93}'..='\u{0AA8}' |
        '\u{0AAA}'..='\u{0AB0}' | '\u{0AB2}'..='\u{0AB3}' |
        '\u{0AB5}'..='\u{0AB9}' | '\u{0ABD}' | '\u{0AE0}' |
        '\u{0B05}'..='\u{0B0C}' | '\u{0B0F}'..='\u{0B10}' |
        '\u{0B13}'..='\u{0B28}' | '\u{0B2A}'..='\u{0B30}' |
        '\u{0B32}'..='\u{0B33}' | '\u{0B36}'..='\u{0B39}' | '\u{0B3D}' |
        '\u{0B5C}'..='\u{0B5D}' | '\u{0B5F}'..='\u{0B61}' |
        '\u{0B85}'..='\u{0B8A}' | '\u{0B8E}'..='\u{0B90}' |
        '\u{0B92}'..='\u{0B95}' | '\u{0B99}'..='\u{0B9A}' | '\u{0B9C}' |
        '\u{0B9E}'..='\u{0B9F}' | '\u{0BA3}'..='\u{0BA4}' |
        '\u{0BA8}'..='\u{0BAA}' | '\u{0BAE}'..='\u{0BB5}' |
        '\u{0BB7}'..='\u{0BB9}' |
        '\u{0C05}'..='\u{0C0C}' | '\u{0C0E}'..='\u{0C10}' |
        '\u{0C12}'..='\u{0C28}' | '\u{0C2A}'..='\u{0C33}' |
        '\u{0C35}'..='\u{0C39}' | '\u{0C60}'..='\u{0C61}' |
        '\u{0C85}'..='\u{0C8C}' | '\u{0C8E}'..='\u{0C90}' |
        '\u{0C92}'..='\u{0CA8}' | '\u{0CAA}'..='\u{0CB3}' |
        '\u{0CB5}'..='\u{0CB9}' | '\u{0CDE}' |
        '\u{0CE0}'..='\u{0CE1}' |
        '\u{0D05}'..='\u{0D0C}' | '\u{0D0E}'..='\u{0D10}' |
        '\u{0D12}'..='\u{0D28}' | '\u{0D2A}'..='\u{0D39}' |
        '\u{0D60}'..='\u{0D61}' |
        '\u{0E01}'..='\u{0E2E}' | '\u{0E30}' |
        '\u{0E32}'..='\u{0E33}' | '\u{0E40}'..='\u{0E45}' |
        '\u{0E81}'..='\u{0E82}' | '\u{0E84}' |
        '\u{0E87}'..='\u{0E88}' | '\u{0E8A}' | '\u{0E8D}' |
        '\u{0E94}'..='\u{0E97}' | '\u{0E99}'..='\u{0E9F}' |
        '\u{0EA1}'..='\u{0EA3}' | '\u{0EA5}' | '\u{0EA7}' |
        '\u{0EAA}'..='\u{0EAB}' | '\u{0EAD}'..='\u{0EAE}' | '\u{0EB0}' |
        '\u{0EB2}'..='\u{0EB3}' | '\u{0EBD}' |
        '\u{0EC0}'..='\u{0EC4}' |
        '\u{0F40}'..='\u{0F47}' | '\u{0F49}'..='\u{0F69}' |
        '\u{10A0}'..='\u{10C5}' | '\u{10D0}'..='\u{10F6}' | '\u{1100}' |
        '\u{1102}'..='\u{1103}' | '\u{1105}'..='\u{1107}' | '\u{1109}' |
        '\u{110B}'..='\u{110C}' | '\u{110E}'..='\u{1112}' |
        '\u{113C}' | '\u{113E}' | '\u{1140}' |
        '\u{114C}' | '\u{114E}' | '\u{1150}' |
        '\u{1154}'..='\u{1155}' | '\u{1159}' |
        '\u{115F}'..='\u{1161}' | '\u{1163}' | '\u{1165}' | '\u{1167}' | '\u{1169}' |
        '\u{116D}'..='\u{116E}' | '\u{1172}'..='\u{1173}' | '\u{1175}' |
        '\u{119E}' | '\u{11A8}' | '\u{11AB}' |
        '\u{11AE}'..='\u{11AF}' | '\u{11B7}'..='\u{11B8}' | '\u{11BA}' |
        '\u{11BC}'..='\u{11C2}' | '\u{11EB}' | '\u{11F0}' | '\u{11F9}' |
        '\u{1E00}'..='\u{1E9B}' | '\u{1EA0}'..='\u{1EF9}' |
        '\u{1F00}'..='\u{1F15}' | '\u{1F18}'..='\u{1F1D}' |
        '\u{1F20}'..='\u{1F45}' | '\u{1F48}'..='\u{1F4D}' |
        '\u{1F50}'..='\u{1F57}' | '\u{1F59}' | '\u{1F5B}' | '\u{1F5D}' |
        '\u{1F5F}'..='\u{1F7D}' | '\u{1F80}'..='\u{1FB4}' |
        '\u{1FB6}'..='\u{1FBC}' | '\u{1FBE}' |
        '\u{1FC2}'..='\u{1FC4}' | '\u{1FC6}'..='\u{1FCC}' |
        '\u{1FD0}'..='\u{1FD3}' | '\u{1FD6}'..='\u{1FDB}' |
        '\u{1FE0}'..='\u{1FEC}' |
        '\u{1FF2}'..='\u{1FF4}' | '\u{1FF6}'..='\u{1FFC}' |
        '\u{2126}' | '\u{212A}'..='\u{212B}' | '\u{212E}' |
        '\u{2180}'..='\u{2182}' |
        '\u{3041}'..='\u{3094}' | '\u{30A1}'..='\u{30FA}' |
        '\u{3105}'..='\u{312C}' |
        '\u{AC00}'..='\u{D7A3}'
    )
}

fn is_ideographic(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FA5}' | '\u{3007}' | '\u{3021}'..='\u{3029}'
    )
}

fn is_combining_char(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{0345}' | '\u{0360}'..='\u{0361}' |
        '\u{0483}'..='\u{0486}' |
        '\u{0591}'..='\u{05A1}' | '\u{05A3}'..='\u{05B9}' |
        '\u{05BB}'..='\u{05BD}' | '\u{05BF}' |
        '\u{05C1}'..='\u{05C2}' | '\u{05C4}' |
        '\u{064B}'..='\u{0652}' | '\u{0670}' |
        '\u{06D6}'..='\u{06DC}' | '\u{06DD}'..='\u{06DF}' |
        '\u{06E0}'..='\u{06E4}' | '\u{06E7}'..='\u{06E8}' |
        '\u{06EA}'..='\u{06ED}' |
        '\u{0901}'..='\u{0903}' | '\u{093C}' |
        '\u{093E}'..='\u{094C}' | '\u{094D}' |
        '\u{0951}'..='\u{0954}' | '\u{0962}'..='\u{0963}' |
        '\u{0981}'..='\u{0983}' | '\u{09BC}' | '\u{09BE}' | '\u{09BF}' |
        '\u{09C0}'..='\u{09C4}' | '\u{09C7}'..='\u{09C8}' |
        '\u{09CB}'..='\u{09CD}' | '\u{09D7}' |
        '\u{09E2}'..='\u{09E3}' |
        '\u{0A02}' | '\u{0A3C}' | '\u{0A3E}' | '\u{0A3F}' |
        '\u{0A40}'..='\u{0A42}' | '\u{0A47}'..='\u{0A48}' |
        '\u{0A4B}'..='\u{0A4D}' | '\u{0A70}'..='\u{0A71}' |
        '\u{0A81}'..='\u{0A83}' | '\u{0ABC}' |
        '\u{0ABE}'..='\u{0AC5}' | '\u{0AC7}'..='\u{0AC9}' |
        '\u{0ACB}'..='\u{0ACD}' |
        '\u{0B01}'..='\u{0B03}' | '\u{0B3C}' |
        '\u{0B3E}'..='\u{0B43}' | '\u{0B47}'..='\u{0B48}' |
        '\u{0B4B}'..='\u{0B4D}' | '\u{0B56}'..='\u{0B57}' |
        '\u{0B82}'..='\u{0B83}' |
        '\u{0BBE}'..='\u{0BC2}' | '\u{0BC6}'..='\u{0BC8}' |
        '\u{0BCA}'..='\u{0BCD}' | '\u{0BD7}' |
        '\u{0C01}'..='\u{0C03}' |
        '\u{0C3E}'..='\u{0C44}' | '\u{0C46}'..='\u{0C48}' |
        '\u{0C4A}'..='\u{0C4D}' | '\u{0C55}'..='\u{0C56}' |
        '\u{0C82}'..='\u{0C83}' |
        '\u{0CBE}'..='\u{0CC4}' | '\u{0CC6}'..='\u{0CC8}' |
        '\u{0CCA}'..='\u{0CCD}' | '\u{0CD5}'..='\u{0CD6}' |
        '\u{0D02}'..='\u{0D03}' |
        '\u{0D3E}'..='\u{0D43}' | '\u{0D46}'..='\u{0D48}' |
        '\u{0D4A}'..='\u{0D4D}' | '\u{0D57}' |
        '\u{0E31}' | '\u{0E34}'..='\u{0E3A}' | '\u{0E47}'..='\u{0E4E}' |
        '\u{0EB1}' | '\u{0EB4}'..='\u{0EB9}' | '\u{0EBB}'..='\u{0EBC}' |
        '\u{0EC8}'..='\u{0ECD}' |
        '\u{0F18}'..='\u{0F19}' | '\u{0F35}' | '\u{0F37}' | '\u{0F39}' |
        '\u{0F3E}' | '\u{0F3F}' |
        '\u{0F71}'..='\u{0F84}' | '\u{0F86}'..='\u{0F8B}' |
        '\u{0F90}'..='\u{0F95}' | '\u{0F97}' |
        '\u{0F99}'..='\u{0FAD}' | '\u{0FB1}'..='\u{0FB7}' | '\u{0FB9}' |
        '\u{20D0}'..='\u{20DC}' | '\u{20E1}' |
        '\u{302A}'..='\u{302F}' | '\u{3099}' | '\u{309A}'
    )
}

fn is_extender(c: char) -> bool {
    matches!(c,
        '\u{00B7}' | '\u{02D0}' | '\u{02D1}' | '\u{0387}' | '\u{0640}' |
        '\u{0E46}' | '\u{0EC6}' | '\u{3005}' |
        '\u{3031}'..='\u{3035}' | '\u{309D}'..='\u{309E}' |
        '\u{30FC}'..='\u{30FE}'
    )
}

fn is_xml_digit(c: char) -> bool {
    matches!(c,
        '\u{0030}'..='\u{0039}' | '\u{0660}'..='\u{0669}' | '\u{06F0}'..='\u{06F9}' |
        '\u{0966}'..='\u{096F}' | '\u{09E6}'..='\u{09EF}' | '\u{0A66}'..='\u{0A6F}' |
        '\u{0AE6}'..='\u{0AEF}' | '\u{0B66}'..='\u{0B6F}' | '\u{0BE7}'..='\u{0BEF}' |
        '\u{0C66}'..='\u{0C6F}' | '\u{0CE6}'..='\u{0CEF}' | '\u{0D66}'..='\u{0D6F}' |
        '\u{0E50}'..='\u{0E59}' | '\u{0EF0}'..='\u{0EF9}' | '\u{0F20}'..='\u{0F29}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Combining marks like U+309A are CombiningChar (not NameStartChar)
    /// in XML 1.0 4th edition, but the simplified 5th-edition rules
    /// allow them via the `U+3001..=U+D7FF` range.  This is the divider
    /// behind the W3C `not-wf-sa-140` test (a 4th-edition test that's
    /// well-formed in 5th-edition rules).
    #[test]
    fn u309a_namestartchar_is_5e_valid_4e_invalid() {
        assert!( is_name_start_char('\u{309A}'),    "5e accepts U+309A");
        assert!(!is_name_start_char_4e('\u{309A}'), "4e rejects U+309A");
        // In both editions it IS a NameChar (combining mark allowed
        // after the first position in 4e; 5e doesn't distinguish).
        assert!(is_name_char_unicode('\u{309A}'));
        assert!(is_name_char_4e('\u{309A}'));
    }

    /// U+00B7 (middle dot) — 4e classifies it as Extender so it's
    /// a NameChar but NOT a NameStartChar; 5e similarly excludes it
    /// from NameStartChar ranges but the 5e NameChar table picks it
    /// up explicitly.  Cross-edition consistency check: never a
    /// NameStartChar.
    #[test]
    fn middle_dot_is_namechar_not_namestart_in_both_editions() {
        assert!(!is_name_start_char('\u{00B7}'));
        assert!(!is_name_start_char_4e('\u{00B7}'));
        // Allowed as a non-leading NameChar in both editions.
        assert!(is_name_char_unicode('\u{00B7}'));
        assert!(is_name_char_4e('\u{00B7}'));
    }

    /// U+0E5C lives in 5e's broad `U+037F..=U+1FFF` NameStartChar
    /// range but is excluded from 4e's BaseChar/Ideographic tables.
    /// This is the exact divider behind W3C `not-wf-sa-141` (a 4e-era
    /// test that's well-formed under 5e rules).
    #[test]
    fn u0e5c_namestartchar_is_5e_valid_4e_invalid() {
        assert!( is_name_start_char('\u{0E5C}'),    "5e accepts U+0E5C");
        assert!(!is_name_start_char_4e('\u{0E5C}'), "4e rejects U+0E5C");
    }

    /// ASCII anchors: letters are NameStartChar in both editions;
    /// digits aren't NameStartChar in either; both can appear after.
    #[test]
    fn ascii_anchors_stable_across_editions() {
        for c in 'A'..='Z' {
            assert!(is_name_start_char_4e(c));
        }
        for c in '0'..='9' {
            assert!(!is_name_start_char_4e(c));
            assert!(is_name_char_4e(c));
        }
    }
}
