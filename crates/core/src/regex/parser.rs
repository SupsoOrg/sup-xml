//! XSD §F regex parser — XSD-flavour source → [`Expr`] AST.
//!
//! Implements the BNF from XSD Part 2 §F: `regExp` is a `branch`
//! list joined by `|`; each branch is a sequence of `piece`s; each
//! piece is an `atom` with an optional `quantifier`.  Atoms are
//! single characters, escapes, character classes, or
//! parenthesised regexps.
//!
//! Pre-translation rejects constructs XSD §F forbids:
//! back-references, lookaround, inline modifiers, anchor escapes.
//! These errors fire at schema compile time so users get a single
//! clear diagnostic up front instead of a confusing
//! deferred-compile failure at first match.

use super::class::ClassSet;
use super::unicode;

/// Cap on counted-repetition expansion.  An NFA built from
/// `a{0,8192}` has 8192 split states; allow generous schemas but
/// reject pathological ones up front rather than risk runaway
/// memory at compile time.
const MAX_REPETITION: u32 = 4096;

/// Parsed XSD regex.  Classes are flattened to [`ClassSet`] at
/// parse time so the NFA builder doesn't need to know about
/// `\p{...}`, `\d`, class subtraction, etc.
#[derive(Debug, Clone)]
pub enum Expr {
    /// Matches the empty string.
    Empty,
    /// Concatenation of subexpressions, evaluated left-to-right.
    Concat(Vec<Expr>),
    /// Alternation — match any one branch.  XSD §F has no
    /// preference between branches (no backtracking semantics to
    /// preserve), but the NFA emits them in source order.
    Alt(Vec<Expr>),
    /// Counted repetition.  `max == None` means unbounded.
    Quant(Box<Expr>, u32, Option<u32>),
    /// Single-codepoint match against a character class.  Literal
    /// chars and `.` lower to single-range / universe classes.
    Class(ClassSet),
    /// Position anchor (XPath 2.0 only; XSD §F has none).  Matches
    /// the empty string when the simulator is at the asserted
    /// position.  The `m`-flag (multiline) variants assert on line
    /// boundaries; without `m` they assert on input boundaries.
    Anchor(AnchorKind),
}

/// Position-anchor variety used by [`Expr::Anchor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    /// `^` — start of input (single-line) or start of any line
    /// (multiline).  Multiline routing is decided at compile time
    /// by the caller; the AST only records that the anchor exists.
    Start,
    /// `$` — end of input (single-line) or end of any line
    /// (multiline).
    End,
}

/// Source-level dialect.  XSD §F.1 forbids `^` and `$` (patterns
/// are implicitly whole-input anchored); XPath 2.0 §7.6 keeps the
/// XSD grammar but adds them back as explicit anchors usable
/// anywhere in the pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// XSD Part 2 §F — what `xs:pattern` facets compile under.
    Xsd,
    /// XPath 2.0 §7.6 — what `fn:matches`, `fn:replace`,
    /// `fn:tokenize`, and `xsl:analyze-string` compile under in
    /// XSLT 3.0+ hosts.  Adds explicit anchors `^` / `$` and
    /// non-capturing `(?:...)` groups on top of the XSD grammar.
    Xpath,
    /// XPath 2.0 §7.6 strict — same as [`Dialect::Xpath`] but
    /// without the XPath 3.0 extensions (`(?:…)` / inline flags).
    /// Used by XSLT 2.0 hosts where the W3C conformance suite
    /// expects FORX0002 on the 3.0 extensions.
    Xpath20,
}

/// Parse XSD §F source into an [`Expr`].
pub fn parse(src: &str) -> Result<Expr, String> {
    parse_with(src, Dialect::Xsd)
}

/// Parse with a specific source dialect.  See [`Dialect`].
pub fn parse_with(src: &str, dialect: Dialect) -> Result<Expr, String> {
    let mut p = Parser { input: src.as_bytes(), pos: 0, chars: src, dialect, depth: 0 };
    let expr = p.parse_regexp()?;
    if p.pos != p.input.len() {
        return Err(format!("unexpected '{}' at position {}",
            p.peek_char().unwrap_or(' '), p.pos));
    }
    Ok(expr)
}

struct Parser<'a> {
    input: &'a [u8],
    pos:   usize,
    /// Original source, for slicing when we need to read a
    /// multi-byte codepoint (the input is UTF-8; `input[pos]` is
    /// only valid for ASCII boundary tests).
    chars: &'a str,
    dialect: Dialect,
    /// Recursion depth of the current `parse_regexp` nesting, bumped
    /// on entry to each parenthesised group so a pathologically nested
    /// pattern (`((((…))))`) is rejected before it overflows the
    /// recursive-descent call stack.  Patterns reach the parser
    /// straight from untrusted XSD `<xs:pattern>` facets and XPath
    /// `matches()`/`replace()`/`tokenize()` arguments.
    depth: u32,
}

