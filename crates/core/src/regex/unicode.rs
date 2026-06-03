//! Unicode category and block tables for XSD §F.1 `\p{...}` escapes.
//!
//! General categories come from the `unicode-properties` crate.
//! Block ranges come from XSD §F.1.1 and are hand-encoded below —
//! the list is short and stable, and the spec pins the names so a
//! drift between Unicode revisions isn't visible to schema authors.
//!
//! Categories are materialised into [`ClassSet`]s lazily on first
//! reference and cached in a `OnceLock` so repeated `\p{L}` matches
//! pay no per-match cost.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

use super::class::ClassSet;
use super::ucd::{self, UnicodeVersion};

thread_local! {
    /// Which Unicode snapshot to consult while compiling the
    /// `\p{...}` properties of the next pattern.  Stays at
    /// `Latest` (the build-time UCD shipped with
    /// `unicode-properties`) for production callers; the conformance
    /// runner pushes `V6_0` / `V9_0` around the version-locked
    /// W3C test sets via [`with_unicode_version`].  Read by
    /// [`property_set`] at pattern-compile time so the resulting
    /// NFA's `ClassSet`s bake in the right Unicode boundaries.
    static REGEX_UCD_VERSION: Cell<UnicodeVersion> =
        const { Cell::new(UnicodeVersion::Latest) };
}

/// Run `f` with the regex engine's bundled Unicode snapshot
/// temporarily set to `version`.  Restores the previous value on
/// return.  Only affects pattern *compilation*; once compiled, an
/// NFA's classes are baked in.
pub fn with_unicode_version<R>(version: UnicodeVersion, f: impl FnOnce() -> R) -> R {
    let prev = REGEX_UCD_VERSION.with(|c| c.replace(version));
    let r = f();
    REGEX_UCD_VERSION.with(|c| c.set(prev));
    r
}

/// Snapshot the active Unicode version for the regex engine's
/// compile-time `\p{...}` resolution.  Read by the module-level
/// compile-cache when building the cache key so a pattern cached
/// under one UCD snapshot isn't returned to a caller asking under
/// another.
pub(super) fn current_ucd_version() -> UnicodeVersion {
    REGEX_UCD_VERSION.with(|c| c.get())
}

/// Look up the [`ClassSet`] for an XSD `\p{...}` body.  Returns
/// `None` for unknown names — callers turn that into a compile-time
/// error citing the offending escape.
pub fn property_set(name: &str) -> Option<&'static ClassSet> {
    if let Some(block_name) = name.strip_prefix("Is") {
        return block_set(block_name);
    }
    match REGEX_UCD_VERSION.with(|c| c.get()) {
        UnicodeVersion::Latest => category_set(name),
        v                      => category_set_versioned(name, v),
    }
}

/// Look up a General_Category property against a bundled historical
/// UCD snapshot (Unicode 6.0 or 9.0).  Builds the [`ClassSet`] from
/// the snapshot's range table on first reference and caches it so
/// repeated `\p{Lu}` queries against the same version stay cheap.
fn category_set_versioned(
    name: &str, version: UnicodeVersion,
) -> Option<&'static ClassSet> {
    static CACHE: OnceLock<Mutex<HashMap<(UnicodeVersion, String), &'static ClassSet>>>
        = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let g = cache.lock().unwrap();
        if let Some(&cs) = g.get(&(version, name.to_string())) {
            return Some(cs);
        }
    }
    let built = build_versioned_category(name, version)?;
    let leaked: &'static ClassSet = Box::leak(Box::new(built));
    cache.lock().unwrap().insert((version, name.to_string()), leaked);
    Some(leaked)
}

