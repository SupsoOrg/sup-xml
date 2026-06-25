//! XSD §F regex engine — native parser, NFA, Pike VM.
//!
//! XSD Part 2 §F defines its own regex flavour: implicit
//! whole-string anchoring, an `\i` / `\c` shortcut family for XML
//! Name characters, character class subtraction (`[a-z-[aeiou]]`),
//! the spec's own `\s` / `\w` definitions, and `\p{IsBlock}` named
//! Unicode blocks.  It also forbids back-references, lookaround,
//! and inline modifiers — XSD patterns are pure regular languages.
//!
//! ## Pipeline
//!
//! 1. [`parser`] consumes XSD §F source into an [`parser::Expr`] AST.
//! 2. [`nfa::Program`] compiles the AST via Thompson's construction
//!    into a flat state list with a side table of character classes
//!    (`Vec<ClassSet>`, hash-consed for dedup).
//! 3. [`vm`] runs the NFA against an input string using two
//!    state-set buffers and a generation-counter dedup, owned by a
//!    thread-local scratch arena so `is_match` stays allocation-free
//!    in steady state.
//!
//! The matcher is O(N · M) in the input length times NFA state
//! count and never backtracks — pathological patterns like
//! `(a|a)*b` cost the same as `a*b`.

#![forbid(unsafe_code)]

mod class;
mod linear;
mod nfa;
pub mod parser;
mod ucd;
mod unicode;
mod vm;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use linear::LinearMatcher;
use nfa::Program;

pub use parser::Dialect;
pub use vm::Captures;
pub use ucd::UnicodeVersion;
pub use unicode::with_unicode_version;

/// The XPath/XSD regex flags (F&O §5.6.1.1): `i` case-insensitive,
/// `s` dotall, `m` multiline, `x` extended.  `q` (literal) is handled by
/// the caller (the whole pattern is escaped), not here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Flags {
    pub case_insensitive: bool,
    pub dotall:           bool,
    pub multiline:        bool,
    pub extended:         bool,
}

impl Flags {
    /// Parse a flags string (`"imsx"`); unknown flag letters are a
    /// (FORX0001) error, surfaced as `Err(letter)`.
    pub fn parse(s: &str) -> Result<Self, char> {
        let mut f = Flags::default();
        for c in s.chars() {
            match c {
                'i' => f.case_insensitive = true,
                's' => f.dotall = true,
                'm' => f.multiline = true,
                'x' => f.extended = true,
                'q' => {} // literal — applied by the caller, not the engine
                other => return Err(other),
            }
        }
        Ok(f)
    }
}

thread_local! {
    /// Per-thread compile cache keyed by (src, dialect, version).
    /// Patterns are returned as `Arc<Pattern>` so callers share one
    /// NFA across calls — critical for hot paths like `fn:matches`
    /// inside a 1.1M-codepoint iteration where the pattern source
    /// is constant.  Unbounded; production callers with thousands
    /// of distinct patterns should fall back to [`Pattern::compile_with`]
    /// directly to avoid the cache growing without bound.
    static COMPILE_CACHE: RefCell<HashMap<(String, Dialect, UnicodeVersion), Arc<Pattern>>>
        = RefCell::new(HashMap::new());
}

/// Cached compile through the thread-local pattern cache.  See
/// [`COMPILE_CACHE`] for the cache lifetime / scope.  Misses fall
/// through to [`Pattern::compile_with`]; the resulting `Pattern`
/// is wrapped in `Arc` and inserted before being returned.
pub fn compile_with_cached(
    src: &str, dialect: Dialect,
) -> Result<Arc<Pattern>, String> {
    let version = unicode::current_ucd_version();
    let key = (src.to_string(), dialect, version);
    if let Some(hit) = COMPILE_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(hit);
    }
    let pat = Pattern::compile_with(src, dialect)?;
    let arc = Arc::new(pat);
    COMPILE_CACHE.with(|c| {
        c.borrow_mut().insert(key, arc.clone());
    });
    Ok(arc)
}

thread_local! {
    static COMPILE_CACHE_F: RefCell<HashMap<(String, Dialect, Flags, UnicodeVersion), Arc<Pattern>>>
        = RefCell::new(HashMap::new());
}