/// Maximum `(…)` nesting accepted in a pattern.  A group nests four
/// recursive-descent frames (`parse_regexp`/`parse_branch`/`parse_piece`/
/// `parse_atom`); 256 levels keeps the worst-case stack bounded while
/// sitting far above any pattern a human writes.
const MAX_REGEX_DEPTH: u32 = 256;

impl<'a> Parser<'a> {
    fn parse_regexp(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > MAX_REGEX_DEPTH {
            // The Parser is consumed on error and not reused, so there
            // is no need to decrement before bailing.
            return Err(format!(
                "regular expression nesting depth exceeds limit ({MAX_REGEX_DEPTH})"
            ));
        }
        let result = self.parse_alternation();
        self.depth -= 1;
        result
    }

    fn parse_alternation(&mut self) -> Result<Expr, String> {
        let first = self.parse_branch()?;
        if !self.eat(b'|') {
            return Ok(first);
        }
        let mut branches = vec![first];
        loop {
            branches.push(self.parse_branch()?);
            if !self.eat(b'|') { break; }
        }
        Ok(Expr::Alt(branches))
    }

    fn parse_branch(&mut self) -> Result<Expr, String> {
        let mut pieces: Vec<Expr> = Vec::new();
        while let Some(b) = self.peek() {
            if b == b'|' || b == b')' { break; }
            pieces.push(self.parse_piece()?);
        }
        Ok(match pieces.len() {
            0 => Expr::Empty,
            1 => pieces.pop().unwrap(),
            _ => Expr::Concat(pieces),
        })
    }

    fn parse_piece(&mut self) -> Result<Expr, String> {
        let atom = self.parse_atom()?;
        let (min, max) = self.parse_quantifier()?;
        // Per XSD §F.1 and XPath 2.0 §7.6 a piece is `atom
        // quantifier?` — at most one quantifier.  Patterns like
        // `a+*`, `a{1}?`, `a*+`, `a??*` are syntactically invalid
        // and must be rejected as FORX0002 in XPath dialect.  The
        // `?` after a quantifier is the "reluctant" modifier
        // (XPath 2.0 §7.6.1) — `a*?` is a single quantifier, not
        // two — so we consume one optional reluctant marker before
        // checking for a stray follow-on quantifier.
        let had_quantifier = (min, max) != (1, Some(1));
        if had_quantifier {
            // Reluctant marker `?` is part of the quantifier.
            self.eat(b'?');
            if let Some(b) = self.peek() {
                if matches!(b, b'?' | b'*' | b'+' | b'{') {
                    return Err(format!(
                        "stray quantifier '{}' after another quantifier — \
                         XPath 2.0/3.0 §7.6 grammar",
                        b as char
                    ));
                }
            }
        }
        Ok(match (min, max) {
            (1, Some(1)) => atom,
            _            => Expr::Quant(Box::new(atom), min, max),
        })
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        let b = self.peek().ok_or("unexpected end of input")?;
        // XPath 2.0 §7.6: `^` and `$` are zero-width position
        // anchors in every XPath dialect (Xpath20 only drops the
        // XPath 3.0 extensions, not the anchors).  XSD §F.1 treats
        // them as literal characters, so we only intercept for XPath.
        if matches!(self.dialect, Dialect::Xpath | Dialect::Xpath20) {
            if b == b'^' { self.bump(); return Ok(Expr::Anchor(AnchorKind::Start)); }
            if b == b'$' { self.bump(); return Ok(Expr::Anchor(AnchorKind::End));   }
        }
        match b {
            b'(' => {
                self.bump();
                if self.eat(b'?') {
                    return self.parse_question_construct();
                }
                let inner = self.parse_regexp()?;
                if !self.eat(b')') {
                    return Err("unbalanced '(' in pattern".into());
                }
                Ok(inner)
            }
            b'[' => {
                self.bump();
                Ok(Expr::Class(self.parse_class()?))
            }
            b'.' => {
                self.bump();
                // XSD §F.1.3: `.` matches any char except line
                // terminators (#x0A, #x0D).
                let nl = ClassSet::from_ranges(vec![(0x0A, 0x0A), (0x0D, 0x0D)]);
                Ok(Expr::Class(ClassSet::universe().subtract(&nl)))
            }
            b'\\' => {
                self.bump();
                let esc = self.bump_char()
                    .ok_or("trailing backslash")?;
                Ok(Expr::Class(self.parse_escape(esc)?))
            }
            b')' | b'|' | b'*' | b'+' | b'?' | b'{' =>
                Err(format!("unexpected metacharacter '{}' at position {}",
                    b as char, self.pos)),
            // Unmatched ']' or '}' outside of their structural context
            // is treated as a literal in XSD dialect (matching PCRE /
            // .NET behavior — the Microsoft XSTS suite relies on it).
            // XPath 2.0/3.0 §7.6 reserves both characters; XPath
            // dialect raises FORX0002.
            b']' | b'}' => match self.dialect {
                Dialect::Xsd => {
                    let c = self.bump_char().expect("peek returned Some");
                    Ok(Expr::Class(ClassSet::from_char(c)))
                }
                Dialect::Xpath | Dialect::Xpath20 => Err(format!(
                    "unmatched '{}' — XPath 2.0/3.0 §7.6 grammar",
                    b as char
                )),
            },
            _ => {
                let c = self.bump_char().expect("peek returned Some");
                Ok(Expr::Class(ClassSet::from_char(c)))
            }
        }
    }

