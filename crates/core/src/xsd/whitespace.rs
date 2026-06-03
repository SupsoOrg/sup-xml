//! XSD whiteSpace facet — `preserve` / `replace` / `collapse`.
//!
//! Whitespace handling runs **before** any other facet check.  `xs:string`
//! defaults to `preserve`; `xs:normalizedString` to `replace`;
//! `xs:token` and most other types to `collapse`.

/// The three whitespace-handling modes defined by XSD §4.3.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhitespaceMode {
    /// Leave the lexical value as-is.
    Preserve,
    /// Replace each tab/CR/LF with a single space.  No collapsing.
    Replace,
    /// `Replace`, then collapse runs of spaces to one and trim leading and
    /// trailing spaces.
    Collapse,
}

impl WhitespaceMode {
    /// Apply this mode to a raw lexical value.
    pub fn apply<'a>(self, s: &'a str) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        match self {
            WhitespaceMode::Preserve => Cow::Borrowed(s),
            WhitespaceMode::Replace => {
                if s.bytes().any(|b| matches!(b, b'\t' | b'\n' | b'\r')) {
                    let out: String = s.chars()
                        .map(|c| if matches!(c, '\t' | '\n' | '\r') { ' ' } else { c })
                        .collect();
                    Cow::Owned(out)
                } else {
                    Cow::Borrowed(s)
                }
            }
            WhitespaceMode::Collapse => {
                // Replace + collapse + trim.  Done in one pass.
                let needs_work = s.bytes().any(|b| matches!(b, b'\t' | b'\n' | b'\r'))
                    || s.contains("  ")
                    || s.starts_with(' ')
                    || s.ends_with(' ');
                if !needs_work {
                    return Cow::Borrowed(s);
                }
                let mut out = String::with_capacity(s.len());
                let mut prev_space = true; // pretend we started after a space → trim leading
                for c in s.chars() {
                    let is_ws = matches!(c, ' ' | '\t' | '\n' | '\r');
                    if is_ws {
                        if !prev_space {
                            out.push(' ');
                            prev_space = true;
                        }
                    } else {
                        out.push(c);
                        prev_space = false;
                    }
                }
                if out.ends_with(' ') {
                    out.pop();
                }
                Cow::Owned(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_passes_through() {
        let s = "  a\tb\n  c  ";
        assert_eq!(WhitespaceMode::Preserve.apply(s), s);
    }

    #[test]
    fn replace_only_swaps_ws_chars() {
        assert_eq!(WhitespaceMode::Replace.apply("a\tb\nc"), "a b c");
        assert_eq!(WhitespaceMode::Replace.apply("  hello  "), "  hello  ");
    }

    #[test]
    fn collapse_normalizes() {
        assert_eq!(WhitespaceMode::Collapse.apply("  a   b\t\n c  "), "a b c");
        assert_eq!(WhitespaceMode::Collapse.apply("plain"), "plain");
        assert_eq!(WhitespaceMode::Collapse.apply(""), "");
        assert_eq!(WhitespaceMode::Collapse.apply("   "), "");
    }

    #[test]
    fn collapse_borrows_when_clean() {
        use std::borrow::Cow;
        let result = WhitespaceMode::Collapse.apply("clean");
        assert!(matches!(result, Cow::Borrowed("clean")));
    }
}