/// Cached compile with explicit [`Flags`].  See [`compile_with_cached`].
pub fn compile_with_cached_flags(
    src: &str, dialect: Dialect, flags: Flags,
) -> Result<Arc<Pattern>, String> {
    let version = unicode::current_ucd_version();
    let key = (src.to_string(), dialect, flags, version);
    if let Some(hit) = COMPILE_CACHE_F.with(|c| c.borrow().get(&key).cloned()) {
        return Ok(hit);
    }
    let arc = Arc::new(Pattern::compile_with_flags(src, dialect, flags)?);
    COMPILE_CACHE_F.with(|c| { c.borrow_mut().insert(key, arc.clone()); });
    Ok(arc)
}

/// A compiled XSD §F pattern.
///
/// Compilation parses the source and either lowers it to a
/// forward-only linear matcher (the common `[class]{quant}…` shape)
/// or compiles it to an NFA driven by a Pike VM.  Matching is
/// linear in the input length in both cases; the linear path skips
/// per-codepoint NFA dispatch for the patterns that fit it.
pub struct Pattern {
    src:  String,
    body: Body,
}

enum Body {
    /// Forward-only fast path — see [`linear::LinearMatcher`].
    Linear(LinearMatcher),
    /// Full NFA simulation — see [`vm`].
    Full(Program),
}

impl Pattern {
    /// Compile an XSD §F pattern.  Returns `Err` on syntax errors,
    /// disallowed constructs (back-references, lookaround, inline
    /// modifiers), or quantifier counts that would exceed the
    /// counted-repetition cap.
    pub fn compile(src: &str) -> Result<Self, String> {
        Self::compile_with(src, Dialect::Xsd)
    }

    /// Compile under a specific source dialect.  XPath 2.0 mode
    /// recognises `^` / `$` as position anchors; XSD mode treats
    /// them as literal characters.  See [`Dialect`].
    ///
    /// XSD-mode patterns can take the linear fast path when their
    /// shape fits it.  XPath-mode patterns always route through
    /// the NFA — find semantics needs the VM's per-position
    /// re-seeding, which the linear matcher doesn't support.
    pub fn compile_with(src: &str, dialect: Dialect) -> Result<Self, String> {
        let ast = parser::parse_with(src, dialect)?;
        let body = match dialect {
            Dialect::Xsd => match LinearMatcher::try_build(&ast) {
                Some(lm) => Body::Linear(lm),
                None     => Body::Full(nfa::compile(&ast)?),
            },
            Dialect::Xpath | Dialect::Xpath20 => Body::Full(nfa::compile(&ast)?),
        };
        Ok(Self { src: src.into(), body })
    }

    /// Compile under a dialect with the full XPath/XSD flag set.  The
    /// `s` (dotall) and `x` (extended) flags affect parsing; `i`
    /// (case-insensitive) and `m` (multiline) are recorded on the NFA
    /// and applied at match time.  Flagged patterns always use the Pike
    /// VM (the linear fast path doesn't carry match-time flags).
    pub fn compile_with_flags(src: &str, dialect: Dialect, flags: Flags) -> Result<Self, String> {
        let ast = parser::parse_with_all_flags(
            src, dialect, flags.dotall, flags.extended, flags.case_insensitive)?;
        if !flags.case_insensitive && !flags.multiline {
            // No match-time flags — reuse the ordinary (possibly linear)
            // build so the fast path still applies in XSD mode.
            let body = match dialect {
                Dialect::Xsd => match LinearMatcher::try_build(&ast) {
                    Some(lm) => Body::Linear(lm),
                    None     => Body::Full(nfa::compile(&ast)?),
                },
                Dialect::Xpath | Dialect::Xpath20 => Body::Full(nfa::compile(&ast)?),
            };
            return Ok(Self { src: src.into(), body });
        }
        let mut prog = nfa::compile(&ast)?;
        prog.case_insensitive = flags.case_insensitive;
        prog.multiline = flags.multiline;
        Ok(Self { src: src.into(), body: Body::Full(prog) })
    }

    /// Compile bypassing the linear fast path — always builds the
    /// Pike VM body.  Used by the regex microbench in
    /// `crates/bench/benches/xsd_regex.rs` to measure the speedup
    /// the linear path provides on patterns that fit it.  Not
    /// part of the supported API.
    #[doc(hidden)]
    pub fn compile_nfa_only(src: &str) -> Result<Self, String> {
        let ast = parser::parse(src)?;
        Ok(Self { src: src.into(), body: Body::Full(nfa::compile(&ast)?) })
    }