/// Compose a `ClassSet` for `name` from the bundled UCD snapshot.
/// Subcategories (`Lu`, `Nd`, …) come straight from the table;
/// the seven group letters (`L`, `M`, `N`, `P`, `S`, `Z`, `C`)
/// are the union of their member subcategories.
fn build_versioned_category(
    name: &str, version: UnicodeVersion,
) -> Option<ClassSet> {
    if let Some(ranges) = ucd::category(name, version) {
        let pairs: Vec<(u32, u32)> = ranges.iter()
            .map(|r| (r.start, r.end))
            .collect();
        return Some(ClassSet::from_sorted_ranges(pairs));
    }
    let members: &[&str] = match name {
        "L"  => &["Lu", "Ll", "Lt", "Lm", "Lo"],
        "M"  => &["Mn", "Mc", "Me"],
        "N"  => &["Nd", "Nl", "No"],
        "P"  => &["Pc", "Pd", "Ps", "Pe", "Pi", "Pf", "Po"],
        "S"  => &["Sm", "Sc", "Sk", "So"],
        "Z"  => &["Zs", "Zl", "Zp"],
        "C"  => &["Cc", "Cf", "Cs", "Co", "Cn"],
        // XSD §F.1.1 also accepts `LC` (Cased Letter = Lu ∪ Ll ∪ Lt).
        "LC" => &["Lu", "Ll", "Lt"],
        _    => return None,
    };
    let mut acc = ClassSet::empty();
    for m in members {
        if let Some(ranges) = ucd::category(m, version) {
            let pairs: Vec<(u32, u32)> = ranges.iter()
                .map(|r| (r.start, r.end))
                .collect();
            acc = acc.union(&ClassSet::from_sorted_ranges(pairs));
        }
    }
    Some(acc)
}

// ── categories ─────────────────────────────────────────────────────────────

/// XSD §F.1.1 — `\p{L}`, `\p{Lu}`, …; both group abbreviations
/// (`L`, `M`, `N`, …) and full subcategories (`Lu`, `Ll`, …).
fn category_set(name: &str) -> Option<&'static ClassSet> {
    macro_rules! cached_cat {
        ($name:literal, $build:expr) => {{
            static CELL: OnceLock<ClassSet> = OnceLock::new();
            if name == $name {
                return Some(CELL.get_or_init(|| $build));
            }
        }};
    }

    // Group categories — defined as the union of their members.
    cached_cat!("L", build_group(GeneralCategoryGroup::Letter));
    cached_cat!("M", build_group(GeneralCategoryGroup::Mark));
    cached_cat!("N", build_group(GeneralCategoryGroup::Number));
    cached_cat!("P", build_group(GeneralCategoryGroup::Punctuation));
    cached_cat!("S", build_group(GeneralCategoryGroup::Symbol));
    cached_cat!("Z", build_group(GeneralCategoryGroup::Separator));
    cached_cat!("C", build_group(GeneralCategoryGroup::Other));

    // Subcategories — one cell each, built by predicate over the
    // full Unicode space on first reference.  The build is ~1 ms.
    use unicode_properties::GeneralCategory as G;
    cached_cat!("Lu", build_filtered(|c| c.general_category() == G::UppercaseLetter));
    cached_cat!("Ll", build_filtered(|c| c.general_category() == G::LowercaseLetter));
    cached_cat!("Lt", build_filtered(|c| c.general_category() == G::TitlecaseLetter));
    cached_cat!("Lm", build_filtered(|c| c.general_category() == G::ModifierLetter));
    cached_cat!("Lo", build_filtered(|c| c.general_category() == G::OtherLetter));

    cached_cat!("Mn", build_filtered(|c| c.general_category() == G::NonspacingMark));
    cached_cat!("Mc", build_filtered(|c| c.general_category() == G::SpacingMark));
    cached_cat!("Me", build_filtered(|c| c.general_category() == G::EnclosingMark));

    cached_cat!("Nd", build_filtered(|c| c.general_category() == G::DecimalNumber));
    cached_cat!("Nl", build_filtered(|c| c.general_category() == G::LetterNumber));
    cached_cat!("No", build_filtered(|c| c.general_category() == G::OtherNumber));

    cached_cat!("Pc", build_filtered(|c| c.general_category() == G::ConnectorPunctuation));
    cached_cat!("Pd", build_filtered(|c| c.general_category() == G::DashPunctuation));
    cached_cat!("Ps", build_filtered(|c| c.general_category() == G::OpenPunctuation));
    cached_cat!("Pe", build_filtered(|c| c.general_category() == G::ClosePunctuation));
    cached_cat!("Pi", build_filtered(|c| c.general_category() == G::InitialPunctuation));
    cached_cat!("Pf", build_filtered(|c| c.general_category() == G::FinalPunctuation));
    cached_cat!("Po", build_filtered(|c| c.general_category() == G::OtherPunctuation));

    cached_cat!("Sm", build_filtered(|c| c.general_category() == G::MathSymbol));
    cached_cat!("Sc", build_filtered(|c| c.general_category() == G::CurrencySymbol));
    cached_cat!("Sk", build_filtered(|c| c.general_category() == G::ModifierSymbol));
    cached_cat!("So", build_filtered(|c| c.general_category() == G::OtherSymbol));

    cached_cat!("Zs", build_filtered(|c| c.general_category() == G::SpaceSeparator));
    cached_cat!("Zl", build_filtered(|c| c.general_category() == G::LineSeparator));
    cached_cat!("Zp", build_filtered(|c| c.general_category() == G::ParagraphSeparator));

    cached_cat!("Cc", build_filtered(|c| c.general_category() == G::Control));
    cached_cat!("Cf", build_filtered(|c| c.general_category() == G::Format));
    cached_cat!("Cs", build_filtered(|c| c.general_category() == G::Surrogate));
    cached_cat!("Co", build_filtered(|c| c.general_category() == G::PrivateUse));
    cached_cat!("Cn", build_filtered(|c| c.general_category() == G::Unassigned));

    None
}

