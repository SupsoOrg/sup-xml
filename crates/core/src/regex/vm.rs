//! NFA simulator — Pike-style multi-state evaluation.
//!
//! Maintains two state lists (`current`, `next`) and a per-state
//! generation counter that doubles as a dedup mark.  Each input
//! codepoint advances every live consuming state in lock-step;
//! epsilon `Split`s and zero-width `Assert`s expand into the active
//! set without consuming input (`Assert` only when the position
//! predicate is satisfied).
//!
//! Properties:
//! - O(N · M) worst case where N = input codepoints and M = NFA
//!   states.  No backtracking — `(a|a)*b` against a long run of
//!   `a`s runs in linear time.
//! - The default [`is_match`] enforces whole-input anchoring: a
//!   match requires the simulator to reach `Match` after consuming
//!   the entire input, matching XSD §F.1 semantics.
//! - [`find_match`] runs the simulator with the equivalent of an
//!   implicit `.*?` prefix and an "accept on any Match" check —
//!   the substring-find semantics XPath 2.0 `fn:matches` uses.
//! - Scratch buffers live in a thread-local arena keyed by NFA
//!   identity, so steady-state matching is allocation-free.

use std::cell::RefCell;

use super::nfa::{Program, State, StateId};
use super::parser::AnchorKind;

/// Match `input` against `prog` with whole-input anchoring.
/// Returns true iff some run through the NFA consumes the entire
/// input and ends at a `Match` state.
pub fn is_match(prog: &Program, input: &str) -> bool {
    SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        scratch.run(prog, input, Mode::WholeString)
    })
}

/// Find-style match: returns true iff any substring of `input`
/// (including the empty string at any position) matches `prog`.
/// Equivalent to compiling the source as `.*?(pattern).*?` and
/// asking whether that whole-input match succeeds, but cheaper —
/// the simulator just re-seeds the start state at every position
/// and accepts as soon as any path reaches `Match`.
pub fn find_match(prog: &Program, input: &str) -> bool {
    SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        scratch.run(prog, input, Mode::Find)
    })
}

/// Length, in bytes, of the *leftmost-first* match starting at byte 0
/// of `slice`, or `None` when no prefix matches.  `Some(0)` is a
/// valid empty match.  Leftmost-first (Perl / XPath 2.0 `fn:matches`
/// substring semantics): among the paths the NFA admits, the highest-
/// priority one wins — alternation prefers its earlier branch and a
/// greedy quantifier prefers more repetitions — rather than the
/// globally longest match (so `a|ana` against "anana" yields `a`, not
/// `ana`).  `abs_char_pos` is the offset (in codepoints) of `slice[0]`
/// inside the original input the caller is scanning; `abs_total_chars`
/// is that original input's full codepoint count.  Both are needed so
/// `^` / `$` fire correctly when invoked on a sub-slice during
/// [`super::Pattern::find_iter`].
pub fn leftmost_match_at_start(
    prog: &Program,
    slice: &str,
    abs_char_pos:    usize,
    abs_total_chars: usize,
) -> Option<usize> {
    SCRATCH.with(|cell| {
        let mut scratch = cell.borrow_mut();
        scratch.run_leftmost(prog, slice, abs_char_pos, abs_total_chars)
    })
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    WholeString,
    Find,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const { RefCell::new(Scratch::new()) };
}

/// Thread-local scratch used across all match calls.  Buffers
/// grow to the largest NFA seen and are reused; identical NFAs
/// keep their existing allocation.
struct Scratch {
    current: Vec<StateId>,
    next:    Vec<StateId>,
    /// One generation stamp per state.  A state is "in `current`"
    /// for the current input position iff `marks[s] == cur_mark`,
    /// and likewise for `next` with `cur_mark + 1`.  Cleared
    /// implicitly by bumping `cur_mark` past the previous value.
    marks:   Vec<u32>,
    cur_mark: u32,
}

impl Scratch {
    const fn new() -> Self {
        Self {
            current: Vec::new(),
            next:    Vec::new(),
            marks:   Vec::new(),
            cur_mark: 0,
        }
    }

