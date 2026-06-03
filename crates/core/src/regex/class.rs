//! Character classes as sorted, non-overlapping codepoint ranges.
//!
//! Every [`ClassSet`] maintains the invariant: ranges are stored in
//! ascending order of `lo`, no two ranges overlap or abut (abutting
//! ranges are coalesced).  The Unicode universe excludes the
//! surrogate hole `[0xD800, 0xDFFF]` — UTF-8 source can never
//! contain those codepoints, so a `complement` skips them.
//!
//! All algebraic operations preserve the invariant.

use std::cmp::max;

/// First and last codepoints in the Unicode scalar value space.
const CP_MIN: u32 = 0x0000;
const CP_MAX: u32 = 0x10_FFFF;

/// UTF-16 surrogate range — never a valid scalar value.
const SUR_LO: u32 = 0xD800;
const SUR_HI: u32 = 0xDFFF;

/// A set of codepoints expressed as sorted, disjoint, non-abutting
/// closed ranges `[lo, hi]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ClassSet {
    ranges: Vec<(u32, u32)>,
}

impl ClassSet {
    /// Empty set — matches nothing.
    pub fn empty() -> Self { Self { ranges: Vec::new() } }

    /// The entire Unicode scalar value space minus the surrogate
    /// hole.  Equivalent to XSD's `.` would be (excluding line
    /// terminators).  Used as the universe for [`complement`].
    pub fn universe() -> Self {
        Self {
            ranges: vec![
                (CP_MIN,    SUR_LO - 1),
                (SUR_HI + 1, CP_MAX),
            ],
        }
    }

    /// Single codepoint.
    pub fn from_char(c: char) -> Self {
        Self::from_range(c as u32, c as u32)
    }

    /// Single inclusive range.  Callers are responsible for `lo <= hi`
    /// and for not straddling the surrogate hole — that's only a
    /// concern for hand-built ranges; class parsing rejects bad
    /// ranges earlier.
    pub fn from_range(lo: u32, hi: u32) -> Self {
        debug_assert!(lo <= hi);
        Self { ranges: vec![(lo, hi)] }
    }

    /// Build from an already-sorted, disjoint, non-abutting range
    /// list.  Used by the Unicode tables; debug-asserts the invariant.
    pub fn from_sorted_ranges(ranges: Vec<(u32, u32)>) -> Self {
        debug_assert!(is_canonical(&ranges));
        Self { ranges }
    }

    /// Build from an arbitrary range list; sorts + coalesces.
    pub fn from_ranges(mut ranges: Vec<(u32, u32)>) -> Self {
        ranges.retain(|(lo, hi)| lo <= hi);
        ranges.sort_unstable_by_key(|&(lo, _)| lo);
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (lo, hi) in ranges {
            match out.last_mut() {
                Some(last) if lo <= last.1.saturating_add(1) => {
                    last.1 = max(last.1, hi);
                }
                _ => out.push((lo, hi)),
            }
        }
        Self { ranges: out }
    }

    /// True iff `c` is in the set.  Linear scan; expected range
    /// counts are small (well under 100 for typical XSD classes),
    /// and a tight branch chain beats binary search at that size.
    pub fn contains(&self, c: char) -> bool {
        let cp = c as u32;
        // Most classes are small enough that a binary search adds
        // more overhead than it saves.  Hoist a binary search if
        // a class crosses the cutover threshold.
        if self.ranges.len() < 16 {
            self.ranges.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
        } else {
            match self.ranges.binary_search_by(|&(lo, hi)| {
                if cp < lo      { std::cmp::Ordering::Greater }
                else if cp > hi { std::cmp::Ordering::Less }
                else            { std::cmp::Ordering::Equal }
            }) {
                Ok(_) => true,
                Err(_) => false,
            }
        }
    }

    /// Borrowed view of the underlying ranges.
    pub fn ranges(&self) -> &[(u32, u32)] { &self.ranges }

    /// Set union.  Both inputs must already be canonical.
    pub fn union(&self, other: &Self) -> Self {
        let mut out: Vec<(u32, u32)> = Vec::with_capacity(
            self.ranges.len() + other.ranges.len()
        );
        let (mut i, mut j) = (0usize, 0usize);
        loop {
            let r = match (self.ranges.get(i), other.ranges.get(j)) {
                (Some(a), Some(b)) =>
                    if a.0 <= b.0 { i += 1; *a } else { j += 1; *b },
                (Some(a), None)    => { i += 1; *a }
                (None,    Some(b)) => { j += 1; *b }
                (None,    None)    => break,
            };
            match out.last_mut() {
                Some(last) if r.0 <= last.1.saturating_add(1) => {
                    last.1 = max(last.1, r.1);
                }
                _ => out.push(r),
            }
        }
        Self { ranges: out }
    }