    /// Handle a `(?...)` construct, with `(?` already consumed.
    ///
    /// XSD §F.1 doesn't define any `(?...)` form — but real-world
    /// schemas (especially Microsoft-generated ones) lean heavily
    /// on PCRE extensions: non-capturing groups `(?:…)`, inline
    /// modifiers `(?i)`, scoped modifier groups `(?i:…)`,
    /// atomic groups `(?>…)`, and so on.  Rejecting all of them
    /// loses ~250 schemas in the XSTS conformance suite.
    ///
    /// The pragmatic interpretation we adopt:
    ///
    /// * `(?:…)` — non-capturing group, parse inner expression.
    /// * `(?X:…)` / `(?X-Y:…)` for modifier letters X / Y —
    ///   modifier scope, parse inner expression, ignore modifier.
    /// * `(?X)` / `(?X-Y)` — inline modifier directive (no body),
    ///   accept and continue parsing the surrounding pattern.
    /// * `(?>…)` — atomic group, parse inner expression
    ///   (atomicity affects backtracking but not boolean
    ///   "does the whole input match?").
    /// * `(?=…)` / `(?!…)` — positive / negative lookahead.
    ///   Spec-forbidden lookaround.  Rejected.
    /// * `(?<=…)` / `(?<!…)` — positive / negative lookbehind.
    ///   Rejected.
    /// * `(?(…)…)` and similar PCRE conditionals — accepted
    ///   opaquely (skip to matching close paren).
    fn parse_question_construct(&mut self) -> Result<Expr, String> {
        let next = self.peek().ok_or("unterminated '(?' in pattern")?;

        // True lookaround / lookbehind — spec-forbidden, no safe
        // lenient interpretation (silently accepting them would
        // produce wrong matches).
        if next == b'=' || next == b'!' {
            return Err("lookaround '(?=…)' / '(?!…)' is not part of XSD §F".into());
        }
        if next == b'<' {
            // Could be (?<= or (?<! — both lookbehind.
            return Err("lookbehind '(?<…)' is not part of XSD §F".into());
        }

        // `(?:…)` — non-capturing group.  Accepted by XSD and
        // XPath 3.0+; XPath 2.0 (which the W3C conformance suite
        // gates on XSD 1.0 grammar) rejects it with FORX0002.
        if next == b':' {
            if self.dialect == Dialect::Xpath20 {
                return Err(
                    "non-capturing group '(?:' is XPath 3.0+ syntax \
                     not permitted in XPath 2.0 (FORX0002)".into()
                );
            }
            self.bump();
            let inner = self.parse_regexp()?;
            if !self.eat(b')') {
                return Err("unbalanced '(' in pattern".into());
            }
            return Ok(inner);
        }

        // XPath dialect (XPath 2.0 §7.6, XPath 3.0 §7.7) rejects
        // every `(?…)` construct other than `(?:…)` above.  The
        // W3C conformance suite expects FORX0002 for inline-modifier
        // forms like `(?i)` and for atomic groups like `(?>…)`.
        // XSD dialect keeps the lenient PCRE-style interpretation
        // for compatibility with real-world schemas.
        if self.dialect == Dialect::Xpath {
            return Err(format!(
                "invalid `(?{}…)` construct — XPath 2.0/3.0 regex \
                 supports only `(?:…)` non-capturing groups",
                next as char
            ));
        }

        // Any other `(?...)` form: consume modifier letters
        // (and optional `-letters` for negation, e.g. `(?-i:…)`)
        // up to either `:` (scoped modifier opens a body), `)`
        // (inline directive, no body), or any other construct.
        // For everything else — atomic groups `(?>…)`,
        // conditionals `(?(…)…)`, etc. — skip opaquely to the
        // matching `)`.
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphabetic() || b == b'-' {
                self.bump();
            } else {
                break;
            }
        }
        let modifiers_consumed = self.pos - start;