    /// Returns true iff `s` matches the pattern in its entirety.
    /// XSD §F patterns are implicitly anchored to both ends of the
    /// lexical value.
    pub fn is_match(&self, s: &str) -> bool {
        match &self.body {
            Body::Linear(m) => m.is_match(s),
            Body::Full(p)   => vm::is_match(p, s),
        }
    }

    /// Find-style match: true iff any substring of `s` matches the
    /// pattern.  This is the semantics XPath 2.0 `fn:matches` uses
    /// — `matches("foo bar", "bar")` is true.  Pair with the
    /// [`Dialect::Xpath`] compiler so `^` / `$` can be used to
    /// re-anchor when the caller wants whole-input semantics.
    ///
    /// Only valid on patterns compiled with [`Dialect::Xpath`] —
    /// XSD-mode patterns may take the linear whole-string fast
    /// path and have no NFA to run find against.
    pub fn find_match(&self, s: &str) -> bool {
        match &self.body {
            Body::Linear(_) => panic!(
                "find_match called on a Linear-compiled Pattern; \
                 compile with Dialect::Xpath for find semantics"
            ),
            Body::Full(p)   => vm::find_match(p, s),
        }
    }

    /// Iterate the non-overlapping matches of the pattern over
    /// `input`, in left-to-right order, returning `(start_byte,
    /// end_byte)` for each.  Leftmost-first match: at each position
    /// the simulator takes the highest-priority path the NFA admits
    /// (XPath / Perl semantics — `a|ana` prefers `a`), then resumes
    /// searching immediately after the match's end.  Zero-length
    /// matches advance one character past the match position so the
    /// loop terminates on patterns like `a*`.
    ///
    /// Used by `xsl:analyze-string` to partition its input into
    /// matching / non-matching segments.  Only valid on patterns
    /// compiled with [`Dialect::Xpath`] — XSD-mode patterns may
    /// take the linear whole-string fast path that has no NFA.
    pub fn find_iter(&self, input: &str) -> Vec<(usize, usize)> {
        let prog = match &self.body {
            Body::Full(p)   => p,
            Body::Linear(_) => panic!(
                "find_iter called on a Linear-compiled Pattern; \
                 compile with Dialect::Xpath for find-style iteration"
            ),
        };
        // Pre-compute the original input's codepoint count so the
        // simulator's `$` anchor fires only at end-of-input.  The
        // running `char_pos` increments as we step over each match.
        let total_chars = input.chars().count();
        let mut out:      Vec<(usize, usize)> = Vec::new();
        let mut pos:      usize = 0;
        let mut char_pos: usize = 0;
        while pos <= input.len() {
            let slice = &input[pos..];
            match vm::leftmost_match_at_start(prog, slice, char_pos, total_chars) {
                Some(len) if len > 0 => {
                    out.push((pos, pos + len));
                    // Advance `char_pos` by the number of codepoints
                    // the match consumed.
                    char_pos += input[pos..pos + len].chars().count();
                    pos += len;
                }
                Some(_) => {
                    // Zero-length match — record it and step past
                    // the current codepoint so we don't loop.
                    out.push((pos, pos));
                    if pos == input.len() { break; }
                    let c = input[pos..].chars().next().unwrap();
                    pos      += c.len_utf8();
                    char_pos += 1;
                }
                None => {
                    if pos == input.len() { break; }
                    let c = input[pos..].chars().next().unwrap();
                    pos      += c.len_utf8();
                    char_pos += 1;
                }
            }
        }
        out
    }

    /// All non-overlapping matches of the pattern over `input`, left to
    /// right, each with its capture-group byte spans (`group 0` is the
    /// whole match).  Used by `fn:replace` and `xsl:analyze-string`.  Only
    /// valid on patterns compiled with [`Dialect::Xpath`] — XSD-mode
    /// patterns may take the linear fast path that has no NFA to capture
    /// against.
    pub fn find_captures(&self, input: &str) -> Vec<Captures> {
        match &self.body {
            Body::Full(p)   => vm::find_captures(p, input),
            Body::Linear(_) => panic!(
                "find_captures called on a Linear-compiled Pattern; \
                 compile with Dialect::Xpath for capture iteration"
            ),
        }
    }