    /// Set difference: `self - other`.  XSD §F.1.5 class subtraction.
    pub fn subtract(&self, other: &Self) -> Self {
        let mut out = Vec::new();
        let mut j = 0usize;
        for &(mut a_lo, a_hi) in &self.ranges {
            while a_lo <= a_hi {
                // Skip subtrahend ranges that end before `a_lo`.
                while j < other.ranges.len() && other.ranges[j].1 < a_lo {
                    j += 1;
                }
                let Some(&(b_lo, b_hi)) = other.ranges.get(j) else {
                    out.push((a_lo, a_hi));
                    break;
                };
                if b_lo > a_hi {
                    out.push((a_lo, a_hi));
                    break;
                }
                if b_lo > a_lo {
                    out.push((a_lo, b_lo - 1));
                }
                if b_hi >= a_hi { break; }
                a_lo = b_hi + 1;
                j += 1;
            }
        }
        Self { ranges: out }
    }

    /// Set complement against the Unicode universe (excluding
    /// surrogates).
    pub fn complement(&self) -> Self {
        Self::universe().subtract(self)
    }
}

fn is_canonical(ranges: &[(u32, u32)]) -> bool {
    if ranges.is_empty() { return true; }
    if ranges[0].0 > ranges[0].1 { return false; }
    for w in ranges.windows(2) {
        let (_, prev_hi) = w[0];
        let (cur_lo, cur_hi) = w[1];
        if cur_lo > cur_hi { return false; }
        // Disjoint AND non-abutting.
        if cur_lo <= prev_hi.saturating_add(1) { return false; }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(ranges: &[(u32, u32)]) -> ClassSet {
        ClassSet::from_ranges(ranges.to_vec())
    }

    #[test]
    fn coalesces_abutting_ranges() {
        let s = cs(&[(1, 5), (6, 10)]);
        assert_eq!(s.ranges(), &[(1, 10)]);
    }

    #[test]
    fn coalesces_overlapping_ranges() {
        let s = cs(&[(1, 5), (3, 10)]);
        assert_eq!(s.ranges(), &[(1, 10)]);
    }

    #[test]
    fn keeps_disjoint_ranges() {
        let s = cs(&[(1, 5), (10, 20)]);
        assert_eq!(s.ranges(), &[(1, 5), (10, 20)]);
    }

    #[test]
    fn contains_basic() {
        let s = cs(&[('a' as u32, 'z' as u32)]);
        assert!(s.contains('a'));
        assert!(s.contains('m'));
        assert!(s.contains('z'));
        assert!(!s.contains('A'));
        assert!(!s.contains('{'));
    }

    #[test]
    fn union_merges() {
        let a = cs(&[(1, 5), (10, 20)]);
        let b = cs(&[(4, 12)]);
        assert_eq!(a.union(&b).ranges(), &[(1, 20)]);
    }

    #[test]
    fn subtract_basic() {
        // [a-z] - [aeiou]
        let a = cs(&[('a' as u32, 'z' as u32)]);
        let vowels: Vec<(u32, u32)> = "aeiou".chars()
            .map(|c| (c as u32, c as u32)).collect();
        let b = ClassSet::from_ranges(vowels);
        let diff = a.subtract(&b);
        assert!(!diff.contains('a'));
        assert!(!diff.contains('e'));
        assert!(!diff.contains('i'));
        assert!(!diff.contains('o'));
        assert!(!diff.contains('u'));
        assert!(diff.contains('b'));
        assert!(diff.contains('z'));
    }

    #[test]
    fn subtract_self_is_empty() {
        let a = cs(&[(1, 100)]);
        assert!(a.subtract(&a).ranges().is_empty());
    }

    #[test]
    fn subtract_disjoint_is_identity() {
        let a = cs(&[(1, 10)]);
        let b = cs(&[(20, 30)]);
        assert_eq!(a.subtract(&b).ranges(), a.ranges());
    }

    #[test]
    fn subtract_splits_range() {
        let a = cs(&[(1, 100)]);
        let b = cs(&[(40, 60)]);
        assert_eq!(a.subtract(&b).ranges(), &[(1, 39), (61, 100)]);
    }

    #[test]
    fn complement_excludes_surrogates() {
        let empty = ClassSet::empty();
        let all = empty.complement();
        assert_eq!(all.ranges(), &[(0, SUR_LO - 1), (SUR_HI + 1, CP_MAX)]);
    }

    #[test]
    fn complement_round_trip() {
        let a = cs(&[(1, 10), (20, 30)]);
        let round = a.complement().complement();
        assert_eq!(round, a);
    }
}