    fn run(&mut self, prog: &Program, input: &str, mode: Mode) -> bool {
        self.reset(prog.states.len());

        // Total codepoint count drives the end-of-input anchor.
        let total = input.chars().count();
        let mut pos: usize = 0;

        // Seed: epsilon-closure of the start state at position 0.
        let seed_mark = self.cur_mark;
        add(&mut self.current, &mut self.marks, seed_mark,
            prog, prog.start, pos, total);
        if mode == Mode::Find && contains_match(prog, &self.current) {
            return true;
        }

        for c in input.chars() {
            // Advance: bump generation, drain current → next.
            self.cur_mark = self.cur_mark.wrapping_add(1);
            let next_mark = self.cur_mark;
            self.next.clear();

            for i in 0..self.current.len() {
                let sid = self.current[i];
                if let State::Char { class, next } = prog.states[sid as usize] {
                    if prog.classes[class as usize].contains(c) {
                        add(&mut self.next, &mut self.marks, next_mark,
                            prog, next, pos + 1, total);
                    }
                }
            }

            pos += 1;
            std::mem::swap(&mut self.current, &mut self.next);

            // In find mode, re-seed the start state at every new
            // position — equivalent to an implicit `.*?` prefix.
            // Use the current mark so the seeding deduplicates
            // against states already in `current` at this position.
            if mode == Mode::Find {
                add(&mut self.current, &mut self.marks, next_mark,
                    prog, prog.start, pos, total);
            }

            if mode == Mode::Find && contains_match(prog, &self.current) {
                return true;
            }
            if self.current.is_empty() {
                if mode == Mode::Find { continue; }
                return false;
            }
        }

        // Whole-input match: did we reach `Match`?  `Match` was
        // included in `current`'s closure on whichever step
        // discovered it.
        contains_match(prog, &self.current)
    }

    /// Leftmost-first match length (in bytes) for the prefix of
    /// `slice` starting at byte 0.  Walks the thread list in priority
    /// order at each input position; the first `Match` encountered
    /// wins and *cuts* all lower-priority threads (they stop
    /// contributing successors), which is what makes alternation
    /// prefer its earlier branch.  A still-live higher-priority thread
    /// (e.g. a greedy quantifier's loop edge, ordered ahead of its
    /// exit) can extend the match on a later step, so greedy
    /// repetition still reaches the longest run its highest-priority
    /// path admits.  `abs_start` / `abs_total` carry the sub-slice's
    /// position so `^` / `$` fire against the original input.  Returns
    /// `Some(byte_len)` (`Some(0)` for an empty match) or `None`.
    fn run_leftmost(
        &mut self, prog: &Program, slice: &str,
        abs_start: usize, abs_total: usize,
    ) -> Option<usize> {
        self.reset(prog.states.len());
        let mut pos:      usize = abs_start;
        let mut byte_pos: usize = 0;
        let mut matched: Option<usize> = None;

        let seed_mark = self.cur_mark;
        add(&mut self.current, &mut self.marks, seed_mark,
            prog, prog.start, pos, abs_total);

        let mut chars = slice.chars();
        loop {
            // Generation for the next-position thread list.
            self.cur_mark = self.cur_mark.wrapping_add(1);
            let next_mark = self.cur_mark;
            self.next.clear();
            let c = chars.next();

            // Threads are ordered high-to-low priority.  Consuming
            // states feed `next` (preserving order); the first `Match`
            // records the candidate end and cuts everything below it.
            for i in 0..self.current.len() {
                let sid = self.current[i];
                match prog.states[sid as usize] {
                    State::Match => {
                        matched = Some(byte_pos);
                        break;
                    }
                    State::Char { class, next } => {
                        if let Some(ch) = c {
                            if prog.classes[class as usize].contains(ch) {
                                add(&mut self.next, &mut self.marks, next_mark,
                                    prog, next, pos + 1, abs_total);
                            }
                        }
                    }
                    // Split / Assert were epsilon-expanded by `add`.
                    _ => {}
                }
            }

            match c {
                None => break, // exhausted the slice
                Some(ch) => {
                    pos      += 1;
                    byte_pos += ch.len_utf8();
                    std::mem::swap(&mut self.current, &mut self.next);
                    if self.current.is_empty() { break; }
                }
            }
        }

        matched
    }

    /// Resize and reset the mark vector for a fresh NFA.  Bumping
    /// the generation past any stored mark would lap (u32 wraps);
    /// reset by zero-fill when that happens or when the state count
    /// changes.  The first live mark value is 1 — 0 is reserved as
    /// the "never touched" sentinel.
    fn reset(&mut self, state_count: usize) {
        let need_resize = self.marks.len() != state_count;
        let need_wrap_reset = self.cur_mark >= u32::MAX - 2;
        if need_resize {
            self.marks.clear();
            self.marks.resize(state_count, 0);
            self.cur_mark = 1;
        } else if need_wrap_reset {
            self.marks.iter_mut().for_each(|m| *m = 0);
            self.cur_mark = 1;
        } else {
            self.cur_mark = self.cur_mark.wrapping_add(1);
        }
        self.current.clear();
        self.next.clear();
    }
}

