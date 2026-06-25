//! Thompson's construction — [`Expr`] AST → flat NFA [`Program`].
//!
//! The NFA is two arrays: a `states` vector (consuming `Char`
//! transitions or epsilon `Split`s, plus a single `Match` state)
//! and a `classes` table (deduplicated character classes referenced
//! by index from `Char` states).
//!
//! Quantifiers desugar to combinations of `Split` and inlined
//! copies of the body; the parser caps counted repetition so this
//! cannot blow up unboundedly.

use rustc_hash::FxHashMap;

use super::class::ClassSet;
use super::parser::{AnchorKind, Expr};

pub type StateId = u32;
pub type ClassId = u32;

/// One NFA state.  Field layout is kept dense (16 bytes on 64-bit)
/// so the VM's hot inner loop stays cache-friendly.
#[derive(Debug, Clone)]
pub enum State {
    /// Consume one codepoint matching `classes[class]`, then
    /// transition to `next`.
    Char { class: ClassId, next: StateId },
    /// Epsilon split — non-deterministic choice between two
    /// successor states.  Both edges are explored by the VM.
    Split(StateId, StateId),
    /// Zero-width position assertion.  Treated like an epsilon
    /// transition to `next` iff the simulator's current input
    /// position satisfies the assertion (start/end of input, with
    /// multiline variants asserting on line boundaries).
    Assert { kind: AnchorKind, next: StateId },
    /// Zero-width capture-slot write — records the current input
    /// position into `slot` of the matching thread, then transitions to
    /// `next` like an epsilon edge.  Group `g` uses slots `2g` (open) and
    /// `2g+1` (close); the whole match is group 0.  Only the capture-
    /// tracking VM acts on these; the boolean matchers treat them as
    /// plain epsilons.
    Save { slot: u32, next: StateId },
    /// Accept state.  Reaching this with no input remaining is a
    /// match (in anchored mode) or a match at any position (in
    /// find mode).
    Match,
}

/// Compiled NFA: states, the class table they reference, and the
/// entry state.
#[derive(Debug, Clone)]
pub struct Program {
    pub states:  Vec<State>,
    pub classes: Vec<ClassSet>,
    pub start:   StateId,
    /// `i` flag — a class matches the simple case-folds of the input
    /// codepoint as well (decided at match time so large classes aren't
    /// expanded).
    pub case_insensitive: bool,
    /// `m` flag — `^` / `$` anchors assert at line boundaries (after /
    /// before a newline) in addition to the input ends.
    pub multiline: bool,
    /// Number of capture slots = `2 * (highest group number + 1)`.  Slots
    /// 0/1 are the whole match; group `g` is `2g`/`2g+1`.
    pub num_slots: u32,
}

/// Compile an AST to a runnable [`Program`].
pub fn compile(ast: &Expr) -> Result<Program, String> {
    let mut b = Builder {
        states:  Vec::new(),
        classes: Vec::new(),
        intern:  FxHashMap::default(),
        max_group: 0,
    };
    // Reserve state 0 for `Match`; every fragment's dangling
    // out-edges get patched to point here.
    let match_state = b.add(State::Match);
    let frag = b.lower(ast, match_state);
    Ok(Program {
        start:   frag.entry,
        states:  b.states,
        classes: b.classes,
        case_insensitive: false,
        multiline: false,
        num_slots: 2 * (b.max_group + 1),
    })
}

/// Compile-time NFA fragment — one entry state and a single
/// successor patched on completion (the `out` placeholder).
struct Frag {
    entry: StateId,
}

struct Builder {
    states:  Vec<State>,
    classes: Vec<ClassSet>,
    /// Hash-cons identical class sets so `[a-z]` appearing twice
    /// in the same pattern shares one slot.  Keyed by the canonical
    /// range list.
    intern:  FxHashMap<Vec<(u32, u32)>, ClassId>,
    /// Highest capturing-group number lowered so far.
    max_group: u32,
}

impl Builder {
    fn add(&mut self, s: State) -> StateId {
        let id = self.states.len() as StateId;
        self.states.push(s);
        id
    }

    fn intern_class(&mut self, set: ClassSet) -> ClassId {
        if let Some(&id) = self.intern.get(set.ranges()) {
            return id;
        }
        let id = self.classes.len() as ClassId;
        let key = set.ranges().to_vec();
        self.classes.push(set);
        self.intern.insert(key, id);
        id
    }