fn build_group(group: GeneralCategoryGroup) -> ClassSet {
    build_filtered(move |c| c.general_category_group() == group)
}

/// Walk the Unicode scalar space, collecting all codepoints
/// satisfying `f` into a sorted range list.  Runs once per category
/// per process.
fn build_filtered(f: impl Fn(char) -> bool) -> ClassSet {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    let mut cur: Option<(u32, u32)> = None;
    for cp in 0u32..=0x10_FFFF {
        let Some(c) = char::from_u32(cp) else { continue };
        if f(c) {
            match &mut cur {
                Some((_, hi)) if *hi + 1 == cp => *hi = cp,
                _ => {
                    if let Some(r) = cur.take() { ranges.push(r); }
                    cur = Some((cp, cp));
                }
            }
        } else if let Some(r) = cur.take() {
            ranges.push(r);
        }
    }
    if let Some(r) = cur { ranges.push(r); }
    ClassSet::from_sorted_ranges(ranges)
}

// ── XSD `\s` and `\w` ─────────────────────────────────────────────────────

/// `\s` per XSD §F.1.4 — the four XML whitespace characters.  Not
/// the Unicode whitespace set (`Zs ∪ Zl ∪ Zp`); the spec is
/// explicit.
pub fn xsd_whitespace() -> &'static ClassSet {
    static CELL: OnceLock<ClassSet> = OnceLock::new();
    CELL.get_or_init(|| ClassSet::from_sorted_ranges(vec![
        (0x09, 0x0A),  // tab, LF
        (0x0D, 0x0D),  // CR
        (0x20, 0x20),  // space
    ]))
}

/// `\w` per XSD §F.1.4 — the universe minus `\p{P} ∪ \p{Z} ∪ \p{C}`.
/// Reads [`REGEX_UCD_VERSION`] so the `P` / `Z` / `C` categories
/// come from the right Unicode snapshot when a version-locked test
/// is active; falls back to the latest UCD otherwise.
pub fn xsd_word() -> &'static ClassSet {
    let v = REGEX_UCD_VERSION.with(|c| c.get());
    match v {
        UnicodeVersion::Latest => {
            static CELL: OnceLock<ClassSet> = OnceLock::new();
            CELL.get_or_init(|| {
                let p = category_set("P").expect("P category");
                let z = category_set("Z").expect("Z category");
                let c = category_set("C").expect("C category");
                ClassSet::universe().subtract(p).subtract(z).subtract(c)
            })
        }
        _ => versioned_word(v),
    }
}

/// Per-version cache for `\w`.  Built from the bundled UCD
/// snapshot's `P` / `Z` / `C` ranges on first reference.
fn versioned_word(version: UnicodeVersion) -> &'static ClassSet {
    static CACHE: OnceLock<Mutex<HashMap<UnicodeVersion, &'static ClassSet>>>
        = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&cs) = cache.lock().unwrap().get(&version) {
        return cs;
    }
    let p = category_set_versioned("P", version).expect("versioned P");
    let z = category_set_versioned("Z", version).expect("versioned Z");
    let c = category_set_versioned("C", version).expect("versioned C");
    let cs = ClassSet::universe().subtract(p).subtract(z).subtract(c);
    let leaked: &'static ClassSet = Box::leak(Box::new(cs));
    cache.lock().unwrap().insert(version, leaked);
    leaked
}

/// `\d` per XSD §F.1.4 — equivalent to `\p{Nd}`.  Honours the
/// current [`REGEX_UCD_VERSION`] so a version-locked test runs
/// against the right Unicode snapshot.
pub fn xsd_digit() -> &'static ClassSet {
    match REGEX_UCD_VERSION.with(|c| c.get()) {
        UnicodeVersion::Latest => category_set("Nd").expect("Nd category"),
        v                      => category_set_versioned("Nd", v).expect("versioned Nd"),
    }
}