fn contains_match(prog: &Program, list: &[StateId]) -> bool {
    list.iter().any(|&s| matches!(prog.states[s as usize], State::Match))
}

/// Add `sid` to `list` unless its mark already equals `mark` (i.e.
/// it was added at this input position).  Epsilon-expands `Split`
/// states and conditional zero-width `Assert` states recursively;
/// consuming states are pushed onto the list for the next input
/// step.  `pos` / `total` are the current codepoint position and
/// total input length, used to evaluate position assertions.
fn add(
    list:  &mut Vec<StateId>,
    marks: &mut [u32],
    mark:  u32,
    prog:  &Program,
    sid:   StateId,
    pos:   usize,
    total: usize,
) {
    if marks[sid as usize] == mark {
        return;
    }
    marks[sid as usize] = mark;
    match prog.states[sid as usize] {
        State::Split(a, b) => {
            add(list, marks, mark, prog, a, pos, total);
            add(list, marks, mark, prog, b, pos, total);
        }
        State::Assert { kind, next } => {
            let ok = match kind {
                AnchorKind::Start => pos == 0,
                AnchorKind::End   => pos == total,
            };
            if ok {
                add(list, marks, mark, prog, next, pos, total);
            }
        }
        _ => list.push(sid),
    }
}

#[cfg(test)]
mod tests {
    use super::super::nfa::compile;
    use super::super::parser::{parse, parse_with, Dialect};

    fn matches(pat: &str, s: &str) -> bool {
        let prog = compile(&parse(pat).unwrap()).unwrap();
        super::is_match(&prog, s)
    }

    fn finds(pat: &str, s: &str) -> bool {
        let prog = compile(&parse_with(pat, Dialect::Xpath).unwrap()).unwrap();
        super::find_match(&prog, s)
    }

    #[test]
    fn empty_pattern_matches_empty_string() {
        assert!(matches("", ""));
        assert!(!matches("", "x"));
    }