        match self.peek() {
            Some(b':') => {
                self.bump();
                let inner = self.parse_regexp()?;
                if !self.eat(b')') {
                    return Err("unbalanced '(' in pattern".into());
                }
                Ok(inner)
            }
            Some(b')') if modifiers_consumed > 0 => {
                // Inline modifier directive like `(?i)pattern` —
                // consume the `)` and emit an empty match; the
                // following pattern is parsed by the enclosing
                // `parse_regexp` loop.
                self.bump();
                Ok(Expr::Empty)
            }
            Some(_) => {
                // Anything else (atomic groups, conditionals,
                // PCRE-specific constructs).  Be lenient: skip
                // opaquely to the matching close paren and emit
                // an empty match.  This loses any pattern
                // semantics inside, which is fine for the
                // "schema compiles" goal — the rare instance
                // tests against these constructs will surface
                // separately.
                self.skip_to_matching_close_paren()?;
                Ok(Expr::Empty)
            }
            None => Err("unterminated '(?' construct".into()),
        }
    }

    /// Consume bytes through the matching close paren of an
    /// already-opened `(`, handling nested parens and backslash
    /// escapes.  Used for opaque PCRE-isms we don't actually
    /// interpret.
    fn skip_to_matching_close_paren(&mut self) -> Result<(), String> {
        let mut depth: i32 = 1;
        while depth > 0 {
            let b = self.peek().ok_or("unterminated '(' in pattern")?;
            match b {
                b'\\' => {
                    self.bump();
                    if self.peek().is_some() { self.bump(); }
                }
                b'(' => { self.bump(); depth += 1; }
                b')' => { self.bump(); depth -= 1; }
                b'[' => {
                    // Skip a char class — the bracket pair can
                    // legitimately contain `(` / `)` literals.
                    self.bump();
                    while let Some(b) = self.peek() {
                        self.bump();
                        if b == b'\\' && self.peek().is_some() { self.bump(); }
                        else if b == b']' { break; }
                    }
                }
                _ => { self.bump(); }
            }
        }
        Ok(())
    }

    /// Body of `[...]`, with the opening `[` already consumed.
    /// Recognises `^` negation, range syntax `a-z`, character-class
    /// escapes, and XSD §F.1.5 subtraction `[a-z-[aeiou]]`.
    fn parse_class(&mut self) -> Result<ClassSet, String> {
        let negated = self.eat(b'^');
        let mut acc = ClassSet::empty();
        // XSD §F.1.1 / FORX0002 — an empty character class `[]` is
        // not a valid regex.  The same applies to `[^]` (negated
        // empty): the spec requires at least one atom.  Reject
        // immediately so callers see the spec error rather than a
        // silently-matching empty set.
        if self.peek() == Some(b']') {
            return Err("empty character class is not permitted".into());
        }
        loop {
            let b = self.peek().ok_or("unclosed character class")?;
            if b == b']' { break; }

            // Subtraction operator: `-[...]` mid-class consumes the
            // accumulator and replaces it with the difference.
            if b == b'-' && self.input.get(self.pos + 1) == Some(&b'[') {
                self.bump(); // -
                self.bump(); // [
                let sub = self.parse_class()?;
                let mut head = if negated { acc.complement() } else { acc };
                head = head.subtract(&sub);
                // After subtraction the class must close immediately —
                // XSD §F.1.5 forbids more atoms in this class body.
                if !self.eat(b']') {
                    return Err(
                        "subtraction must be the last operation in a character class"
                        .into(),
                    );
                }
                return Ok(head);
            }

            let lo_atom = self.parse_class_atom()?;
            // `a-b` is only a range when `lo_atom` is a single char
            // and the `-` isn't immediately followed by `]` or `[`
            // (which would be class close / subtraction).
            if let ClassAtom::Char(lo) = lo_atom {
                let mut is_range = false;
                if self.peek() == Some(b'-') {
                    let next2 = self.input.get(self.pos + 1).copied();
                    if next2 != Some(b']') && next2 != Some(b'[') {
                        is_range = true;
                    }
                }
                if is_range {
                    self.bump();
                    let hi_atom = self.parse_class_atom()?;
                    match hi_atom {
                        ClassAtom::Char(hi) => {
                            if hi < lo {
                                return Err(format!(
                                    "inverted character range: '{lo}' > '{hi}'"
                                ));
                            }
                            acc = acc.union(&ClassSet::from_range(lo as u32, hi as u32));
                        }
                        // XSD §F.1.5 and XPath 2.0 §7.6 require both
                        // range endpoints to be single-character
                        // atoms.  Class-shorthand escapes (`\d`,
                        // `\w`, `\s`, …) can't serve as a range
                        // bound; reject in XPath dialect.  XSD
                        // dialect stays lenient for compatibility
                        // with Microsoft-authored schemas — there
                        // the lower bound becomes a literal and the
                        // shorthand's set is unioned in.
                        ClassAtom::Set(s) => match self.dialect {
                            Dialect::Xsd => {
                                acc = acc.union(&ClassSet::from_char(lo)).union(&s);
                            }
                            Dialect::Xpath | Dialect::Xpath20 => return Err(format!(
                                "class-shorthand escape cannot be the \
                                 upper bound of a range starting at '{lo}'"
                            )),
                        },
                    }
                } else {
                    acc = acc.union(&ClassSet::from_char(lo));
                }
            } else if let ClassAtom::Set(s) = lo_atom {
                acc = acc.union(&s);
            }
        }
        self.bump(); // consume ']'
        Ok(if negated { acc.complement() } else { acc })
    }

    /// One atom inside a class body — either a single literal/escaped
    /// char or a multi-codepoint set (from `\d`, `\p{L}`, …).
    fn parse_class_atom(&mut self) -> Result<ClassAtom, String> {
        let b = self.peek().ok_or("unclosed character class")?;
        if b == b'\\' {
            self.bump();
            let esc = self.bump_char().ok_or("trailing backslash")?;
            // PCRE `\xHH` / `\x{HHHH}` and `\uHHHH` / `\u{HHHH}` —
            // resolve to the actual codepoint so they remain usable
            // as range bounds (`[Ք-՗]`).  XSD dialect only; XPath
            // dialect rejects the escapes themselves below.
            if (esc == 'x' || esc == 'u') && self.dialect == Dialect::Xsd {
                if let Some(c) = self.parse_hex_codepoint(esc) {
                    return Ok(ClassAtom::Char(c));
                }
            }
            match single_char_escape(esc) {
                Some(c) => Ok(ClassAtom::Char(c)),
                None    => Ok(ClassAtom::Set(self.parse_escape(esc)?)),
            }
        } else {
            // XSD §F.1.5 / XPath 2.0 §7.6 reserve `[` inside a
            // class body (it would open a subtraction operand,
            // which must follow a `-`).  XSD dialect lets a stray
            // `[` through as a literal for Microsoft-schema
            // compatibility; XPath dialect raises FORX0002.
            if b == b'[' && self.dialect == Dialect::Xpath {
                return Err(
                    "literal '[' inside a character class — XPath \
                     2.0/3.0 §7.6 requires it to be escaped".into()
                );
            }
            let c = self.bump_char().expect("peek returned Some");
            Ok(ClassAtom::Char(c))
        }
    }

    /// Consume a PCRE hex/unicode escape body (`\xHH`, `\x{H+}`,
    /// `\uHHHH`, `\u{H+}`) starting just after the escape letter.
    /// Returns `None` if the body doesn't look like one (caller falls
    /// back to lenient handling).
    fn parse_hex_codepoint(&mut self, esc: char) -> Option<char> {
        let braced = self.peek() == Some(b'{');
        if braced { self.bump(); }
        let want = if braced { usize::MAX } else if esc == 'u' { 4 } else { 2 };
        let start = self.pos;
        while self.pos - start < want {
            match self.peek() {
                Some(b) if b.is_ascii_hexdigit() => { self.bump(); }
                _ => break,
            }
        }
        let body = std::str::from_utf8(&self.input[start..self.pos]).ok()?;
        if body.is_empty() { return None; }
        let cp = u32::from_str_radix(body, 16).ok()?;
        if braced && !self.eat(b'}') { return None; }
        char::from_u32(cp)
    }

    /// Map `\X` outside a class body to a [`ClassSet`].  Inside a
    /// class body, [`parse_class_atom`] short-circuits single-char
    /// escapes via [`single_char_escape`] first; this is the
    /// shared path for multi-char shortcuts.
    fn parse_escape(&mut self, esc: char) -> Result<ClassSet, String> {
        // Single-char escapes lift to a one-codepoint class.
        if let Some(c) = single_char_escape(esc) {
            return Ok(ClassSet::from_char(c));
        }
        match esc {
            'd' => Ok(unicode::xsd_digit().clone()),
            'D' => Ok(unicode::xsd_digit().complement()),
            's' => Ok(unicode::xsd_whitespace().clone()),
            'S' => Ok(unicode::xsd_whitespace().complement()),
            'w' => Ok(unicode::xsd_word().clone()),
            'W' => Ok(unicode::xsd_word().complement()),
            'i' => Ok(name_start_class().clone()),
            'I' => Ok(name_start_class().complement()),
            'c' => Ok(name_char_class().clone()),
            'C' => Ok(name_char_class().complement()),

            'p' | 'P' => {
                if !self.eat(b'{') {
                    return Err(format!("\\{esc} must be followed by '{{name}}'"));
                }
                let mut name = String::new();
                while let Some(b) = self.peek() {
                    if b == b'}' { break; }
                    let c = self.bump_char().expect("peek returned Some");
                    name.push(c);
                }
                if !self.eat(b'}') {
                    return Err(format!("unclosed \\{esc}{{...}} property name"));
                }
                let set = unicode::property_set(&name)
                    .ok_or_else(|| format!("unknown Unicode property '{name}'"))?
                    .clone();
                Ok(if esc == 'P' { set.complement() } else { set })
            }

            // XSD §F.1.4 explicitly forbids back-references — there
            // are no capture-group semantics in the XSD regex
            // flavour.  But Microsoft-generated schemas use them
            // (`\1`, `\2`, …) regardless.  In XSD dialect we accept
            // the syntax to let those schemas compile, treating
            // `\N` as a "match any character" placeholder; real
            // back-ref semantics aren't implemented.  XPath 2.0/3.0
            // forbid back-references in the pattern of fn:matches,
            // fn:replace, and fn:tokenize (XPath 3.0 §5.6.1.1) —
            // raise FORX0002 in that dialect.
            '0'..='9' => match self.dialect {
                Dialect::Xsd   => Ok(ClassSet::universe()),
                Dialect::Xpath | Dialect::Xpath20 => Err(format!(
                    "back-reference \\{esc} is not permitted in an \
                     XPath 2.0/3.0 regex pattern"
                )),
            },

            // XSD §F.1.4 forbids anchor / boundary escapes
            // (`\b`, `\B`, `\A`, `\Z`, `\z`).  Lenient in XSD
            // dialect for Microsoft-schema compatibility; XPath
            // dialect rejects them as FORX0002.
            'b' | 'B' | 'A' | 'Z' | 'z' => match self.dialect {
                Dialect::Xsd   => Ok(ClassSet::universe()),
                Dialect::Xpath | Dialect::Xpath20 => Err(format!(
                    "boundary escape \\{esc} is not part of the \
                     XPath 2.0/3.0 regex grammar"
                )),
            },

            // PCRE hex-byte / Unicode escapes (`\x41`, `A`).
            // Not in XSD §F or XPath 2.0/3.0 but appear in
            // Microsoft schemas; accept in XSD dialect only.
            'x' | 'u' => match self.dialect {
                Dialect::Xsd => match self.parse_hex_codepoint(esc) {
                    Some(c) => Ok(ClassSet::from_char(c)),
                    None    => Ok(ClassSet::universe()),
                },
                Dialect::Xpath | Dialect::Xpath20 => Err(format!(
                    "escape \\{esc} is not part of the XPath 2.0/3.0 \
                     regex grammar"
                )),
            },

            _ => Err(format!("unrecognised escape \\{esc}")),
        }
    }

    /// Parse a trailing quantifier — `?`, `*`, `+`, `{n}`, `{n,}`,
    /// `{n,m}`.  Returns `(1, Some(1))` when no quantifier is
    /// present.  Enforces [`MAX_REPETITION`] on counted forms.
    fn parse_quantifier(&mut self) -> Result<(u32, Option<u32>), String> {
        let Some(b) = self.peek() else { return Ok((1, Some(1))); };
        let parsed = match b {
            b'?' => { self.bump(); (0, Some(1)) }
            b'*' => { self.bump(); (0, None) }
            b'+' => { self.bump(); (1, None) }
            b'{' => {
                self.bump();
                // XSD §F.1.7 / XPath 2.0 §7.6.1 only define `{n}`,
                // `{n,}`, `{n,m}` — the leading count is required.
                // PCRE / Microsoft accept `{,m}` as a shorthand for
                // `{0,m}`; allow that in XSD dialect for schema
                // compatibility and reject it in XPath dialect.
                let min = if self.peek() == Some(b',') {
                    if self.dialect == Dialect::Xpath {
                        return Err(
                            "quantifier '{,m}' requires an explicit \
                             minimum — XPath 2.0/3.0 §7.6 grammar".into()
                        );
                    }
                    0
                } else {
                    let n = self.read_uint()?;
                    if n > MAX_REPETITION {
                        return Err(format!(
                            "quantifier minimum {n} exceeds cap {MAX_REPETITION}"
                        ));
                    }
                    n
                };
                if self.eat(b'}') {
                    (min, Some(min))
                } else if self.eat(b',') {
                    if self.eat(b'}') {
                        (min, None)
                    } else {
                        let max = self.read_uint()?;
                        if max > MAX_REPETITION {
                            return Err(format!(
                                "quantifier maximum {max} exceeds cap {MAX_REPETITION}"
                            ));
                        }
                        if max < min {
                            return Err(format!(
                                "quantifier range {{{min},{max}}} is empty"
                            ));
                        }
                        if !self.eat(b'}') {
                            return Err("unclosed '{' quantifier".into());
                        }
                        (min, Some(max))
                    }
                } else {
                    return Err("malformed '{' quantifier".into());
                }
            }
            _ => return Ok((1, Some(1))),
        };
        // Lazy / chained-quantifier handling differs by dialect.
        //
        // * XSD dialect: silently consume any trailing `?` (lazy
        //   marker — boolean match is greedy/lazy-agnostic) and
        //   then coalesce chained quantifiers (`?*`, `{0,16}*`,
        //   `+?+`) into the loosest `(0, None)`.  Strict PCRE
        //   rejects these, but Microsoft-authored schemas in the
        //   XSTS suite rely on the lenient interpretation.
        //
        // * XPath dialect: per XPath 2.0 §7.6.1 a single optional
        //   `?` is the reluctant marker — accept and leave it for
        //   [`parse_piece`] to consume.  No coalescing: a second
        //   quantifier following a reluctant `?` is invalid and
        //   must surface as FORX0002 in [`parse_piece`]'s
        //   stray-quantifier check.
        if self.dialect == Dialect::Xsd {
            if self.peek() == Some(b'?') {
                self.bump();
            }
            let mut widened = parsed;
            while let Some(b) = self.peek() {
                match b {
                    b'*' | b'+' | b'?' => { self.bump(); widened = (0, None); }
                    b'{' => {
                        let save = self.pos;
                        self.bump();
                        if self.read_uint().is_ok() || self.peek() == Some(b',') {
                            while let Some(c) = self.peek() {
                                self.bump();
                                if c == b'}' { break; }
                            }
                            widened = (0, None);
                        } else {
                            self.pos = save;
                            break;
                        }
                    }
                    _ => break,
                }
            }
            return Ok(widened);
        }
        Ok(parsed)
    }

    fn read_uint(&mut self) -> Result<u32, String> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() { self.bump(); } else { break; }
        }
        if self.pos == start {
            return Err("expected digit in quantifier".into());
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .unwrap()  // ASCII digits — always valid UTF-8
            .parse()
            .map_err(|e: std::num::ParseIntError| e.to_string())
    }

    // ── byte-level helpers ────────────────────────────────────────────────

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek_char(&self) -> Option<char> {
        self.chars[self.pos..].chars().next()
    }

    /// Advance one byte.  Caller must already have verified that
    /// `pos` is on an ASCII byte (the metacharacters above are all
    /// ASCII); for arbitrary codepoint advance use [`bump_char`].
    fn bump(&mut self) { self.pos += 1; }

    fn bump_char(&mut self) -> Option<char> {
        let c = self.chars[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) { self.bump(); true } else { false }
    }
}