// ── blocks ─────────────────────────────────────────────────────────────────

/// XSD §F.1.1 named Unicode block — `\p{IsBasicLatin}` and friends.
/// Names are stripped of their leading `Is` by the caller.
fn block_set(name: &str) -> Option<&'static ClassSet> {
    let ranges = block_ranges(name)?;
    static MAP: OnceLock<std::sync::Mutex<rustc_hash::FxHashMap<&'static str, &'static ClassSet>>>
        = OnceLock::new();
    let map = MAP.get_or_init(Default::default);
    let mut guard = map.lock().unwrap();
    if let Some(&cs) = guard.get(name) {
        return Some(cs);
    }
    let cs: &'static ClassSet =
        Box::leak(Box::new(ClassSet::from_sorted_ranges(ranges)));
    let canonical = block_name_static(name)?;
    guard.insert(canonical, cs);
    Some(cs)
}

/// All ranges for a named Unicode block per XSD §F.1.1.  Most
/// blocks are a single contiguous range; a few (PrivateUse,
/// HighSurrogates) cover multiple disjoint code-point ranges.
fn block_ranges(name: &str) -> Option<Vec<(u32, u32)>> {
    // XSD §F.1.1: PrivateUse spans the BMP private-use area and
    // both supplementary private-use planes.  Implement as a
    // multi-range union so `\p{IsPrivateUse}` matches every PUA
    // codepoint (the conformance suite exercises Plane-15 / 16).
    if name == "PrivateUse" || name == "PrivateUseArea" {
        return Some(vec![
            (0xE000,   0xF8FF),
            (0xF0000,  0xFFFFD),
            (0x100000, 0x10FFFD),
        ]);
    }
    block_table().iter()
        .find(|(n, _, _)| *n == name)
        .map(|&(_, lo, hi)| vec![(lo, hi)])
}

fn block_name_static(name: &str) -> Option<&'static str> {
    block_table().iter()
        .find(|(n, _, _)| *n == name)
        .map(|&(n, _, _)| n)
}

