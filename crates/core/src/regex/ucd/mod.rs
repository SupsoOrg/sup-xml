//! Bundled Unicode Character Database snapshots.
//!
//! The XSLT / XPath spec doesn't pin a Unicode version — implementations
//! report whatever they were built against.  Most production code paths
//! in this crate use the latest UCD via the `unicode-properties` crate
//! dependency.  Two corners of the W3C XSLT conformance suite, however,
//! pin their expected answers to specific historical Unicode versions:
//!
//! * `tests/misc/regex-classes` — Unicode 6.0 (when these tests were
//!   first written, ~2012).  120 cases.
//! * `tests/misc/unicode-90` — Unicode 9.0 (deliberately added to
//!   probe Unicode 9.0-only digits).  ~1440 cases.
//!
//! Bundling per-version `General_Category` snapshots costs ~45 KB of
//! compiled data total and lets the regex engine answer those tests
//! against the version they were authored for instead of skipping
//! them.  Production users of `fn:matches` etc. continue to get the
//! latest UCD by default.
//!
//! ## Provenance
//!
//! The static tables under `v6_0.rs` / `v9_0.rs` are generated from
//! the official `UnicodeData.txt` files at
//! `https://www.unicode.org/Public/<version>/ucd/UnicodeData.txt`
//! by parsing each line's `General_Category` (field 2), merging
//! contiguous codepoints with the same category, and emitting the
//! result as `Range` arrays sorted by `start`.  Range markers
//! (`<…, First>` / `<…, Last>`) expand to the inclusive span.

pub mod v6_0;
pub mod v9_0;

/// A half-open inclusive codepoint range.  `start <= end`.  Used in
/// the per-version `General_Category` tables to keep the bundled data
/// compact (~8 bytes / entry on 32-bit codepoints).
#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub start: u32,
    pub end:   u32,
}

/// Which Unicode snapshot the regex engine should consult when
/// resolving `\p{…}` properties.  `Latest` defers to the
/// `unicode-properties` crate (production code path); the older
/// variants force lookup against the bundled snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum UnicodeVersion {
    /// Whatever `unicode-properties` ships — what real consumers
    /// of the library actually want (current Unicode at build time).
    #[default]
    Latest,
    /// Unicode 6.0.0 — required by W3C `regex-classes` conformance.
    V6_0,
    /// Unicode 9.0.0 — required by W3C `unicode-90` conformance.
    V9_0,
}

/// Look up a General_Category short-name (`Lu`, `Nd`, `Mn`, …) in the
/// requested Unicode snapshot.  Returns the sorted range list for that
/// category, or `None` if the name isn't a recognised category or the
/// snapshot doesn't carry it.
pub fn category(name: &str, version: UnicodeVersion) -> Option<&'static [Range]> {
    match version {
        UnicodeVersion::V6_0 => v6_0::category(name),
        UnicodeVersion::V9_0 => v9_0::category(name),
        // Latest is handled by the caller via the `unicode-properties`
        // crate; this module is only consulted for the pinned versions.
        UnicodeVersion::Latest => None,
    }
}