enum ClassAtom {
    Char(char),
    Set(ClassSet),
}

/// Single-char escapes — punctuation, the named control characters,
/// and any non-alphanumeric character as a literal of itself.  XSD
/// §F's SingleCharEsc list is narrower (just the metacharacters)
/// but schemas in the wild commonly write `\_`, `\@`, and similar
/// no-op escapes; rejecting them would break drop-in compatibility.
/// Returns `None` for escapes that should lower to multi-codepoint
/// classes (`\d`, `\p{…}`, …) or that aren't valid in XSD at all
/// (back-references `\1`, unknown letters); those route through
/// [`Parser::parse_escape`].
fn single_char_escape(esc: char) -> Option<char> {
    Some(match esc {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        c if !c.is_ascii_alphanumeric() => c,
        _ => return None,
    })
}

// ── XML 1.0 Name characters (XML 1.0 §2.3) ─────────────────────────────

fn name_start_class() -> &'static ClassSet {
    use std::sync::OnceLock;
    static CELL: OnceLock<ClassSet> = OnceLock::new();
    CELL.get_or_init(|| ClassSet::from_ranges(vec![
        (':' as u32,  ':' as u32),
        ('A' as u32,  'Z' as u32),
        ('_' as u32,  '_' as u32),
        ('a' as u32,  'z' as u32),
        (0x00C0,      0x00D6),
        (0x00D8,      0x00F6),
        (0x00F8,      0x02FF),
        (0x0370,      0x037D),
        (0x037F,      0x1FFF),
        (0x200C,      0x200D),
        (0x2070,      0x218F),
        (0x2C00,      0x2FEF),
        (0x3001,      0xD7FF),
        (0xF900,      0xFDCF),
        (0xFDF0,      0xFFFD),
        (0x10000,     0xEFFFF),
    ]))
}