    #[test]
    fn literal() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "ab"));
        assert!(!matches("abc", "abcd"));
        assert!(!matches("abc", "xbc"));
    }

    #[test]
    fn alternation() {
        assert!(matches("a|b|c", "a"));
        assert!(matches("a|b|c", "b"));
        assert!(matches("a|b|c", "c"));
        assert!(!matches("a|b|c", "d"));
    }

    #[test]
    fn star() {
        assert!(matches("a*", ""));
        assert!(matches("a*", "a"));
        assert!(matches("a*", "aaaa"));
        assert!(!matches("a*", "ab"));
    }

    #[test]
    fn plus() {
        assert!(!matches("a+", ""));
        assert!(matches("a+", "a"));
        assert!(matches("a+", "aaaa"));
    }

    #[test]
    fn optional() {
        assert!(matches("a?", ""));
        assert!(matches("a?", "a"));
        assert!(!matches("a?", "aa"));
    }

    #[test]
    fn counted_exact() {
        assert!(matches("a{3}", "aaa"));
        assert!(!matches("a{3}", "aa"));
        assert!(!matches("a{3}", "aaaa"));
    }

    #[test]
    fn counted_range() {
        assert!(!matches("a{2,4}", "a"));
        assert!(matches("a{2,4}", "aa"));
        assert!(matches("a{2,4}", "aaa"));
        assert!(matches("a{2,4}", "aaaa"));
        assert!(!matches("a{2,4}", "aaaaa"));
    }

    #[test]
    fn counted_unbounded() {
        assert!(matches("a{2,}", "aa"));
        assert!(matches("a{2,}", "aaaaaa"));
        assert!(!matches("a{2,}", "a"));
    }

    #[test]
    fn class_subtraction_matches_correctly() {
        // `[a-z-[aeiou]]+` should accept consonant-only strings.
        assert!(matches("[a-z-[aeiou]]+", "bcdfg"));
        assert!(!matches("[a-z-[aeiou]]+", "abcdf"));
    }

    #[test]
    fn xsd_whitespace_is_not_unicode_whitespace() {
        // U+00A0 is Unicode whitespace but not XSD `\s`.
        assert!(matches(r"\s+", " \t\n"));
        assert!(!matches(r"\s+", "\u{A0}"));
    }

    #[test]
    fn zip_code_pattern() {
        assert!(matches(r"\d{5}(-\d{4})?", "12345"));
        assert!(matches(r"\d{5}(-\d{4})?", "12345-6789"));
        assert!(!matches(r"\d{5}(-\d{4})?", "1234"));
        assert!(!matches(r"\d{5}(-\d{4})?", "12345-678"));
    }

    #[test]
    fn dot_excludes_newline() {
        assert!(matches(".+", "abc"));
        assert!(!matches(".+", "a\nb"));
    }

    #[test]
    fn linear_time_on_adversarial_pattern() {
        // The classic catastrophic-backtracking trap: `(a|a)*b`
        // against 30 `a`s and no `b`.  A naive backtracker would
        // explore 2^30 paths.  The Pike VM is linear.
        let pat = "(a|a)*b";
        let prog = compile(&parse(pat).unwrap()).unwrap();
        let s: String = "a".repeat(30);
        let t0 = std::time::Instant::now();
        let m = super::is_match(&prog, &s);
        assert!(!m);
        assert!(t0.elapsed().as_millis() < 50, "took {:?}", t0.elapsed());
    }

    #[test]
    fn unicode_property_letter() {
        assert!(matches(r"\p{L}+", "héllo"));
        assert!(matches(r"\p{L}+", "中文"));
        assert!(!matches(r"\p{L}+", "h3llo"));
    }

    #[test]
    fn block_basic_latin() {
        assert!(matches(r"\p{IsBasicLatin}+", "hello"));
        assert!(!matches(r"\p{IsBasicLatin}+", "héllo"));
    }

    // ── XPath 2.0 anchors and find semantics ──

    #[test]
    fn xpath_find_substring() {
        assert!(finds("bar",   "foo bar baz"));
        assert!(finds(r"\d+",  "abc 123 def"));
        assert!(!finds(r"\d+", "abcdef"));
    }

    #[test]
    fn xpath_start_anchor() {
        assert!(finds("^foo", "foo bar"));
        assert!(!finds("^foo", "bar foo"));
    }

    #[test]
    fn xpath_end_anchor() {
        assert!(finds("bar$", "foo bar"));
        assert!(!finds("bar$", "bar foo"));
    }

    #[test]
    fn xpath_both_anchors_force_whole_string() {
        assert!(finds("^foo$", "foo"));
        assert!(!finds("^foo$", "foo bar"));
        assert!(!finds("^foo$", "x foo"));
    }

    #[test]
    fn xpath_empty_pattern_finds_everywhere() {
        // matches() with empty pattern matches the empty string at
        // position 0 — always true for any input.
        assert!(finds("", ""));
        assert!(finds("", "anything"));
    }

    #[test]
    fn xpath_anchored_empty_against_nonempty() {
        // ^$ only succeeds on the empty input.
        assert!(finds("^$", ""));
        assert!(!finds("^$", "x"));
    }

    // ── leftmost-first match length (substring find) ──

    fn lm(pat: &str, s: &str) -> Option<usize> {
        let prog = compile(&parse_with(pat, Dialect::Xpath).unwrap()).unwrap();
        let total = s.chars().count();
        super::leftmost_match_at_start(&prog, s, 0, total)
    }

    #[test]
    fn leftmost_first_alternation_prefers_earlier_branch() {
        // `a|ana` against "ana…" matches `a` (1 byte), not the longer
        // `ana` — XPath leftmost-first, not POSIX leftmost-longest.
        assert_eq!(lm("a|ana", "anana"), Some(1));
        assert_eq!(lm("ana|a", "anana"), Some(3)); // earlier branch wins
        assert_eq!(lm("a|ab", "ab"), Some(1));
    }

    #[test]
    fn leftmost_first_greedy_quantifier_takes_longest_run() {
        // Greedy `*`/`+` still reach the longest run: the loop edge is
        // ordered ahead of the exit edge, so the high-priority thread
        // keeps consuming.
        assert_eq!(lm("a*", "aaa"), Some(3));
        assert_eq!(lm("a*", "aaab"), Some(3));
        assert_eq!(lm("a*", "b"), Some(0));   // empty match
        assert_eq!(lm("(an)*a", "anana"), Some(5));
    }

    #[test]
    fn xpath_literal_caret_dollar_in_xsd_mode() {
        // XSD dialect treats `^` and `$` as literal chars.
        assert!(matches("^abc$", "^abc$"));
        assert!(!matches("^abc$", "abc"));
    }
}