    /// Lower `ast` so it transitions to `out` on success.
    fn lower(&mut self, ast: &Expr, out: StateId) -> Frag {
        match ast {
            Expr::Empty => Frag { entry: out },

            Expr::Class(set) => {
                let class = self.intern_class(set.clone());
                let entry = self.add(State::Char { class, next: out });
                Frag { entry }
            }

            Expr::Concat(parts) => {
                // Build right-to-left so each fragment's `out` is
                // the next fragment's entry.
                let mut next = out;
                for p in parts.iter().rev() {
                    next = self.lower(p, next).entry;
                }
                Frag { entry: next }
            }

            Expr::Alt(branches) => {
                if branches.is_empty() {
                    return Frag { entry: out };
                }
                let mut iter = branches.iter().rev();
                let last = iter.next().unwrap();
                let mut acc = self.lower(last, out).entry;
                for branch in iter {
                    let b_entry = self.lower(branch, out).entry;
                    acc = self.add(State::Split(b_entry, acc));
                }
                Frag { entry: acc }
            }

            Expr::Quant(body, min, max, greedy) =>
                self.lower_quant(body, *min, *max, *greedy, out),

            Expr::Anchor(kind) => {
                let entry = self.add(State::Assert { kind: *kind, next: out });
                Frag { entry }
            }

            Expr::Group(body, idx) => {
                if *idx > self.max_group { self.max_group = *idx; }
                // open-save ─► body ─► close-save ─► out
                let close = self.add(State::Save { slot: 2 * idx + 1, next: out });
                let body_entry = self.lower(body, close).entry;
                let open = self.add(State::Save { slot: 2 * idx, next: body_entry });
                Frag { entry: open }
            }
        }
    }

    fn lower_quant(
        &mut self,
        body:   &Expr,
        min:    u32,
        max:    Option<u32>,
        greedy: bool,
        out:    StateId,
    ) -> Frag {
        // A split offers two edges; the VM explores the first with higher
        // priority.  Greedy quantifiers try the body (more repetitions)
        // first; reluctant (`…?`) ones try the exit first.
        let order = |body_edge: StateId, exit_edge: StateId| -> State {
            if greedy { State::Split(body_edge, exit_edge) }
            else      { State::Split(exit_edge, body_edge) }
        };
        // Tail: the optional / unbounded portion that follows the
        // `min` mandatory copies.
        let tail = match max {
            None => {
                // `{min,}` — append `body*` to the chain.  Use a
                // back-edge from the body's exit to the loop's
                // entry split.
                //
                //   ┌────────────────┐
                //   │                ▼
                //  split ──► body ──┘
                //   │
                //   └──► out
                //
                // We need the split state to exist before we can
                // lower the body (so the body's `out` can point to
                // it), so allocate a placeholder and patch.
                let split_id = self.add(State::Split(0, out));
                let body_entry = self.lower(body, split_id).entry;
                self.states[split_id as usize] = order(body_entry, out);
                split_id
            }
            Some(m) if m == min => out,
            Some(m) => {
                // `{min,m}` — `m - min` optional copies, each a
                // split that can skip directly to `out`.
                let mut cur = out;
                for _ in 0..(m - min) {
                    let body_frag = self.lower(body, cur);
                    cur = self.add(order(body_frag.entry, out));
                }
                cur
            }
        };
        // `min` mandatory copies prepended onto the tail.
        let mut entry = tail;
        for _ in 0..min {
            entry = self.lower(body, entry).entry;
        }
        Frag { entry }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::parser::parse;

    fn build(src: &str) -> Program {
        compile(&parse(src).unwrap()).unwrap()
    }

    #[test]
    fn empty_pattern_is_one_match_state() {
        let p = build("");
        assert!(matches!(p.states[p.start as usize], State::Match));
    }

    #[test]
    fn literal_has_char_into_match() {
        let p = build("a");
        let State::Char { next, .. } = p.states[p.start as usize] else {
            panic!("expected Char start");
        };
        assert!(matches!(p.states[next as usize], State::Match));
    }

    #[test]
    fn alternation_emits_splits() {
        let p = build("a|b|c");
        // Three alts produce two Splits at the front.
        let split_count = p.states.iter().filter(|s| matches!(s, State::Split(..))).count();
        assert_eq!(split_count, 2);
    }

    #[test]
    fn class_dedup() {
        // `[a-z]` appearing twice should share one class table entry.
        let p = build("[a-z][a-z]");
        assert_eq!(p.classes.len(), 1);
    }

    #[test]
    fn star_emits_one_split() {
        let p = build("a*");
        assert!(p.states.iter().filter(|s| matches!(s, State::Split(..))).count() >= 1);
    }
}