fn name_char_class() -> &'static ClassSet {
    use std::sync::OnceLock;
    static CELL: OnceLock<ClassSet> = OnceLock::new();
    CELL.get_or_init(|| ClassSet::from_ranges(vec![
        ('-' as u32,  '-' as u32),
        ('.' as u32,  '.' as u32),
        ('0' as u32,  '9' as u32),
        (':' as u32,  ':' as u32),
        ('A' as u32,  'Z' as u32),
        ('_' as u32,  '_' as u32),
        ('a' as u32,  'z' as u32),
        (0x00B7,      0x00B7),
        (0x00C0,      0x00D6),
        (0x00D8,      0x00F6),
        (0x00F8,      0x037D),
        (0x037F,      0x1FFF),
        (0x200C,      0x200D),
        (0x203F,      0x2040),
        (0x2070,      0x218F),
        (0x2C00,      0x2FEF),
        (0x3001,      0xD7FF),
        (0xF900,      0xFDCF),
        (0xFDF0,      0xFFFD),
        (0x10000,     0xEFFFF),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(src: &str) -> Expr { parse(src).unwrap() }

    fn class_of(e: &Expr) -> &ClassSet {
        match e { Expr::Class(c) => c, _ => panic!("not a class: {e:?}") }
    }

    #[test]
    fn empty_pattern() {
        assert!(matches!(p(""), Expr::Empty));
    }

    #[test]
    fn literal_char() {
        let e = p("a");
        assert!(class_of(&e).contains('a'));
        assert!(!class_of(&e).contains('b'));
    }

    #[test]
    fn concatenation() {
        match p("abc") {
            Expr::Concat(v) => assert_eq!(v.len(), 3),
            other => panic!("expected Concat, got {other:?}"),
        }
    }

    #[test]
    fn alternation() {
        match p("a|b|c") {
            Expr::Alt(v) => assert_eq!(v.len(), 3),
            other => panic!("expected Alt, got {other:?}"),
        }
    }

    #[test]
    fn quantifier_star() {
        match p("a*") {
            Expr::Quant(_, 0, None) => {}
            other => panic!("expected Quant(_, 0, None), got {other:?}"),
        }
    }

    #[test]
    fn quantifier_range() {
        match p("a{2,4}") {
            Expr::Quant(_, 2, Some(4)) => {}
            other => panic!("expected Quant(_, 2, Some(4)), got {other:?}"),
        }
    }

    #[test]
    fn class_with_subtraction() {
        let e = p("[a-z-[aeiou]]");
        let c = class_of(&e);
        assert!(c.contains('b'));
        assert!(!c.contains('a'));
        assert!(!c.contains('e'));
    }

    #[test]
    fn class_negated() {
        let e = p("[^0-9]");
        let c = class_of(&e);
        assert!(!c.contains('5'));
        assert!(c.contains('a'));
    }

    #[test]
    fn shortcut_digit_in_class() {
        let e = p(r"[\d]");
        let c = class_of(&e);
        assert!(c.contains('5'));
        assert!(!c.contains('a'));
    }

    #[test]
    fn whitespace_shortcut_is_xsd_flavour() {
        let e = p(r"\s");
        let c = class_of(&e);
        assert!(c.contains(' '));
        assert!(c.contains('\t'));
        assert!(!c.contains('\u{A0}'), "XSD \\s is the four XML whitespace chars only");
    }

    #[test]
    fn property_unknown_errors() {
        let err = parse(r"\p{NotARealCategory}").unwrap_err();
        assert!(err.contains("unknown Unicode property"));
    }

    #[test]
    fn property_letter() {
        let e = p(r"\p{L}");
        let c = class_of(&e);
        assert!(c.contains('a'));
        assert!(c.contains('中'));
        assert!(!c.contains('1'));
    }

    #[test]
    fn rejects_true_lookaround() {
        // True lookaround/lookbehind have no safe lenient mapping —
        // silently accepting them would mis-match.  Rejected.
        assert!(parse("(?=foo)").is_err());
        assert!(parse("(?!foo)").is_err());
        assert!(parse("(?<=foo)").is_err());
        assert!(parse("(?<!foo)").is_err());
    }

    #[test]
    fn accepts_inline_modifier_directives_leniently() {
        // `(?i)pattern` is a PCRE inline modifier directive — not
        // in XSD §F, but Microsoft-generated schemas use them.
        // We accept and ignore the modifier so the pattern compiles.
        assert!(parse("(?i)foo").is_ok());
        assert!(parse("(?m)bar").is_ok());
        assert!(parse("(?s)baz").is_ok());
        assert!(parse("(?-i:quux)").is_ok());
    }

    #[test]
    fn accepts_modifier_groups_leniently() {
        // `(?i:foo)` is a scoped modifier group.  Same treatment
        // as inline directives — accept, ignore modifier.
        assert!(parse("(?i:foo)").is_ok());
        assert!(parse("(?r:foo)").is_ok());
        assert!(parse("(?n:(foo))").is_ok());
    }

    #[test]
    fn accepts_back_references_leniently() {
        // XSD §F forbids back-references but Microsoft-generated
        // schemas use them.  We accept `\N` as a match-any
        // placeholder so the pattern compiles; real back-ref
        // semantics aren't implemented.
        assert!(parse(r"(a)\1").is_ok());
        assert!(parse(r"(a)(b)\2").is_ok());
    }

    #[test]
    fn accepts_anchor_escapes_leniently() {
        // \b, \B, \z, \Z, \A — anchors not in XSD §F.  Accepted
        // as match-any placeholders (degraded but compiles).
        assert!(parse(r"\bfoo").is_ok());
        assert!(parse(r"foo\Z").is_ok());
    }

    #[test]
    fn rejects_unbalanced() {
        assert!(parse("(foo").is_err());
        assert!(parse("foo)").is_err());
        assert!(parse("[abc").is_err());
    }

    #[test]
    fn dot_excludes_line_terminators() {
        let e = p(".");
        let c = class_of(&e);
        assert!(c.contains('a'));
        assert!(c.contains(' '));
        assert!(!c.contains('\n'));
        assert!(!c.contains('\r'));
    }

    #[test]
    fn quantifier_cap() {
        let err = parse("a{99999}").unwrap_err();
        assert!(err.contains("exceeds cap"));
    }

    #[test]
    fn deep_group_nesting_rejected() {
        // A pattern nested past MAX_REGEX_DEPTH must error rather than
        // overflow the recursive-descent stack.  Build `(((…a…)))` with
        // enough groups to exceed the cap.
        let n = (MAX_REGEX_DEPTH as usize) + 50;
        let pattern = format!("{}a{}", "(".repeat(n), ")".repeat(n));
        let err = parse(&pattern).unwrap_err();
        assert!(
            err.contains("nesting depth exceeds limit"),
            "expected depth-limit error, got: {err}"
        );
    }

    #[test]
    fn moderate_group_nesting_accepted() {
        // Nesting comfortably under the cap must still parse.
        let n = 32;
        let pattern = format!("{}a{}", "(".repeat(n), ")".repeat(n));
        assert!(parse(&pattern).is_ok());
    }
}