/// `(name, lo, hi)` triples for XSD §F.1.1's block list.  Codepoint
/// ranges from Unicode `Blocks.txt`; names match the XSD spec
/// verbatim (note: `IsLatin-1Supplement` is written without a
/// hyphen in XSD's table, but real-world schemas use both — we
/// accept both forms).
fn block_table() -> &'static [(&'static str, u32, u32)] {
    // Kept short on purpose — the full XSD §F.1.1 table has ~150
    // entries and most are vanishingly rare in schemas.  Add
    // entries here as users hit `\p{IsX}` names we don't yet
    // recognise; the engine errors at compile time so missing
    // names surface immediately.
    // Full XSD §F.1.1 / Unicode 3.1 block list.  Names match the
    // XSD spec verbatim; some have alias spellings (with/without
    // hyphens) — both included so real-world schemas using either
    // form resolve.
    &[
        ("BasicLatin",                              0x0000, 0x007F),
        ("Latin-1Supplement",                       0x0080, 0x00FF),
        ("Latin1Supplement",                        0x0080, 0x00FF),
        ("LatinExtended-A",                         0x0100, 0x017F),
        ("LatinExtended-B",                         0x0180, 0x024F),
        ("IPAExtensions",                           0x0250, 0x02AF),
        ("SpacingModifierLetters",                  0x02B0, 0x02FF),
        ("CombiningDiacriticalMarks",               0x0300, 0x036F),
        ("Greek",                                   0x0370, 0x03FF),
        // Unicode 4.1+ renamed the block to "Greek and Coptic" while
        // splitting out a separate Coptic block; XSD §F.1.1 keeps the
        // legacy name, but many real-world schemas and the W3C
        // conformance suite use the modern alias.
        ("GreekandCoptic",                          0x0370, 0x03FF),
        ("Cyrillic",                                0x0400, 0x04FF),
        ("Armenian",                                0x0530, 0x058F),
        ("Hebrew",                                  0x0590, 0x05FF),
        ("Arabic",                                  0x0600, 0x06FF),
        ("Syriac",                                  0x0700, 0x074F),
        ("Thaana",                                  0x0780, 0x07BF),
        ("Devanagari",                              0x0900, 0x097F),
        ("Bengali",                                 0x0980, 0x09FF),
        ("Gurmukhi",                                0x0A00, 0x0A7F),
        ("Gujarati",                                0x0A80, 0x0AFF),
        ("Oriya",                                   0x0B00, 0x0B7F),
        ("Tamil",                                   0x0B80, 0x0BFF),
        ("Telugu",                                  0x0C00, 0x0C7F),
        ("Kannada",                                 0x0C80, 0x0CFF),
        ("Malayalam",                               0x0D00, 0x0D7F),
        ("Sinhala",                                 0x0D80, 0x0DFF),
        ("Thai",                                    0x0E00, 0x0E7F),
        ("Lao",                                     0x0E80, 0x0EFF),
        ("Tibetan",                                 0x0F00, 0x0FFF),
        ("Myanmar",                                 0x1000, 0x109F),
        ("Georgian",                                0x10A0, 0x10FF),
        ("HangulJamo",                              0x1100, 0x11FF),
        ("Ethiopic",                                0x1200, 0x137F),
        ("Cherokee",                                0x13A0, 0x13FF),
        ("UnifiedCanadianAboriginalSyllabics",      0x1400, 0x167F),
        ("Ogham",                                   0x1680, 0x169F),
        ("Runic",                                   0x16A0, 0x16FF),
        ("Khmer",                                   0x1780, 0x17FF),
        ("Mongolian",                               0x1800, 0x18AF),
        ("LatinExtendedAdditional",                 0x1E00, 0x1EFF),
        ("GreekExtended",                           0x1F00, 0x1FFF),
        ("GeneralPunctuation",                      0x2000, 0x206F),
        ("SuperscriptsandSubscripts",               0x2070, 0x209F),
        ("CurrencySymbols",                         0x20A0, 0x20CF),
        ("CombiningMarksforSymbols",                0x20D0, 0x20FF),
        // Unicode 4.1+ alias.
        ("CombiningDiacriticalMarksforSymbols",     0x20D0, 0x20FF),
        ("LetterlikeSymbols",                       0x2100, 0x214F),
        ("NumberForms",                             0x2150, 0x218F),
        ("Arrows",                                  0x2190, 0x21FF),
        ("MathematicalOperators",                   0x2200, 0x22FF),
        ("MiscellaneousTechnical",                  0x2300, 0x23FF),
        ("ControlPictures",                         0x2400, 0x243F),
        ("OpticalCharacterRecognition",             0x2440, 0x245F),
        ("EnclosedAlphanumerics",                   0x2460, 0x24FF),
        ("BoxDrawing",                              0x2500, 0x257F),
        ("BlockElements",                           0x2580, 0x259F),
        ("GeometricShapes",                         0x25A0, 0x25FF),
        ("MiscellaneousSymbols",                    0x2600, 0x26FF),
        ("Dingbats",                                0x2700, 0x27BF),
        ("BraillePatterns",                         0x2800, 0x28FF),
        ("CJKRadicalsSupplement",                   0x2E80, 0x2EFF),
        ("KangxiRadicals",                          0x2F00, 0x2FDF),
        ("IdeographicDescriptionCharacters",        0x2FF0, 0x2FFF),
        ("CJKSymbolsandPunctuation",                0x3000, 0x303F),
        ("Hiragana",                                0x3040, 0x309F),
        ("Katakana",                                0x30A0, 0x30FF),
        ("Bopomofo",                                0x3100, 0x312F),
        ("HangulCompatibilityJamo",                 0x3130, 0x318F),
        ("Kanbun",                                  0x3190, 0x319F),
        ("BopomofoExtended",                        0x31A0, 0x31BF),
        ("EnclosedCJKLettersandMonths",             0x3200, 0x32FF),
        ("CJKCompatibility",                        0x3300, 0x33FF),
        ("CJKUnifiedIdeographsExtensionA",          0x3400, 0x4DB5),
        ("CJKUnifiedIdeographs",                    0x4E00, 0x9FFF),
        ("YiSyllables",                             0xA000, 0xA48F),
        ("YiRadicals",                              0xA490, 0xA4CF),
        ("HangulSyllables",                         0xAC00, 0xD7AF),
        ("HighSurrogates",                          0xD800, 0xDB7F),
        ("HighPrivateUseSurrogates",                0xDB80, 0xDBFF),
        ("LowSurrogates",                           0xDC00, 0xDFFF),
        ("PrivateUse",                              0xE000, 0xF8FF),
        ("PrivateUseArea",                          0xE000, 0xF8FF),
        ("CJKCompatibilityIdeographs",              0xF900, 0xFAFF),
        ("AlphabeticPresentationForms",             0xFB00, 0xFB4F),
        ("ArabicPresentationForms-A",               0xFB50, 0xFDFF),
        ("ArabicPresentationFormsA",                0xFB50, 0xFDFF),
        ("CombiningHalfMarks",                      0xFE20, 0xFE2F),
        ("CJKCompatibilityForms",                   0xFE30, 0xFE4F),
        ("SmallFormVariants",                       0xFE50, 0xFE6F),
        ("ArabicPresentationForms-B",               0xFE70, 0xFEFF),
        ("ArabicPresentationFormsB",                0xFE70, 0xFEFF),
        ("HalfwidthandFullwidthForms",              0xFF00, 0xFFEF),
        ("Specials",                                0xFFF0, 0xFFFF),
        // Supplementary planes
        ("OldItalic",                               0x10300, 0x1032F),
        ("Gothic",                                  0x10330, 0x1034F),
        ("Deseret",                                 0x10400, 0x1044F),
        ("ByzantineMusicalSymbols",                 0x1D000, 0x1D0FF),
        ("MusicalSymbols",                          0x1D100, 0x1D1FF),
        ("MathematicalAlphanumericSymbols",         0x1D400, 0x1D7FF),
        ("CJKUnifiedIdeographsExtensionB",          0x20000, 0x2A6D6),
        ("CJKCompatibilityIdeographsSupplement",    0x2F800, 0x2FA1F),
        ("Tags",                                    0xE0000, 0xE007F),
        // Plane-15 / Plane-16 supplementary private-use areas.  XSD
        // §F.1.1 names these "SupplementaryPrivateUseArea-A" and
        // "SupplementaryPrivateUseArea-B"; older XSD revisions used
        // a single "PrivateUse" group that covered both, hence the
        // multi-range handling for "PrivateUse" above.
        ("SupplementaryPrivateUseArea-A",           0xF0000, 0xFFFFD),
        ("SupplementaryPrivateUseAreaA",            0xF0000, 0xFFFFD),
        ("SupplementaryPrivateUseArea-B",           0x100000, 0x10FFFD),
        ("SupplementaryPrivateUseAreaB",            0x100000, 0x10FFFD),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_xml_four() {
        let s = xsd_whitespace();
        assert!(s.contains(' '));
        assert!(s.contains('\t'));
        assert!(s.contains('\n'));
        assert!(s.contains('\r'));
        assert!(!s.contains('\u{A0}'), "non-breaking space is not XSD \\s");
    }

    #[test]
    fn digit_is_decimal_only() {
        let d = xsd_digit();
        assert!(d.contains('0'));
        assert!(d.contains('9'));
        assert!(d.contains('\u{0660}'), "Arabic-Indic digit");
        // Ethiopic digits (U+1369..U+1371) were Nd in Unicode 3.2,
        // reclassified to No (OtherNumber) in Unicode 6.2.  XSD §F.1
        // pins to the runtime Unicode database, so they're not in our
        // Nd set on modern toolchains.  Some XSTS tests assume the
        // older classification; we follow the current spec.
        assert!(!d.contains('\u{1369}'), "Ethiopic digits are not Nd in modern Unicode");
        assert!(!d.contains('a'));
    }

    #[test]
    fn category_letter() {
        let l = property_set("L").unwrap();
        assert!(l.contains('a'));
        assert!(l.contains('中'));
        assert!(!l.contains('1'));
        assert!(!l.contains(' '));
    }

    #[test]
    fn category_uppercase() {
        let lu = property_set("Lu").unwrap();
        assert!(lu.contains('A'));
        assert!(!lu.contains('a'));
    }

    #[test]
    fn block_basic_latin() {
        let b = property_set("IsBasicLatin").unwrap();
        assert!(b.contains('A'));
        assert!(b.contains('\u{7F}'));
        assert!(!b.contains('\u{80}'));
    }

    #[test]
    fn unknown_property_returns_none() {
        assert!(property_set("NotARealCategory").is_none());
        assert!(property_set("IsBogusBlock").is_none());
    }

    #[test]
    fn word_excludes_punctuation_and_whitespace() {
        let w = xsd_word();
        assert!(w.contains('a'));
        assert!(w.contains('1'));
        assert!(!w.contains(' '));
        assert!(!w.contains(','));
        assert!(!w.contains('!'));
    }
}