    /// Number of capturing groups in the pattern (excluding group 0).
    pub fn group_count(&self) -> usize {
        match &self.body {
            Body::Full(p)   => (p.num_slots / 2).saturating_sub(1) as usize,
            Body::Linear(_) => 0,
        }
    }

    /// Original XSD-flavour source, preserved for diagnostics.
    pub fn src(&self) -> &str { &self.src }
}

impl Clone for Pattern {
    fn clone(&self) -> Self {
        let body = match &self.body {
            Body::Linear(m) => Body::Linear(m.clone()),
            Body::Full(p)   => Body::Full(p.clone()),
        };
        Self { src: self.src.clone(), body }
    }
}

impl std::fmt::Debug for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pattern").field("src", &self.src).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm(src: &str, flags: &str, input: &str) -> bool {
        let f = Flags::parse(flags).unwrap();
        Pattern::compile_with_flags(src, Dialect::Xpath, f)
            .unwrap()
            .find_match(input)
    }

    #[test]
    fn flag_i_positive_and_negated_classes() {
        // Positive class: a literal/class matches either case.
        assert!(fm("^abc$", "i", "ABC"));
        assert!(fm("[a-z]+", "i", "ABC"));
        // Negated class under `i` must exclude BOTH cases — `[^a-z]`
        // does not match a letter of either case, but matches a digit.
        assert!(!fm("^[^a-z]$", "i", "A"));
        assert!(!fm("^[^a-z]$", "i", "a"));
        assert!(fm("^[^a-z]$", "i", "5"));
    }

    fn caps(src: &str, input: &str) -> Vec<Captures> {
        Pattern::compile_with(src, Dialect::Xpath).unwrap().find_captures(input)
    }

    #[test]
    fn captures_basic_groups() {
        let c = caps("(a)(b)", "ab");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], vec![Some((0, 2)), Some((0, 1)), Some((1, 2))]);
    }

    #[test]
    fn captures_quantified_groups() {
        let c = caps("(a+)(b+)", "aabb");
        assert_eq!(c[0], vec![Some((0, 4)), Some((0, 2)), Some((2, 4))]);
    }

    #[test]
    fn captures_non_participating_alternative() {
        // `(a)|(b)` on "b": group 1 absent, group 2 = "b".
        let c = caps("(a)|(b)", "b");
        assert_eq!(c[0], vec![Some((0, 1)), None, Some((0, 1))]);
    }

    #[test]
    fn captures_find_all_nonoverlapping() {
        let c = caps("([0-9])", "a1b2");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], vec![Some((1, 2)), Some((1, 2))]);
        assert_eq!(c[1], vec![Some((3, 4)), Some((3, 4))]);
    }

    #[test]
    fn captures_non_capturing_group_not_numbered() {
        // `(?:a)(b)` — only one capturing group.
        let c = caps("(?:a)(b)", "ab");
        assert_eq!(c[0], vec![Some((0, 2)), Some((1, 2))]);
    }

    #[test]
    fn flag_s_dotall() {
        assert!(fm("a.b", "s", "a\nb"));
        assert!(!fm("a.b", "", "a\nb"));
    }

    #[test]
    fn flag_m_multiline_anchors() {
        assert!(fm("^bar$", "m", "foo\nbar"));
        assert!(!fm("^bar$", "", "foo\nbar"));
    }

    #[test]
    fn flag_x_extended_ignores_whitespace() {
        assert!(fm("a b c", "x", "abc"));
        // Escaped whitespace stays literal even under `x`.
        assert!(fm("a\\ b", "x", "a b"));
    }

    /// XPath 2.0 §7.6 adds `^` / `$` as zero-width anchors on top of
    /// the XSD grammar.  Both the `Xpath` (3.0) and `Xpath20` dialects
    /// must honour them — `Xpath20` only drops the XPath 3.0 extensions
    /// (`(?:…)`, inline flags), not the anchors.  XSD mode alone treats
    /// `^` / `$` as literal characters.
    #[test]
    fn caret_dollar_are_anchors_in_both_xpath_dialects() {
        for d in [Dialect::Xpath, Dialect::Xpath20] {
            let re = Pattern::compile_with("^a$", d).unwrap();
            assert!(re.is_match("a"), "{d:?}: `^a$` should anchor-match \"a\"");
            assert!(!re.is_match("^a$"), "{d:?}: `^`/`$` must be anchors, not literals");

            // The shape `re.xsl` in the W3C suite builds: `^(...)$`.
            let g = Pattern::compile_with("^(a+)$", d).unwrap();
            assert!(g.is_match("aaa"), "{d:?}: `^(a+)$` should match \"aaa\"");
            assert!(!g.is_match("baaa"), "{d:?}: anchored, so a leading `b` fails");

            // fn:matches uses find (substring) semantics; anchors must
            // still constrain the match position.
            assert!(re.find_match("a"), "{d:?}: find `^a$` in \"a\"");
            assert!(!re.find_match("xa"), "{d:?}: `^` pins to start");
            let tail = Pattern::compile_with("a$", d).unwrap();
            assert!(tail.find_match("ba"), "{d:?}: `a$` matches the tail of \"ba\"");
            assert!(!tail.find_match("ab"), "{d:?}: `$` pins to end");
        }
    }

    /// XSD §F.1 has no anchors — `^` / `$` are ordinary characters
    /// there, and patterns are implicitly whole-value anchored.
    #[test]
    fn caret_dollar_are_literals_in_xsd_dialect() {
        let re = Pattern::compile_with("^a$", Dialect::Xsd).unwrap();
        assert!(re.is_match("^a$"), "XSD: `^`/`$` are literal characters");
        assert!(!re.is_match("a"), "XSD: the literal `^`/`$` must be present");
    }

    // ─────────────────────── reluctant quantifiers ───────────────────────

    #[test]
    fn reluctant_vs_greedy_find_all() {
        // Greedy `a+` swallows the whole run (one match); reluctant `a+?`
        // takes the shortest run at each position (three single-char matches).
        assert_eq!(caps("a+", "aaa").len(), 1);
        assert_eq!(caps("a+?", "aaa").len(), 3);
    }

    #[test]
    fn reluctant_group_span_is_shortest() {
        // `<.+>` greedy spans the whole input; `<.+?>` stops at the first `>`.
        assert_eq!(caps("(<.+>)", "<a><b>")[0][1], Some((0, 6)));
        let rel = caps("(<.+?>)", "<a><b>");
        assert_eq!(rel.len(), 2);
        assert_eq!(rel[0][1], Some((0, 3)));
        assert_eq!(rel[1][1], Some((3, 6)));
    }

    #[test]
    fn reluctant_star_and_bounded() {
        // `a*?` prefers zero; `a{2,4}?` prefers the lower bound (2).
        assert_eq!(caps("a*?", "aa").len(), 3);            // empty at 0,1,2
        let b = caps("(a{2,4}?)", "aaaa");
        assert_eq!(b[0][1], Some((0, 2)));
    }

    #[test]
    fn greedy_bounded_takes_max_in_range() {
        // Sanity counterpart: greedy `a{2,4}` takes 4 of the 4 a's.
        assert_eq!(caps("(a{2,4})", "aaaa")[0][1], Some((0, 4)));
    }

    // ───────────────────────── capture edge cases ────────────────────────

    #[test]
    fn captures_nested_groups() {
        // `((a)(b))` — group 1 wraps 2 and 3.
        let c = caps("((a)(b))", "ab");
        assert_eq!(c[0], vec![Some((0, 2)), Some((0, 2)), Some((0, 1)), Some((1, 2))]);
    }

    #[test]
    fn captures_quantified_group_keeps_last_iteration() {
        // `(a)+` over "aaa": group 1 records only the final iteration.
        let c = caps("(a)+", "aaa");
        assert_eq!(c[0][0], Some((0, 3)));
        assert_eq!(c[0][1], Some((2, 3)));
    }

    #[test]
    fn captures_anchored_group() {
        assert_eq!(caps("^(a+)$", "aaa")[0][1], Some((0, 3)));
        // `$`-anchored group must reach the end.
        assert!(caps("^(a+)$", "aaab").is_empty());
    }

    #[test]
    fn captures_empty_group_participates() {
        // `(a*)` against "" matches once with a zero-length group span.
        let c = caps("(a*)", "");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0], vec![Some((0, 0)), Some((0, 0))]);
    }

    #[test]
    fn captures_optional_group_absent_is_none() {
        // `(a)?b` against "b": group 1 didn't participate.
        let c = caps("(a)?b", "b");
        assert_eq!(c[0], vec![Some((0, 1)), None]);
    }

    // ───────────────────────── flag combinations ─────────────────────────

    #[test]
    fn flag_combo_i_and_m() {
        // Case-insensitive + multiline: `^abc$` matches an upper-case line.
        assert!(fm("^abc$", "im", "xyz\nABC"));
        assert!(!fm("^abc$", "i", "xyz\nABC")); // no m → no interior anchor
    }

    #[test]
    fn flag_combo_s_and_x() {
        // dotall + extended: whitespace ignored, `.` crosses the newline.
        assert!(fm("a . b", "sx", "a\nb"));
    }

    #[test]
    fn flag_x_strips_comments() {
        assert!(fm("ab  # a comment\nc", "x", "abc"));
    }

    // ─────────────────────── case-insensitivity depth ────────────────────

    #[test]
    fn flag_i_range_matches_opposite_case() {
        assert!(fm("^[A-Z]+$", "i", "abc"));
        assert!(fm("^[a-z]+$", "i", "ABC"));
    }

    #[test]
    fn flag_i_negated_range_excludes_both_cases() {
        // `[^A-Z]` under i excludes a-z too (case-closed before complement).
        assert!(!fm("^[^A-Z]$", "i", "a"));
        assert!(!fm("^[^A-Z]$", "i", "A"));
        assert!(fm("^[^A-Z]$", "i", "0"));
    }

    #[test]
    fn flag_i_does_not_leak_without_flag() {
        assert!(!fm("^abc$", "", "ABC"));
    }

    // ───────────────────────────── find_iter ─────────────────────────────

    #[test]
    fn find_iter_non_overlapping_spans() {
        let p = Pattern::compile_with("ab", Dialect::Xpath).unwrap();
        assert_eq!(p.find_iter("xababy"), vec![(1, 3), (3, 5)]);
    }

    #[test]
    fn find_iter_empty_matches_step_one_codepoint() {
        // `a*` matches empty between every position plus the runs.
        let p = Pattern::compile_with("a*", Dialect::Xpath).unwrap();
        // "ba": empty@0, then 'a' run at 1, empty@2 → terminates (no hang).
        assert!(!p.find_iter("ba").is_empty());
    }

    #[test]
    fn empty_string_match_detection_for_forx0003() {
        // fn:replace / fn:tokenize reject patterns that match the empty
        // string (FORX0003) via find_match("").
        let m = |p: &str| Pattern::compile_with(p, Dialect::Xpath).unwrap().find_match("");
        assert!(m("a*"));
        assert!(m("a?"));
        assert!(m("(abc)*"));
        assert!(!m("a+"));
        assert!(!m("abc"));
    }

    #[test]
    fn captures_honor_multiline_flag() {
        // find_captures runs the Pike VM with full input context, so the
        // `m` flag's interior `^` boundary works (unlike the find_iter
        // slice path).  `^(b+)` matches at the line start after the \n.
        let f = Flags::parse("m").unwrap();
        let p = Pattern::compile_with_flags("^(b+)", Dialect::Xpath, f).unwrap();
        let c = p.find_captures("aaa\nbbb");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0][1], Some((4, 7)));
    }

    #[test]
    fn captures_case_insensitive_group() {
        let f = Flags::parse("i").unwrap();
        let p = Pattern::compile_with_flags("(ab)+", Dialect::Xpath, f).unwrap();
        let c = p.find_captures("ABab");
        assert_eq!(c[0][0], Some((0, 4)));
    }

    #[test]
    fn group_count_reported() {
        assert_eq!(Pattern::compile_with("(a)(b)(c)", Dialect::Xpath).unwrap().group_count(), 3);
        assert_eq!(Pattern::compile_with("(?:a)(b)", Dialect::Xpath).unwrap().group_count(), 1);
        assert_eq!(Pattern::compile_with("abc", Dialect::Xpath).unwrap().group_count(), 0);
    }
}
