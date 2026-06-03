//! Forward-only fast path for the common `[class]{quant}…` shape.
//!
//! Most real-world XSD patterns are a sequence of atoms (literal,
//! character class, escape shortcut) each with an optional
//! quantifier — `E\d{6}`, `[A-Z]{3}-\d{4}-[a-f0-9]{8}`,
//! `[a-z][a-z0-9_\-\.]*`.  The general Pike VM handles these
//! correctly but pays NFA dispatch overhead per codepoint;
//! [`LinearMatcher`] walks the input once with a tight `for` loop
//! and no allocation.
//!
//! To stay forward-only (no backtracking) we constrain unbounded
//! greedy quantifiers (`*`, `+`, `{n,}`) to the *last* item of the
//! pattern.  Anything else — alternation, grouped subexpressions,
//! quantifiers over a non-leaf body, or an unbounded quant followed
//! by more atoms — falls back to the Pike VM.

use super::class::ClassSet;
use super::parser::Expr;

/// Compiled linear pattern.  Built only if the AST fits the
/// forward-only subset; callers fall back to the NFA otherwise.
#[derive(Debug, Clone)]
pub struct LinearMatcher {
    items: Vec<Item>,
}

#[derive(Debug, Clone)]
struct Item {
    class: ClassSet,
    min:   u32,
    /// `None` = unbounded.  By the subset rule this can only be
    /// `None` on the last item.
    max:   Option<u32>,
}

impl LinearMatcher {
    /// Try to compile `ast` into a linear matcher.  Returns `None`
    /// if the AST uses constructs the fast path can't handle.
    pub fn try_build(ast: &Expr) -> Option<Self> {
        let mut items: Vec<Item> = Vec::new();
        collect(ast, &mut items)?;

        // Forward-only invariant: any unbounded greedy quantifier
        // must be the very last item.  Otherwise a following atom
        // could need characters the quant has already swallowed,
        // which the loop below can't give back.
        for (i, it) in items.iter().enumerate() {
            if it.max.is_none() && i + 1 != items.len() {
                return None;
            }
        }
        Some(Self { items })
    }

    /// Walk `s` once, advancing through each item in order.  XSD
    /// patterns are implicitly anchored: the whole input must be
    /// consumed.
    pub fn is_match(&self, s: &str) -> bool {
        let mut iter = s.chars().peekable();
        for item in &self.items {
            let max = item.max.unwrap_or(u32::MAX);
            let mut matched = 0u32;
            while matched < max {
                match iter.peek() {
                    Some(&c) if item.class.contains(c) => {
                        iter.next();
                        matched += 1;
                    }
                    _ => break,
                }
            }
            if matched < item.min { return false; }
        }
        iter.next().is_none()
    }
}

/// Flatten an AST into a linear item list, or fail.
fn collect(expr: &Expr, items: &mut Vec<Item>) -> Option<()> {
    match expr {
        Expr::Empty => Some(()),
        Expr::Class(c) => {
            items.push(Item { class: c.clone(), min: 1, max: Some(1) });
            Some(())
        }
        Expr::Quant(inner, min, max) => match inner.as_ref() {
            Expr::Class(c) => {
                items.push(Item { class: c.clone(), min: *min, max: *max });
                Some(())
            }
            // A quantifier over a Concat/Alt/Quant isn't linear —
            // the Pike VM handles those.
            _ => None,
        },
        Expr::Concat(parts) => {
            for p in parts {
                collect(p, items)?;
            }
            Some(())
        }
        Expr::Alt(_) => None,
        // Anchors are zero-width position assertions — the linear
        // fast path only knows about character runs, so route
        // anchored patterns through the full NFA.
        Expr::Anchor(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::parser::parse;

    fn try_linear(src: &str) -> Option<LinearMatcher> {
        LinearMatcher::try_build(&parse(src).unwrap())
    }

    #[test]
    fn accepts_literal_chain() {
        assert!(try_linear("abc").is_some());
    }

    #[test]
    fn accepts_class_with_bounded_quant() {
        assert!(try_linear(r"E\d{6}").is_some());
        assert!(try_linear(r"[A-Z]{3}-\d{4}-[a-f0-9]{8}").is_some());
    }

    #[test]
    fn accepts_trailing_unbounded() {
        assert!(try_linear(r"[a-z][a-z0-9_\-\.]*").is_some());
        assert!(try_linear(r"\d+").is_some());
    }

    #[test]
    fn rejects_unbounded_followed_by_atom() {
        // `\d+5` would require backtracking.
        assert!(try_linear(r"\d+5").is_none());
    }

    #[test]
    fn rejects_alternation() {
        assert!(try_linear("a|b").is_none());
    }

    #[test]
    fn rejects_grouped_subexpression() {
        // Even `(a)` shouldn't take the fast path — the parser
        // collapses `(a)` to a bare atom so it does, but `(ab)+`
        // is a quantifier over a Concat and should fall back.
        assert!(try_linear("(ab)+").is_none());
        assert!(try_linear(r"\d{5}(-\d{4})?").is_none());
    }

    #[test]
    fn matches_basic() {
        let m = try_linear(r"E\d{6}").unwrap();
        assert!(m.is_match("E000123"));
        assert!(!m.is_match("E00012"));
        assert!(!m.is_match("E0001234"));
        assert!(!m.is_match("X000123"));
    }

    #[test]
    fn matches_with_trailing_star() {
        let m = try_linear(r"[a-z][a-z0-9_\-\.]*").unwrap();
        assert!(m.is_match("org.example"));
        assert!(m.is_match("a"));
        assert!(!m.is_match("A"));
        assert!(!m.is_match(""));
    }
}
