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
pub use ucd::UnicodeVersion;
pub use unicode::with_unicode_version;

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
}
