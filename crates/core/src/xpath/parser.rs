//! XPath grammar driver — recursive-descent over the token stream
//! produced by [`super::lexer`].
//!
//! ## UTF-8 slicing discipline
//!
//! Several diagnostic paths in this module slice the source string by
//! byte offset to quote the failing token in error messages.  Byte
//! offsets that land inside a multi-byte UTF-8 code point panic via
//! `core::str::slice_error_fail` — a real bug the fuzzer found in
//! the error-snippet truncation logic (`(e - s).min(40)`).
//!
//! To prevent regressions, this file opts in to the `restriction`
//! lint [`clippy::string_slice`], which flags every `&str[a..b]`
//! operation.  Each safe site below carries an `#[allow(...)]` with
//! a comment explaining why the indices are guaranteed to lie on
//! char boundaries — typically because they came from
//! [`str::char_indices`] or the lexer's span table (whose endpoints
//! are always set just after a complete codepoint).
#![warn(clippy::string_slice)]

use super::ast::*;
use super::lexer::Token;
use crate::error::{ErrorDomain, ErrorLevel, XmlError};

type Result<T> = std::result::Result<T, XmlError>;

fn parse_err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

const NODE_TYPES: &[&str] = &[
    "node", "text", "comment", "processing-instruction",
    // XPath 2.0 §2.5.4 KindTest forms.  These also need to be
    // recognised at step-start so the parser sees `element()` /
    // `attribute()` / `document-node()` as kind tests rather than
    // function calls.  schema-element / schema-attribute too.
    "element", "attribute", "document-node",
    "schema-element", "schema-attribute",
];

fn is_node_type(name: &str) -> bool {
    NODE_TYPES.contains(&name)
}

fn is_name_tok(tok: &Token, name: &str) -> bool {
    matches!(tok, Token::Name(n) if n == name)
}

/// True when `tok` is one of the XPath 2.0 word-form comparison
/// operators (`eq` / `ne` / `lt` / `gt` / `le` / `ge` for value
/// comparison, and `is` for node-identity comparison) named in
/// `name`.  The parser uses this in operator positions only —
/// lookup-by-position rules let the same names appear as element /
/// function identifiers in other contexts.
fn is_value_op_kw(tok: &Token, name: &str) -> bool {
    debug_assert!(matches!(name, "eq" | "ne" | "lt" | "gt" | "le" | "ge" | "is"));
    matches!(tok, Token::Name(n) if n == name)
}

/// Cap on the kind of nesting that actually grows the stack: each
/// `(…)`, `[…]`, and function-call argument list re-enters the
/// recursive-descent precedence chain (`parse_or` → `parse_and` →
/// … → `parse_primary`).  The depth counter is bumped at exactly
/// those re-entry points so `MAX_PARSE_DEPTH` corresponds to the
/// number of such nestings, not to any one parser helper's
/// re-entry count.
const MAX_PARSE_DEPTH: u32 = 64;

/// Parse a `SequenceType` from its lexical form (`"xs:long"`,
/// `"element(e)"`, `"function(*)?"`, …).  Used to reconstruct a user
/// function's declared signature for function-subtyping checks.  Returns
/// `None` on a lex/parse error or trailing input.
pub fn parse_sequence_type_str(src: &str)
    -> Option<crate::xpath::ast::SequenceType>
{
    let tokens = crate::xpath::lexer::tokenize_only(src).ok()?;
    let mut p = Parser::new(tokens);
    p.xpath_2_0 = true;
    let st = p.parse_sequence_type().ok()?;
    (p.peek() == &Token::Eof).then_some(st)
}

pub struct Parser {
    tokens: Vec<Token>,
    /// Source byte ranges for each `tokens[i]`.  Same length as
    /// `tokens` when populated by [`Parser::new_with_spans`];
    /// otherwise empty (older callers that hand-build token lists
    /// for tests skip span tracking — error messages still work,
    /// just without column info).  Used by [`Parser::error`] to
    /// point messages at the offending substring.
    spans:  Vec<crate::xpath::lexer::Span>,
    /// Original expression source.  Needed alongside `spans` to
    /// reconstruct the lexeme for diagnostic messages.  Empty
    /// when no source is available (test-only path).
    src:    String,
    pos: usize,
    /// Tracks how many `parse_expr` frames are currently on the
    /// call stack — checked against [`MAX_PARSE_DEPTH`] on each
    /// recursive re-entry to prevent stack overflow.
    depth: u32,
    /// XPath 2.0 grammar extensions enabled (`if-then-else`,
    /// `for-return`).  Off in XPath 1.0; the XSLT compiler sets it
    /// when the stylesheet declares `version="2.0"` or higher.
    xpath_2_0: bool,
}

impl Parser {
    #[allow(dead_code)]
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, spans: Vec::new(), src: String::new(), pos: 0, depth: 0, xpath_2_0: false }
    }

    /// Build a parser with full source-location context.  Both the
    /// original `src` string and the lexer's per-token `spans` are
    /// retained so any error this parser surfaces can quote the
    /// failing substring and point at its byte offset.
    pub fn new_with_spans(
        tokens: Vec<Token>,
        spans:  Vec<crate::xpath::lexer::Span>,
        src:    impl Into<String>,
    ) -> Self {
        Self { tokens, spans, src: src.into(), pos: 0, depth: 0, xpath_2_0: false }
    }

    /// Opt into XPath 2.0 grammar (`if-then-else`, `for-return`).
    /// Set by [`crate::xpath::parse_xpath_with`] from
    /// [`crate::xpath::XPathOptions::xpath_2_0`].
    pub fn set_xpath_2_0(&mut self, enabled: bool) { self.xpath_2_0 = enabled; }

    /// Construct a parse-time error tagged with the current token's
    /// source position.  Falls back to a bare message when this
    /// parser wasn't seeded with span info.
    //
    // The two byte-index slices below are guaranteed safe:
    //   * `self.src[start..end]` — `(start, end)` comes from the
    //     lexer's span table, whose endpoints are always set just
    //     after a complete UTF-8 codepoint.
    //   * `self.src[start..cut]` — `cut` is produced by walking
    //     `char_indices()` from `start`, so it is always a char
    //     boundary by construction.
    #[allow(clippy::string_slice)]
    fn error(&self, msg: impl Into<String>) -> XmlError {
        let base = msg.into();
        if let Some(&(start, end)) = self.spans.get(self.pos) {
            let snippet = if !self.src.is_empty() && end > start && end <= self.src.len() {
                let cut = self.src[start..end]
                    .char_indices()
                    .nth(40)
                    .map_or(end, |(i, _)| start + i);
                Some(&self.src[start..cut])
            } else {
                None
            };
            let context = match snippet {
                Some(s) if !s.is_empty() => format!(" at byte {start}: '{s}'"),
                _                        => format!(" at byte {start}"),
            };
            parse_err(format!("{base}{context}"))
        } else {
            parse_err(base)
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn peek2(&self) -> &Token {
        self.tokens.get(self.pos + 1).unwrap_or(&Token::Eof)
    }

    fn consume(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    // The two byte-index slices below are guaranteed safe — see the
    // discipline note on [`Self::error`] for the full argument.
    #[allow(clippy::string_slice)]
    fn expect(&mut self, expected: &Token) -> Result<()> {
        // Capture position BEFORE consuming so the error span points
        // at the unexpected token rather than the one past it.
        let saved_pos = self.pos;
        let tok = self.consume();
        if &tok == expected {
            Ok(())
        } else {
            let pos = saved_pos;
            let snippet = self.spans.get(pos).and_then(|&(s, e)| {
                if !self.src.is_empty() && e > s && e <= self.src.len() {
                    let end = self.src[s..e]
                        .char_indices()
                        .nth(40)
                        .map_or(e, |(i, _)| s + i);
                    Some(&self.src[s..end])
                } else { None }
            });
            let context = match (self.spans.get(pos), snippet) {
                (Some(&(s, _)), Some(snip)) if !snip.is_empty() =>
                    format!(" at byte {s}: '{snip}'"),
                (Some(&(s, _)), _) => format!(" at byte {s}"),
                _ => String::new(),
            };
            Err(parse_err(format!(
                "expected {expected:?}, got {tok:?}{context}"
            )))
        }
    }

    pub fn expect_eof(&self) -> Result<()> {
        if self.peek() == &Token::Eof {
            Ok(())
        } else {
            Err(self.error(format!(
                "unexpected token after expression: {:?}", self.peek()
            )))
        }
    }

    pub fn parse_expr(&mut self) -> Result<Expr> {
        // XPath 2.0 § 3.5 `Expr ::= ExprSingle ("," ExprSingle)*` —
        // a top-level comma-separated sequence of expressions.
        // 1.0 doesn't have this form so the loop body never executes
        // there.
        let first = self.parse_expr_single()?;
        if !self.xpath_2_0 || self.peek() != &Token::Comma {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.peek() == &Token::Comma {
            self.consume();
            items.push(self.parse_expr_single()?);
        }
        Ok(Expr::Sequence(items))
    }

    fn parse_expr_single(&mut self) -> Result<Expr> {
        // XPath 2.0 § 3.5 `ExprSingle ::= ForExpr | QuantifiedExpr |
        // IfExpr | OrExpr` — the additions are recognized by their
        // leading contextual keyword + a deterministic lookahead.
        // No depth bump here: this helper itself doesn't introduce
        // recursion; the precedence chain it kicks off bottoms out
        // at `parse_primary_expr`, which is where the recursive
        // re-entry (`(…)`, `[…]`, `f(…)`) is gated.
        if self.xpath_2_0 && self.peek_if_expr_2_0() {
            self.parse_if_expr_2_0()
        } else if self.xpath_2_0 && self.peek_for_expr_2_0() {
            self.parse_for_expr_2_0()
        } else if self.xpath_2_0 && self.peek_let_expr_2_0() {
            self.parse_let_expr_2_0()
        } else if self.xpath_2_0 && self.peek_quantified_2_0() {
            self.parse_quantified_2_0()
        } else if self.xpath_2_0 && self.peek_try_catch() {
            self.parse_try_catch()
        } else {
            self.parse_or_expr()
        }
    }

    /// Look-ahead for `try {` — the XPath 3.1 try/catch opener.
    /// Falls through when `try` is a bare name (no `{`) so older
    /// stylesheets using `try` as an element / variable name still
    /// parse.
    fn peek_try_catch(&self) -> bool {
        is_name_tok(self.peek(), "try") && self.peek2() == &Token::LBrace
    }

    /// `TryCatchExpr ::= TryClause CatchClause+`
    /// `TryClause   ::= "try" "{" Expr "}"`
    /// `CatchClause ::= "catch" NameTest ("|" NameTest)* "{" Expr "}"`
    fn parse_try_catch(&mut self) -> Result<Expr> {
        use crate::xpath::ast::{XPathCatch, CatchNameTest};
        self.consume(); // "try"
        self.expect(&Token::LBrace)?;
        self.enter()?;
        let body = self.parse_expr()?;
        self.expect(&Token::RBrace)?;
        self.leave();
        let mut catches: Vec<XPathCatch> = Vec::new();
        while is_name_tok(self.peek(), "catch") {
            self.consume(); // "catch"
            let mut matchers: Vec<CatchNameTest> = Vec::new();
            matchers.push(self.parse_catch_name_test()?);
            while self.peek() == &Token::Pipe {
                self.consume();
                matchers.push(self.parse_catch_name_test()?);
            }
            self.expect(&Token::LBrace)?;
            self.enter()?;
            let body = self.parse_expr()?;
            self.expect(&Token::RBrace)?;
            self.leave();
            catches.push(XPathCatch { matchers, body });
        }
        if catches.is_empty() {
            return Err(self.error(
                "try/catch requires at least one catch clause"));
        }
        Ok(Expr::TryCatch { body: Box::new(body), catches })
    }

    /// One name-test inside an XPath 3.1 `catch` clause name list:
    /// `*`, `prefix:*`, `*:NCName`, or `prefix:local` / `local`.
    fn parse_catch_name_test(&mut self) -> Result<crate::xpath::ast::CatchNameTest> {
        use crate::xpath::ast::CatchNameTest;
        match self.peek().clone() {
            Token::Star => {
                self.consume();
                if matches!(self.peek(), Token::Colon) {
                    if let Token::Name(local) = self.peek2().clone() {
                        self.consume(); // ':'
                        self.consume(); // local
                        return Ok(CatchNameTest::LocalNameOnly(local));
                    }
                }
                Ok(CatchNameTest::Any)
            }
            Token::Name(n) => {
                self.consume();
                if let Some((prefix, local)) = n.split_once(':') {
                    if local == "*" {
                        Ok(CatchNameTest::PrefixWildcard(prefix.to_string()))
                    } else {
                        Ok(CatchNameTest::QName {
                            prefix: Some(prefix.to_string()),
                            local:  local.to_string(),
                        })
                    }
                } else {
                    Ok(CatchNameTest::QName { prefix: None, local: n })
                }
            }
            other => Err(self.error(format!(
                "expected name-test in catch clause, got {other:?}"
            ))),
        }
    }

    /// Increment the parser's recursion-depth counter and bail out
    /// cleanly if it exceeds [`MAX_PARSE_DEPTH`].  Call this just
    /// before re-entering the precedence chain (inside `(…)`, `[…]`,
    /// function-call argument lists, etc.) — each such re-entry
    /// pushes ~20 frames worth of precedence helpers before the
    /// next gate can fire.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            // Don't decrement: this Parser instance is consumed on
            // error and not reused.  Returning early keeps the
            // remaining tokens unscanned, which the caller surfaces
            // through `expect_eof` if it ever recovers.
            return Err(parse_err(format!(
                "XPath expression nesting depth exceeds limit ({MAX_PARSE_DEPTH})"
            )));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Look-ahead for `if (` — the only spelling that XPath 2.0
    /// `IfExpr` can start with.  Won't fire on `if` used as a bare
    /// name (no `(` after) so XPath 1.0 stylesheets that happen to
    /// contain an element/attribute literally named `if` still parse.
    fn peek_if_expr_2_0(&self) -> bool {
        is_name_tok(self.peek(), "if") && self.peek2() == &Token::LParen
    }

    /// Look-ahead for `for $` — `ForExpr`'s leading
    /// `SimpleForClause` always opens with `for "$" VarName`.
    fn peek_for_expr_2_0(&self) -> bool {
        is_name_tok(self.peek(), "for") && self.peek2() == &Token::Dollar
    }

    /// Look-ahead for `let $` — `LetExpr`'s `SimpleLetClause` always
    /// opens with `let "$" VarName`.  The `$` disambiguates from a
    /// `let` element / function name.
    fn peek_let_expr_2_0(&self) -> bool {
        is_name_tok(self.peek(), "let") && self.peek2() == &Token::Dollar
    }

    /// `IfExpr ::= "if" "(" Expr ")" "then" ExprSingle "else" ExprSingle`
    /// (XPath 2.0 § 3.8).  Both branches are full expressions, so
    /// `if(a)then if(b)then x else y else z` nests left-to-right.
    fn parse_if_expr_2_0(&mut self) -> Result<Expr> {
        self.consume(); // "if"
        self.expect(&Token::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&Token::RParen)?;
        if !is_name_tok(self.peek(), "then") {
            return Err(self.error("expected `then` after `if (...)`"));
        }
        self.consume(); // "then"
        let then_branch = self.parse_expr_single()?;
        if !is_name_tok(self.peek(), "else") {
            return Err(self.error("expected `else` after `if (...) then ...`"));
        }
        self.consume(); // "else"
        let else_branch = self.parse_expr_single()?;
        Ok(Expr::IfThenElse {
            cond:        Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }

    /// Look-ahead for `some $` or `every $` — `QuantifiedExpr`'s
    /// only lexical openings.  Same disambiguation rationale as
    /// `peek_for_expr_2_0`: the `$` after the keyword is required.
    fn peek_quantified_2_0(&self) -> bool {
        (is_name_tok(self.peek(), "some") || is_name_tok(self.peek(), "every"))
            && self.peek2() == &Token::Dollar
    }

    /// `QuantifiedExpr ::= ("some" | "every") "$" VarName "in"
    /// ExprSingle ("," "$" VarName "in" ExprSingle)*
    /// "satisfies" ExprSingle`.  Shares its binding-chain shape
    /// with `ForExpr`; only the trailing keyword and the boolean
    /// folding semantics differ.
    fn parse_quantified_2_0(&mut self) -> Result<Expr> {
        let kind = match self.consume() {
            Token::Name(n) if n == "some"  => crate::xpath::ast::QuantifierKind::Some,
            Token::Name(n) if n == "every" => crate::xpath::ast::QuantifierKind::Every,
            other => return Err(parse_err(format!(
                "internal: peek_quantified_2_0 matched but consumed {other:?}"
            ))),
        };
        let mut bindings = Vec::new();
        loop {
            self.expect(&Token::Dollar)?;
            let name = match self.consume() {
                Token::Name(s) => s,
                other          => return Err(parse_err(format!(
                    "expected variable name after `$`, got {other:?}"
                ))),
            };
            if !is_name_tok(self.peek(), "in") {
                return Err(self.error("expected `in` after quantified-binding name"));
            }
            self.consume(); // "in"
            let in_expr = self.parse_expr_single()?;
            bindings.push((name, in_expr));
            if self.peek() == &Token::Comma {
                self.consume();
                continue;
            }
            break;
        }
        if !is_name_tok(self.peek(), "satisfies") {
            return Err(self.error("expected `satisfies` after quantified bindings"));
        }
        self.consume(); // "satisfies"
        let test = self.parse_expr_single()?;
        Ok(Expr::Quantified { kind, bindings, test: Box::new(test) })
    }

    /// `ForExpr ::= SimpleForClause "return" ExprSingle`
    /// where `SimpleForClause ::= "for" "$" VarName "in" ExprSingle
    /// ("," "$" VarName "in" ExprSingle)*`.  Captures every binding
    /// in source order; evaluation iterates them as nested loops
    /// (rightmost varies fastest).
    fn parse_for_expr_2_0(&mut self) -> Result<Expr> {
        self.consume(); // "for"
        let mut bindings = Vec::new();
        loop {
            self.expect(&Token::Dollar)?;
            let name = match self.consume() {
                Token::Name(s) => s,
                other          => return Err(parse_err(format!(
                    "expected variable name after `$`, got {other:?}"
                ))),
            };
            if !is_name_tok(self.peek(), "in") {
                return Err(self.error("expected `in` after `for $name`"));
            }
            self.consume(); // "in"
            let in_expr = self.parse_expr_single()?;
            bindings.push((name, in_expr));
            if self.peek() == &Token::Comma {
                self.consume(); // ","
                continue;
            }
            break;
        }
        if !is_name_tok(self.peek(), "return") {
            return Err(self.error("expected `return` after `for $v in ...`"));
        }
        self.consume(); // "return"
        let body = self.parse_expr_single()?;
        Ok(Expr::For { bindings, body: Box::new(body) })
    }

    /// `LetExpr ::= SimpleLetClause "return" ExprSingle`
    /// where `SimpleLetClause ::= "let" "$" VarName ":=" ExprSingle
    /// ("," "$" VarName ":=" ExprSingle)*` (XPath 3.0 § 3.10).  Each
    /// binding is evaluated once and is in scope for later bindings
    /// and the body.
    fn parse_let_expr_2_0(&mut self) -> Result<Expr> {
        self.consume(); // "let"
        let mut bindings = Vec::new();
        loop {
            self.expect(&Token::Dollar)?;
            let name = match self.consume() {
                Token::Name(s) => s,
                other          => return Err(parse_err(format!(
                    "expected variable name after `$`, got {other:?}"
                ))),
            };
            self.expect(&Token::ColonEq)?;
            let bound = self.parse_expr_single()?;
            bindings.push((name, bound));
            if self.peek() == &Token::Comma {
                self.consume(); // ","
                continue;
            }
            break;
        }
        if !is_name_tok(self.peek(), "return") {
            return Err(self.error("expected `return` after `let $v := ...`"));
        }
        self.consume(); // "return"
        let body = self.parse_expr_single()?;
        Ok(Expr::Let { bindings, body: Box::new(body) })
    }

    fn parse_or_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_and_expr()?;
        while is_name_tok(self.peek(), "or") {
            self.consume();
            let right = self.parse_and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_equality_expr()?;
        while is_name_tok(self.peek(), "and") {
            self.consume();
            let right = self.parse_equality_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_relational_expr()?;
        loop {
            // XPath 2.0 value-comparison keywords (`eq` / `ne`) read
            // as `Token::Name`.  We accept them as aliases of `=` /
            // `!=` here so XSLT 2.0+ stylesheets that only differ in
            // operator spelling parse cleanly.  Strict spec 2.0
            // says these error on node-set operands; we don't add
            // that restriction — the runtime general-comparison
            // result is equivalent for the atomic cases that
            // dominate real stylesheets.
            //
            // Disambiguation: a bare `Name("eq")` immediately
            // followed by `LParen` is the rare function-call
            // `eq(...)` (e.g. EXSLT?), not an operator.  Otherwise
            // it's the keyword.
            match self.peek() {
                Token::Eq => {
                    self.consume();
                    let right = self.parse_relational_expr()?;
                    left = Expr::Eq(Box::new(left), Box::new(right));
                }
                Token::Ne => {
                    self.consume();
                    let right = self.parse_relational_expr()?;
                    left = Expr::Ne(Box::new(left), Box::new(right));
                }
                _ if is_value_op_kw(self.peek(), "eq") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_relational_expr()?;
                    left = Expr::ValueEq(Box::new(left), Box::new(right));
                }
                _ if is_value_op_kw(self.peek(), "ne") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_relational_expr()?;
                    left = Expr::ValueNe(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_relational_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_string_concat_or_range()?;
        loop {
            match self.peek() {
                Token::Lt => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::Lt(Box::new(left), Box::new(right));
                }
                Token::Gt => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::Gt(Box::new(left), Box::new(right));
                }
                Token::Le => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::Le(Box::new(left), Box::new(right));
                }
                Token::Ge => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::Ge(Box::new(left), Box::new(right));
                }
                // XPath 2.0 `lt` / `gt` / `le` / `ge` — same alias
                // treatment as `eq` / `ne` above.
                _ if is_value_op_kw(self.peek(), "lt") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::ValueLt(Box::new(left), Box::new(right));
                }
                _ if is_value_op_kw(self.peek(), "gt") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::ValueGt(Box::new(left), Box::new(right));
                }
                _ if is_value_op_kw(self.peek(), "le") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::ValueLe(Box::new(left), Box::new(right));
                }
                _ if is_value_op_kw(self.peek(), "ge") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::ValueGe(Box::new(left), Box::new(right));
                }
                // XPath 2.0 §3.5.3 node-comparison operator `is`.
                // Returns true iff the two operands identify the same
                // node.  We approximate via generated-id equality —
                // each node in the index has a stable id, so this is
                // exact for source-tree nodes and consistent for
                // constructed nodes within one evaluation.  Same name-
                // tok / function-call disambiguation as the other
                // word operators.
                _ if is_value_op_kw(self.peek(), "is") && self.peek2() != &Token::LParen => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::NodeIs(Box::new(left), Box::new(right));
                }
                // XPath 2.0 node-comparison `<<` / `>>` —
                // document-order operators.  Same precedence as `is`.
                Token::LtLt if self.xpath_2_0 => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::NodeBefore(Box::new(left), Box::new(right));
                }
                Token::GtGt if self.xpath_2_0 => {
                    self.consume();
                    let right = self.parse_string_concat_or_range()?;
                    left = Expr::NodeAfter(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// XPath 2.0 `RangeExpr ::= AdditiveExpr ("to" AdditiveExpr)?`.
    /// In 1.0 mode the `to` branch is never considered, so `to` stays
    /// available as a plain Name in patterns / element tests.
    fn parse_range_or_additive(&mut self) -> Result<Expr> {
        let left = self.parse_additive_expr()?;
        // XPath 2.0 §3.3.1 — `to` is a reserved keyword (never a
        // function name in 2.0), so a trailing LParen here always
        // belongs to the parenthesised right operand of the range
        // (`E to (M)` or `E to (M+1)`), not a function call.
        if self.xpath_2_0 && is_name_tok(self.peek(), "to") {
            self.consume(); // "to"
            let right = self.parse_additive_expr()?;
            return Ok(Expr::Range(Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    /// XPath 3.0 `StringConcatExpr ::= RangeExpr ("||" RangeExpr)*`.
    /// Each operand atomises to a single xs:string; the result is
    /// the concatenation.  In 2.0 (no `||`) and 1.0 mode this layer
    /// is transparent — it just delegates to the range layer.
    fn parse_string_concat_or_range(&mut self) -> Result<Expr> {
        let mut left = self.parse_range_or_additive()?;
        while self.peek() == &Token::PipePipe {
            self.consume();
            let right = self.parse_range_or_additive()?;
            // Represent `a || b` as `concat(a, b)`.  Multi-segment
            // chains fold into a single concat call.
            left = match left {
                Expr::FunctionCall(name, mut args) if name == "concat" => {
                    args.push(right);
                    Expr::FunctionCall(name, args)
                }
                other => Expr::FunctionCall("concat".to_string(), vec![other, right]),
            };
        }
        Ok(left)
    }

    fn parse_additive_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_multiplicative_expr()?;
        loop {
            match self.peek() {
                Token::Plus => {
                    self.consume();
                    let right = self.parse_multiplicative_expr()?;
                    left = Expr::Add(Box::new(left), Box::new(right));
                }
                Token::Minus => {
                    self.consume();
                    let right = self.parse_multiplicative_expr()?;
                    left = Expr::Sub(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplicative_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary_expr()?;
        loop {
            match self.peek() {
                Token::Star => {
                    self.consume();
                    let right = self.parse_unary_expr()?;
                    left = Expr::Mul(Box::new(left), Box::new(right));
                }
                tok if is_name_tok(tok, "div") => {
                    self.consume();
                    let right = self.parse_unary_expr()?;
                    left = Expr::Div(Box::new(left), Box::new(right));
                }
                // XPath 2.0 `idiv` — integer division, truncating
                // towards zero.  Same precedence as `div` / `mod`.
                tok if self.xpath_2_0 && is_name_tok(tok, "idiv")
                    && self.peek2() != &Token::LParen =>
                {
                    self.consume();
                    let right = self.parse_unary_expr()?;
                    left = Expr::IDiv(Box::new(left), Box::new(right));
                }
                tok if is_name_tok(tok, "mod") => {
                    self.consume();
                    let right = self.parse_unary_expr()?;
                    left = Expr::Mod(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> Result<Expr> {
        // XPath 2.0 §3.4 `UnaryExpr ::= ("-" | "+")* UnionExpr` —
        // chains of leading `+` / `-` allowed; even count is a no-op,
        // odd count of `-` negates.  Unary `+` was XPath 1.0
        // forbidden but accepted by libxslt and by XPath 2.0.
        let mut neg = false;
        loop {
            match self.peek() {
                Token::Minus => { self.consume(); neg = !neg; }
                Token::Plus  => { self.consume(); /* identity */ }
                _ => break,
            }
        }
        let inner = self.parse_union_expr()?;
        let mut expr = if neg { Expr::Neg(Box::new(inner)) } else { inner };
        // XPath 3.1 `ArrowExpr ::= UnaryExpr ('=>' ArrowFunctionSpecifier
        // ArgumentList)*` — `e => f(args)` is sugar for
        // `f(e, args)`.  We desugar by emitting an `Expr::FunctionCall`
        // with `expr` prepended to the argument list.  Multi-segment
        // chains nest naturally.  Only the simple `name(...)`
        // arrow-function-specifier form is accepted here; the parser
        // doesn't carry the function-reference / variable-binding
        // forms that XPath 3.1 also allows.
        while self.xpath_2_0 && self.peek() == &Token::Arrow {
            self.consume();
            let Token::Name(name) = self.peek().clone() else {
                return Err(parse_err("=> must be followed by a function name"));
            };
            self.consume();
            self.expect(&Token::LParen)?;
            let mut args = vec![expr];
            if !matches!(self.peek(), Token::RParen) {
                self.enter()?;
                args.push(self.parse_expr_single()?);
                self.leave();
                while matches!(self.peek(), Token::Comma) {
                    self.consume();
                    self.enter()?;
                    args.push(self.parse_expr_single()?);
                    self.leave();
                }
            }
            self.expect(&Token::RParen)?;
            expr = Expr::FunctionCall(name, args);
        }
        Ok(expr)
    }

    fn parse_union_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_intersect_except_expr()?;
        loop {
            match self.peek() {
                Token::Pipe => {
                    self.consume();
                    let right = self.parse_intersect_except_expr()?;
                    left = Expr::Union(Box::new(left), Box::new(right));
                }
                // XPath 2.0 word-form alias for `|`.
                tok if self.xpath_2_0 && is_name_tok(tok, "union")
                    && self.peek2() != &Token::LParen =>
                {
                    self.consume();
                    let right = self.parse_intersect_except_expr()?;
                    left = Expr::Union(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// XPath 2.0 `IntersectExceptExpr ::= InstanceofExpr (("intersect"
    /// | "except") InstanceofExpr)*` — set operations on node-set
    /// operands.  In 1.0 mode this layer is transparent (passes
    /// straight through to the instance-of layer, which itself is
    /// transparent without the keyword).
    fn parse_intersect_except_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_instanceof_expr()?;
        loop {
            if !self.xpath_2_0 { break; }
            if is_name_tok(self.peek(), "intersect") && self.peek2() != &Token::LParen {
                self.consume();
                let right = self.parse_instanceof_expr()?;
                left = Expr::Intersect(Box::new(left), Box::new(right));
            } else if is_name_tok(self.peek(), "except") && self.peek2() != &Token::LParen {
                self.consume();
                let right = self.parse_instanceof_expr()?;
                left = Expr::Except(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// XPath 2.0 `InstanceofExpr ::= TreatExpr ("instance" "of"
    /// SequenceType)?`.  Treat / cast / castable are folded into the
    /// same descent — they all read a SequenceType / SingleType on
    /// the right.  All four are gated on `xpath_2_0`.
    fn parse_instanceof_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_treat_expr()?;
        if self.xpath_2_0
            && is_name_tok(self.peek(), "instance")
            && is_name_tok(&self.tokens.get(self.pos + 1).cloned().unwrap_or(Token::Eof), "of")
        {
            self.consume(); // instance
            self.consume(); // of
            let st = self.parse_sequence_type()?;
            left = Expr::InstanceOf(Box::new(left), st);
        }
        Ok(left)
    }

    fn parse_treat_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_castable_expr()?;
        if self.xpath_2_0
            && is_name_tok(self.peek(), "treat")
            && is_name_tok(&self.tokens.get(self.pos + 1).cloned().unwrap_or(Token::Eof), "as")
        {
            self.consume(); self.consume();
            let st = self.parse_sequence_type()?;
            left = Expr::TreatAs(Box::new(left), st);
        }
        Ok(left)
    }

    fn parse_castable_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_cast_expr()?;
        if self.xpath_2_0
            && is_name_tok(self.peek(), "castable")
            && is_name_tok(&self.tokens.get(self.pos + 1).cloned().unwrap_or(Token::Eof), "as")
        {
            self.consume(); self.consume();
            let st = self.parse_single_type()?;
            left = Expr::CastableAs(Box::new(left), st);
        }
        Ok(left)
    }

    fn parse_cast_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_path_expr()?;
        if self.xpath_2_0
            && is_name_tok(self.peek(), "cast")
            && is_name_tok(&self.tokens.get(self.pos + 1).cloned().unwrap_or(Token::Eof), "as")
        {
            self.consume(); self.consume();
            let st = self.parse_single_type()?;
            left = Expr::CastAs(Box::new(left), st);
        }
        Ok(left)
    }

    /// `SequenceType ::= ("empty-sequence" "(" ")" | ItemType
    /// OccurrenceIndicator?)`.  We accept the small atomic + kind
    /// vocabulary detailed on [`crate::xpath::ast::ItemType`].
    fn parse_sequence_type(&mut self) -> Result<crate::xpath::ast::SequenceType> {
        use crate::xpath::ast::{SequenceType, Occurrence};
        // `empty-sequence()` short-circuits — no occurrence indicator.
        if is_name_tok(self.peek(), "empty-sequence")
            && self.peek2() == &Token::LParen
        {
            self.consume(); self.consume();
            self.expect(&Token::RParen)?;
            // `empty-sequence()` matches only the empty sequence: the
            // item test matches nothing, so a non-empty value fails the
            // per-item check while an empty value passes on cardinality
            // alone (ZeroOrMore admits zero items).
            return Ok(SequenceType {
                item: crate::xpath::ast::ItemType::EmptySequence,
                occurrence: Occurrence::ZeroOrMore,
            });
        }
        let item = self.parse_item_type()?;
        let occurrence = match self.peek() {
            Token::Star     => { self.consume(); Occurrence::ZeroOrMore }
            Token::Plus     => { self.consume(); Occurrence::OneOrMore }
            Token::Question => { self.consume(); Occurrence::Optional   }
            _               => Occurrence::One,
        };
        Ok(SequenceType { item, occurrence })
    }

    /// `SingleType ::= AtomicType "?"?`.  We accept the same atomic
    /// vocabulary as ItemType plus the trailing `?` (parsed as
    /// occurrence here, even though SingleType's `?` is permitted
    /// but our occurrence handling treats it the same).
    fn parse_single_type(&mut self) -> Result<crate::xpath::ast::SequenceType> {
        self.parse_sequence_type()
    }

    fn parse_item_type(&mut self) -> Result<crate::xpath::ast::ItemType> {
        use crate::xpath::ast::{ItemType, FunctionSig, SequenceType, Occurrence};
        // XPath 3.1 §2.5.4.3 function test: `function(*)` or
        // `function(SeqType, …) as SeqType`.  The specific form captures
        // the parameter and return types for function subtyping.
        if matches!(self.peek(), Token::Name(n) if n == "function")
            && self.peek2() == &Token::LParen
        {
            self.consume(); // function
            self.consume(); // (
            if self.peek() == &Token::Star {
                self.consume();
                self.expect(&Token::RParen)?;
                return Ok(ItemType::Function(None));
            }
            let mut params = Vec::new();
            if self.peek() != &Token::RParen {
                loop {
                    params.push(self.parse_sequence_type()?);
                    if self.peek() == &Token::Comma { self.consume(); continue; }
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            let ret = if matches!(self.peek(), Token::Name(a) if a == "as") {
                self.consume();
                self.parse_sequence_type()?
            } else {
                SequenceType { item: ItemType::Any, occurrence: Occurrence::ZeroOrMore }
            };
            return Ok(ItemType::Function(Some(Box::new(FunctionSig { params, ret }))));
        }
        // KindTest forms — `node()`, `element()`, etc. — recognise via
        // the lookahead `name (` pattern.
        if let Token::Name(name) = self.peek().clone() {
            if self.peek2() == &Token::LParen {
                let kind = match name.as_str() {
                    "item"                   => Some(ItemType::Any),
                    "node"                   => Some(ItemType::AnyNode),
                    "text"                   => Some(ItemType::Text),
                    "comment"                => Some(ItemType::Comment),
                    "document-node"          => Some(ItemType::Document),
                    "processing-instruction" => Some(ItemType::PI(None)),
                    "element"                => Some(ItemType::Element(None)),
                    "attribute"              => Some(ItemType::Attribute(None)),
                    _ => None,
                };
                if let Some(k) = kind {
                    self.consume(); // name
                    self.consume(); // (
                    // Most kind tests accept an optional name arg.
                    // For `element(*)` / `attribute(*)` we keep `None`.
                    let k = match k {
                        ItemType::Document => {
                            // `document-node(element(...))` / `document-node(schema-element(...))`
                            // — drill into the inner kind test so the
                            // parser consumes both `)`s.  We discard
                            // the inner name (schema validation isn't
                            // implemented); the outer test still
                            // matches any document node.
                            if matches!(self.peek(), Token::Name(n)
                                if n == "element" || n == "schema-element")
                            {
                                self.consume();        // element / schema-element
                                self.expect(&Token::LParen)?;
                                while self.peek() != &Token::RParen
                                   && self.peek() != &Token::Eof
                                {
                                    self.consume();
                                }
                                self.expect(&Token::RParen)?;
                            }
                            ItemType::Document
                        }
                        ItemType::PI(_) => {
                            let arg = match self.peek() {
                                Token::Literal(s) | Token::Name(s) => {
                                    let s = s.clone(); self.consume(); Some(s)
                                }
                                _ => None,
                            };
                            // Accept `, type` (schema-aware) by
                            // skipping rest until `)` — we don't
                            // honour the type but compile shouldn't
                            // fail on it.
                            while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                                self.consume();
                            }
                            ItemType::PI(arg)
                        }
                        ItemType::Element(_) => {
                            let arg = match self.peek() {
                                Token::Name(s) => { let s = s.clone(); self.consume(); Some(s) }
                                Token::Star    => { self.consume(); None }
                                _              => None,
                            };
                            while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                                self.consume();
                            }
                            ItemType::Element(arg)
                        }
                        ItemType::Attribute(_) => {
                            let arg = match self.peek() {
                                Token::Name(s) => { let s = s.clone(); self.consume(); Some(s) }
                                Token::At      => { self.consume();
                                                    match self.peek() {
                                                        Token::Name(s) => { let s = s.clone(); self.consume(); Some(s) }
                                                        _              => None,
                                                    } }
                                Token::Star    => { self.consume(); None }
                                _              => None,
                            };
                            while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                                self.consume();
                            }
                            ItemType::Attribute(arg)
                        }
                        other => other,
                    };
                    self.expect(&Token::RParen)?;
                    return Ok(k);
                }
            }
            // Atomic-type test: `xs:integer`, `xs:string`, …  Accept
            // any prefix:local form; eval handles the type matching.
            // XPath 2.0 §2.5.4 says the type must be a QName, but
            // an unprefixed name may resolve through the surrounding
            // xpath-default-namespace; defer the strictness to eval.
            self.consume();
            let local = match name.split_once(':') {
                Some((_, l)) => l.to_string(),
                None         => name,
            };
            return Ok(ItemType::Atomic(local));
        }
        Err(self.error(format!(
            "expected SequenceType / ItemType, got {:?}", self.peek()
        )))
    }

    /// Determine if the current position starts a location path step (not a primary expr).
    fn is_location_path_start(&self) -> bool {
        match self.peek() {
            Token::Slash | Token::DoubleSlash | Token::Dot | Token::DotDot | Token::At | Token::Star => true,
            Token::Name(name) => {
                // Axis name followed by :: → location path
                if self.peek2() == &Token::ColonColon {
                    return true;
                }
                // XPath 3.1 `map { … }` / `array { … }` constructors are
                // primary expressions, not name-test steps.
                if self.xpath_2_0 && self.peek2() == &Token::LBrace
                    && (name == "map" || name == "array")
                {
                    return false;
                }
                // XPath 3.1 named function reference `name#arity` is a
                // primary expression, not a name-test step.
                if self.xpath_2_0 && self.peek2() == &Token::Hash {
                    return false;
                }
                // Node-type name followed by ( → location path (step with node type test)
                if self.peek2() == &Token::LParen && is_node_type(name) {
                    return true;
                }
                // Any other Name → element name test → location path
                // UNLESS it's a function call (Name followed by '(' that is NOT a node type)
                if self.peek2() == &Token::LParen {
                    return false; // function call → primary expr
                }
                true
            }
            _ => false,
        }
    }

    fn parse_path_expr(&mut self) -> Result<Expr> {
        let mut left = self.parse_path_expr_inner()?;
        // XPath 3.0 `SimpleMapExpr ::= PathExpr ("!" PathExpr)*`.
        // We fold the chain into a left-associative tree of
        // `Expr::SimpleMap` nodes.  Each `!` evaluates the RHS
        // once per item of the LHS, with that item as context.
        while self.xpath_2_0 && self.peek() == &Token::Bang {
            self.consume();
            let right = self.parse_path_expr_inner()?;
            left = Expr::SimpleMap(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_path_expr_inner(&mut self) -> Result<Expr> {
        if self.is_location_path_start() {
            let path = self.parse_location_path()?;
            return Ok(Expr::Path(path));
        }
        // Primary expression, optionally followed by predicates and steps
        self.parse_filter_path_expr()
    }

    fn parse_filter_path_expr(&mut self) -> Result<Expr> {
        let mut expr = self.parse_primary_expr()?;

        // XPath 3.1 PostfixExpr: a primary followed by any mix of dynamic
        // function calls `E(args)`, filter predicates `E[…]`, postfix
        // lookups `E?K`, and relative-path steps `E/…`, applied left to
        // right (§3.2.2 / §3.11.3).  A single chained loop is what lets a
        // call follow a predicate (`$f[3](2)[1]`) and vice versa.
        loop {
            match self.peek() {
                Token::LParen if self.xpath_2_0 => {
                    expr = self.parse_dynamic_call(expr)?;
                }
                Token::LBracket => {
                    // Fold consecutive predicates into one FilterPath; an
                    // interleaved call/lookup/step starts a fresh wrapper.
                    let mut predicates = Vec::new();
                    while self.peek() == &Token::LBracket {
                        self.consume();
                        self.enter()?;
                        predicates.push(self.parse_expr_single()?);
                        self.leave();
                        self.expect(&Token::RBracket)?;
                    }
                    expr = Expr::FilterPath {
                        primary: Box::new(expr),
                        predicates,
                        steps: Vec::new(),
                    };
                }
                Token::Question if self.xpath_2_0 => {
                    self.consume();
                    let key = self.parse_lookup_key()?;
                    expr = Expr::Lookup(Box::new(expr), key);
                }
                Token::Slash | Token::DoubleSlash => {
                    let steps = self.parse_path_steps()?;
                    expr = Expr::FilterPath {
                        primary: Box::new(expr),
                        predicates: Vec::new(),
                        steps,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// Parse the argument list of a dynamic function call, starting at
    /// the opening `(`.  A bare `?` argument (followed by `,` or `)`) is
    /// an [`Expr::Placeholder`] for partial application.
    fn parse_dynamic_call(&mut self, func: Expr) -> Result<Expr> {
        self.consume(); // (
        self.enter()?;
        let mut args = Vec::new();
        if self.peek() != &Token::RParen {
            loop {
                if self.peek() == &Token::Question
                    && matches!(self.peek2(), Token::Comma | Token::RParen)
                {
                    self.consume();
                    args.push(Expr::Placeholder);
                } else {
                    args.push(self.parse_expr_single()?);
                }
                if self.peek() == &Token::Comma { self.consume(); continue; }
                break;
            }
        }
        self.expect(&Token::RParen)?;
        self.leave();
        Ok(Expr::DynamicCall { func: Box::new(func), args })
    }

    fn parse_location_path(&mut self) -> Result<LocationPath> {
        match self.peek() {
            Token::Slash => {
                self.consume();
                if self.is_step_start() {
                    let steps = self.parse_steps()?;
                    Ok(LocationPath::Absolute(steps))
                } else {
                    Ok(LocationPath::Absolute(vec![]))
                }
            }
            Token::DoubleSlash => {
                self.consume();
                let mut steps = vec![desc_or_self_step()];
                steps.extend(self.parse_steps()?);
                Ok(LocationPath::Absolute(steps))
            }
            _ => {
                let steps = self.parse_steps()?;
                Ok(LocationPath::Relative(steps))
            }
        }
    }

    fn is_step_start(&self) -> bool {
        match self.peek() {
            Token::Dot | Token::DotDot | Token::At | Token::Star => true,
            // Parenthesised expression as a step (XPath 2.0 §3.2 —
            // FilterExpr alternative).
            Token::LParen => true,
            Token::Name(_) => true,
            _ => false,
        }
    }

    fn parse_steps(&mut self) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        steps.push(self.parse_step()?);
        loop {
            match self.peek() {
                Token::DoubleSlash => {
                    self.consume();
                    steps.push(desc_or_self_step());
                    steps.push(self.parse_step()?);
                }
                Token::Slash => {
                    self.consume();
                    steps.push(self.parse_step()?);
                }
                _ => break,
            }
        }
        Ok(steps)
    }

    /// Parse path steps that continue from a filter expression (after '/' or '//').
    fn parse_path_steps(&mut self) -> Result<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            match self.peek() {
                Token::DoubleSlash => {
                    self.consume();
                    steps.push(desc_or_self_step());
                    if self.is_step_start() {
                        steps.push(self.parse_step()?);
                    }
                }
                Token::Slash => {
                    self.consume();
                    if self.is_step_start() {
                        steps.push(self.parse_step()?);
                    }
                }
                _ => break,
            }
        }
        Ok(steps)
    }

    fn parse_step(&mut self) -> Result<Step> {
        if self.peek() == &Token::Dot {
            self.consume();
            let mut predicates = Vec::new();
            // XPath 2.0 §3.2.1 — `.[predicate]` filters the context
            // item.  Consume any trailing predicate list.
            while self.peek() == &Token::LBracket {
                self.consume();
                self.enter()?;
                let pred = self.parse_expr_single()?;
                self.leave();
                predicates.push(pred);
                self.expect(&Token::RBracket)?;
            }
            return Ok(Step { axis: Axis::Self_, node_test: NodeTest::AnyNode, predicates, filter: None });
        }
        if self.peek() == &Token::DotDot {
            self.consume();
            return Ok(Step { axis: Axis::Parent, node_test: NodeTest::AnyNode, predicates: vec![], filter: None });
        }

        // XPath 2.0 §3.2 — a step can be a FilterExpr (function
        // call or parenthesised expression) in addition to the
        // axis-step shape.  Detect these shapes first so
        // `path/key('x', 'y')` and `path/(expr)` parse correctly.
        if let Some(filter) = self.try_parse_filter_step()? {
            let mut predicates = Vec::new();
            while self.peek() == &Token::LBracket {
                self.consume();
                self.enter()?;
                let pred = self.parse_expr_single()?;
                self.leave();
                predicates.push(pred);
                self.expect(&Token::RBracket)?;
            }
            return Ok(Step {
                axis: Axis::Self_,
                node_test: NodeTest::AnyNode,
                predicates,
                filter: Some(Box::new(filter)),
            });
        }

        let (axis, node_test) = self.parse_axis_and_node_test()?;
        let mut predicates = Vec::new();
        while self.peek() == &Token::LBracket {
            self.consume();
            self.enter()?;
            let pred = self.parse_expr_single()?;
            self.leave();
            predicates.push(pred);
            self.expect(&Token::RBracket)?;
        }
        Ok(Step { axis, node_test, predicates, filter: None })
    }

    /// Attempt to parse a step as a FilterExpr.  Returns
    /// `Some(expr)` when the next tokens shape up as a
    /// parenthesised expression `(…)` or a function call
    /// `name(…)` whose name isn't a recognised kind test.
    /// Returns `None` to let the caller fall back to axis-step
    /// parsing.
    fn try_parse_filter_step(&mut self) -> Result<Option<Expr>> {
        match self.peek() {
            // Parenthesised expression step — XPath 2.0 §3.2 allows
            // a comma-separated sequence inside the parens (so
            // `./(a, b)` sorts (a,b) into document order).  Use the
            // top-level Expr grammar so the comma is recognised.
            Token::LParen => {
                self.consume();
                if matches!(self.peek(), Token::RParen) {
                    self.consume();
                    return Ok(Some(Expr::Sequence(Vec::new())));
                }
                self.enter()?;
                let e = self.parse_expr()?;
                self.leave();
                self.expect(&Token::RParen)?;
                Ok(Some(e))
            }
            // Function call step — name followed by `(`, where the
            // name isn't a recognised KindTest keyword.
            Token::Name(n) if self.peek2() == &Token::LParen
                && !is_kind_test_name(n) =>
            {
                let name = n.clone();
                self.consume(); // name
                self.consume(); // (
                let mut args = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    self.enter()?;
                    args.push(self.parse_expr_single()?);
                    self.leave();
                    while matches!(self.peek(), Token::Comma) {
                        self.consume();
                        self.enter()?;
                        args.push(self.parse_expr_single()?);
                        self.leave();
                    }
                }
                self.expect(&Token::RParen)?;
                Ok(Some(Expr::FunctionCall(name, args)))
            }
            // XPath 2.0 §3.2 — a step may be a primary VariableRef
            // (`$var` or `path/$var`).  The variable's value is the
            // step's input items and any trailing predicates filter
            // it the same way they'd filter a node-step's output.
            Token::Dollar if self.xpath_2_0
                && matches!(self.peek2(), Token::Name(_)) =>
            {
                self.consume(); // $
                let name = match self.consume() {
                    Token::Name(n) => n,
                    _ => unreachable!("guarded by peek2"),
                };
                Ok(Some(Expr::Variable(name)))
            }
            _ => Ok(None),
        }
    }

    fn parse_axis_and_node_test(&mut self) -> Result<(Axis, NodeTest)> {
        if self.peek() == &Token::At {
            self.consume();
            let nt = self.parse_node_test(true)?;
            return Ok((Axis::Attribute, nt));
        }

        if let Token::Name(name) = self.peek() {
            if self.peek2() == &Token::ColonColon {
                let axis = parse_axis_name(&name.clone())?;
                self.consume(); // axis name
                self.consume(); // ::
                let is_attr = axis == Axis::Attribute;
                let nt = self.parse_node_test(is_attr)?;
                return Ok((axis, nt));
            }
        }

        // XPath 2.0 §2.5.5 — when the axis is implicit, the
        // default depends on the kind test that follows.
        // `attribute()` / `attribute(name)` defaults to the
        // attribute axis; everything else (including `element()`,
        // `text()`, `*`) defaults to child.  `namespace::` only
        // appears with an explicit prefix.
        let implicit_axis = match (self.peek(), self.peek2()) {
            (Token::Name(n), Token::LParen)
                if n == "attribute" || n == "schema-attribute" => Axis::Attribute,
            _ => Axis::Child,
        };
        let is_attr = implicit_axis == Axis::Attribute;
        let nt = self.parse_node_test(is_attr)?;
        Ok((implicit_axis, nt))
    }

    /// Parse the name/type argument of an XPath 2.0 KindTest
    /// (`element(name)`, `attribute(*, xs:T)`).  Accepts `*` (any
    /// name), a prefixed QName, or a bare NCName.  Returns the
    /// closest equivalent 1.0 NodeTest variant — the kind keyword
    /// at the call site already encodes whether we're matching
    /// elements or attributes, and the engine relies on axis context
    /// to disambiguate.
    fn parse_kind_name_or_wildcard_nt(&mut self) -> Result<NodeTest> {
        match self.peek().clone() {
            Token::Star => {
                self.consume();
                Ok(NodeTest::Wildcard)
            }
            Token::Name(name) => {
                self.consume();
                if let Some((prefix, local)) = name.split_once(':') {
                    if local == "*" {
                        Ok(NodeTest::PrefixWildcard(prefix.to_string()))
                    } else {
                        Ok(NodeTest::QName(prefix.to_string(), local.to_string()))
                    }
                } else {
                    Ok(NodeTest::LocalName(name))
                }
            }
            other => Err(parse_err(format!(
                "kind test expected name or '*', got {other:?}"
            ))),
        }
    }

    fn parse_node_test(&mut self, _is_attr_axis: bool) -> Result<NodeTest> {
        match self.peek().clone() {
            Token::Star => {
                self.consume();
                // XPath 2.0 wildcard `*:NCName` — any namespace,
                // matching local name.  Lexer emits Star then Colon
                // then Name; recognise that sequence here.
                if matches!(self.peek(), Token::Colon) {
                    if let Token::Name(n) = self.peek2().clone() {
                        self.consume(); // ':'
                        self.consume(); // local name
                        return Ok(NodeTest::LocalNameOnly(n));
                    }
                }
                Ok(NodeTest::Wildcard)
            }
            Token::Name(name) if self.peek2() == &Token::LParen => {
                let name = name.clone();
                self.consume(); // name
                self.consume(); // (
                let nt = match name.as_str() {
                    "node" => {
                        self.expect(&Token::RParen)?;
                        NodeTest::AnyNode
                    }
                    "text" => {
                        self.expect(&Token::RParen)?;
                        NodeTest::Text
                    }
                    "comment" => {
                        self.expect(&Token::RParen)?;
                        NodeTest::Comment
                    }
                    "processing-instruction" => {
                        // XPath 1.0 §2.3 spells the target as a string
                        // Literal (`processing-instruction('thing')`).
                        // Saxon, libxslt, and Xalan all also accept a
                        // bare NCName form (`processing-instruction(thing)`)
                        // for compatibility with informal stylesheets;
                        // honour the same extension so the W3C suite's
                        // `match="processing-instruction(thing)"` cases
                        // compile.
                        let target = match self.peek() {
                            Token::Literal(s) => {
                                let s = s.clone();
                                self.consume();
                                Some(s)
                            }
                            Token::Name(s) => {
                                let s = s.clone();
                                self.consume();
                                Some(s)
                            }
                            _ => None,
                        };
                        // XPath 2.0 §2.5.4 / XML Names — a PI target
                        // is an NCName, so a colon is never valid.
                        if let Some(t) = &target {
                            if t.contains(':') {
                                return Err(parse_err(format!(
                                    "processing-instruction target '{t}' \
                                     contains a colon and is not a valid \
                                     NCName"
                                )));
                            }
                        }
                        self.expect(&Token::RParen)?;
                        NodeTest::PI(target)
                    }
                    // XPath 2.0 §2.5.4 schema-aware KindTest variants —
                    // `element()`, `element(*)`, `element(name)`,
                    // `element(name, T)`, `element(*, T)`, plus the
                    // analogous `attribute(...)`, `document-node(...)`,
                    // and `schema-{element,attribute}(name)` forms.
                    // We don't carry XPath 2.0 type annotations through
                    // the index, so the schema portion is ignored: the
                    // tests collapse to the existing 1.0 name-based
                    // NodeTest variants, keyed on axis context.
                    "element" | "attribute" | "schema-element" | "schema-attribute" => {
                        let nt = if matches!(self.peek(), Token::RParen) {
                            self.consume();
                            NodeTest::Wildcard
                        } else {
                            let nt = self.parse_kind_name_or_wildcard_nt()?;
                            // Optional second arg: TypeName — ignored.
                            // (XPath 2.0 also allows a trailing `?` for
                            // nillable; the lexer doesn't yet tokenise
                            // `?`, so callers using that form fall into
                            // a separate compile error we can address
                            // once we add the token.)
                            if matches!(self.peek(), Token::Comma) {
                                self.consume();
                                let _ = self.parse_kind_name_or_wildcard_nt()?;
                            }
                            self.expect(&Token::RParen)?;
                            nt
                        };
                        nt
                    }
                    "document-node" => {
                        let nt = if matches!(self.peek(), Token::RParen) {
                            self.consume();
                            NodeTest::Document(None)
                        } else if matches!(self.peek(), Token::Name(n) if n == "element" || n == "schema-element") {
                            self.consume(); // inner kind keyword
                            self.expect(&Token::LParen)?;
                            let inner = if matches!(self.peek(), Token::RParen) {
                                NodeTest::Wildcard
                            } else {
                                let n = self.parse_kind_name_or_wildcard_nt()?;
                                if matches!(self.peek(), Token::Comma) {
                                    self.consume();
                                    let _ = self.parse_kind_name_or_wildcard_nt()?;
                                }
                                n
                            };
                            self.expect(&Token::RParen)?;
                            self.expect(&Token::RParen)?;
                            // The node must be a document node whose
                            // document element satisfies the inner test
                            // — keep it wrapped rather than collapsing
                            // to the bare element test.
                            NodeTest::Document(Some(Box::new(inner)))
                        } else {
                            return Err(parse_err(
                                "document-node(...) expects element() or schema-element(...)"));
                        };
                        nt
                    }
                    _ => return Err(parse_err(format!("unknown node type function: {name}"))),
                };
                Ok(nt)
            }
            Token::Name(name) => {
                let name = name.clone();
                self.consume();
                if let Some((prefix, local)) = name.split_once(':') {
                    if local == "*" {
                        Ok(NodeTest::PrefixWildcard(prefix.to_string()))
                    } else {
                        Ok(NodeTest::QName(prefix.to_string(), local.to_string()))
                    }
                } else {
                    Ok(NodeTest::LocalName(name))
                }
            }
            other => Err(parse_err(format!("expected node test, got {other:?}"))),
        }
    }

    fn parse_primary_expr(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::Dollar => {
                self.consume();
                let saved_pos = self.pos;
                match self.consume() {
                    Token::Name(var) => Ok(Expr::Variable(var)),
                    tok => {
                        let context = self.spans.get(saved_pos)
                            .map(|&(s, _)| format!(" at byte {s}"))
                            .unwrap_or_default();
                        Err(parse_err(format!(
                            "expected variable name after '$', got {tok:?}{context}"
                        )))
                    }
                }
            }
            Token::LParen => {
                self.consume();
                // XPath 2.0 § 3.1.2: `(Expr1, Expr2, …)` is a sequence
                // constructor; a singleton `(Expr)` is just the
                // parenthesised expression.  In 1.0 mode we accept
                // `()` as the empty sequence too (some legacy
                // stylesheets use it) but otherwise stay strict.
                if self.peek() == &Token::RParen {
                    self.consume();
                    return Ok(Expr::Sequence(Vec::new()));
                }
                self.enter()?;
                let first = self.parse_expr()?;
                if self.peek() == &Token::Comma {
                    if !self.xpath_2_0 {
                        self.leave();
                        return Err(self.error(
                            "comma at top of `(...)` is XPath 2.0 sequence-literal syntax"
                        ));
                    }
                    let mut items = vec![first];
                    while self.peek() == &Token::Comma {
                        self.consume();
                        items.push(self.parse_expr()?);
                    }
                    self.expect(&Token::RParen)?;
                    self.leave();
                    return Ok(Expr::Sequence(items));
                }
                self.expect(&Token::RParen)?;
                self.leave();
                Ok(first)
            }
            Token::Literal(s) => {
                let s = s.clone();
                self.consume();
                Ok(Expr::Literal(s))
            }
            Token::Integer(i) => {
                self.consume();
                Ok(Expr::Integer(i))
            }
            Token::Decimal(n) => {
                self.consume();
                Ok(Expr::Decimal(n))
            }
            Token::Double(n) => {
                self.consume();
                Ok(Expr::Double(n))
            }
            // XPath 3.1 inline function `function($p, …) { body }`.
            Token::Name(name) if name == "function"
                && self.peek2() == &Token::LParen && self.xpath_2_0 =>
            {
                self.parse_inline_function()
            }
            // XPath 3.1 map constructor `map { k: v, … }`.
            Token::Name(name) if name == "map"
                && self.peek2() == &Token::LBrace && self.xpath_2_0 =>
            {
                self.parse_map_constructor()
            }
            // XPath 3.1 curly array constructor `array { … }`.
            Token::Name(name) if name == "array"
                && self.peek2() == &Token::LBrace && self.xpath_2_0 =>
            {
                self.parse_curly_array()
            }
            // XPath 3.1 square array constructor `[ a, b, c ]`.
            Token::LBracket if self.xpath_2_0 => self.parse_square_array(),
            // XPath 3.1 unary lookup `? K` — applies to the context item.
            Token::Question if self.xpath_2_0 => {
                self.consume();
                let key = self.parse_lookup_key()?;
                Ok(Expr::UnaryLookup(key))
            }
            // XPath 3.1 §3.1.6 named function reference `name#arity`.
            Token::Name(name) if self.xpath_2_0 && self.peek2() == &Token::Hash => {
                let name = name.clone();
                self.consume(); // name
                self.consume(); // #
                let arity = match self.consume() {
                    Token::Integer(i) if i >= 0 => i as usize,
                    t => return Err(self.error(format!(
                        "expected non-negative arity after '#', got {t:?}"))),
                };
                Ok(Expr::NamedFunctionRef { name, arity })
            }
            Token::Name(name) if self.peek2() == &Token::LParen => {
                let name = name.clone();
                self.consume(); // function name
                self.consume(); // (
                // Each argument is an ExprSingle (commas are
                // argument separators, not sequence constructors —
                // XPath 2.0 § 3.1.5).
                self.enter()?;
                let mut args = Vec::new();
                if self.peek() != &Token::RParen {
                    args.push(self.parse_expr_single()?);
                    while self.peek() == &Token::Comma {
                        self.consume();
                        args.push(self.parse_expr_single()?);
                    }
                }
                self.expect(&Token::RParen)?;
                self.leave();
                Ok(Expr::FunctionCall(name, args))
            }
            other => Err(self.error(format!("unexpected token in expression: {other:?}"))),
        }
    }

    /// XPath 3.1 §3.11.1 `map { key : value, … }`.  `map` and `{`
    /// already peeked by the caller.
    fn parse_map_constructor(&mut self) -> Result<Expr> {
        self.consume(); // map
        self.consume(); // {
        self.enter()?;
        let mut entries = Vec::new();
        if self.peek() != &Token::RBrace {
            loop {
                let key = self.parse_expr_single()?;
                self.expect(&Token::Colon)?;
                let val = self.parse_expr_single()?;
                entries.push((key, val));
                if self.peek() == &Token::Comma { self.consume(); continue; }
                break;
            }
        }
        self.expect(&Token::RBrace)?;
        self.leave();
        Ok(Expr::MapConstructor(entries))
    }

    /// XPath 3.1 §3.1.7 inline function expression
    /// `function ( ($p as Type, …)? ) (as Type)? { body }`.  Parameter
    /// and return-type annotations are accepted and discarded — the
    /// dynamic evaluator is untyped, so they impose no runtime check.
    fn parse_inline_function(&mut self) -> Result<Expr> {
        use crate::xpath::ast::{FunctionSig, ItemType, Occurrence, SequenceType};
        let item_star = || SequenceType { item: ItemType::Any, occurrence: Occurrence::ZeroOrMore };
        self.consume(); // function
        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        let mut param_types = Vec::new();
        if self.peek() != &Token::RParen {
            loop {
                self.expect(&Token::Dollar)?;
                let name = match self.consume() {
                    Token::Name(n) => n,
                    t => return Err(self.error(format!(
                        "expected inline-function parameter name, got {t:?}"))),
                };
                params.push(name);
                // An omitted parameter type defaults to `item()*`.
                let pt = if is_name_tok(self.peek(), "as") {
                    self.consume();
                    self.parse_sequence_type()?
                } else {
                    item_star()
                };
                param_types.push(pt);
                if self.peek() == &Token::Comma { self.consume(); continue; }
                break;
            }
        }
        self.expect(&Token::RParen)?;
        let ret = if is_name_tok(self.peek(), "as") {
            self.consume();
            self.parse_sequence_type()?
        } else {
            item_star()
        };
        self.expect(&Token::LBrace)?;
        self.enter()?;
        let body = if self.peek() == &Token::RBrace {
            Expr::Sequence(Vec::new())
        } else {
            self.parse_expr()?
        };
        self.expect(&Token::RBrace)?;
        self.leave();
        Ok(Expr::InlineFunction {
            params,
            sig: Box::new(FunctionSig { params: param_types, ret }),
            body: Box::new(body),
        })
    }

    /// XPath 3.1 §3.11.2 `array { Expr }` — curly form; the contained
    /// expression's items each become one array member.
    fn parse_curly_array(&mut self) -> Result<Expr> {
        self.consume(); // array
        self.consume(); // {
        self.enter()?;
        let members = if self.peek() == &Token::RBrace {
            Vec::new()
        } else {
            vec![self.parse_expr()?]
        };
        self.expect(&Token::RBrace)?;
        self.leave();
        Ok(Expr::ArrayConstructor { members, square: false })
    }

    /// XPath 3.1 §3.11.2 `[ a, b, c ]` — square form; each
    /// comma-separated expression becomes one array member.
    fn parse_square_array(&mut self) -> Result<Expr> {
        self.consume(); // [
        self.enter()?;
        let mut members = Vec::new();
        if self.peek() != &Token::RBracket {
            members.push(self.parse_expr_single()?);
            while self.peek() == &Token::Comma {
                self.consume();
                members.push(self.parse_expr_single()?);
            }
        }
        self.expect(&Token::RBracket)?;
        self.leave();
        Ok(Expr::ArrayConstructor { members, square: true })
    }

    /// Parse a lookup key selector following `?` (postfix or unary):
    /// `*`, an NCName, an integer, or a parenthesised expression.
    fn parse_lookup_key(&mut self) -> Result<crate::xpath::ast::LookupKey> {
        use crate::xpath::ast::LookupKey;
        match self.peek().clone() {
            Token::Star => { self.consume(); Ok(LookupKey::Wildcard) }
            Token::Integer(i) => { self.consume(); Ok(LookupKey::Integer(i)) }
            Token::Name(n) => { self.consume(); Ok(LookupKey::Name(n)) }
            Token::LParen => {
                self.consume();
                self.enter()?;
                let e = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                self.leave();
                Ok(LookupKey::Expr(Box::new(e)))
            }
            other => Err(self.error(format!(
                "expected a lookup key (NCName, integer, '*', or '(expr)') after '?', got {other:?}"
            ))),
        }
    }
}

fn parse_axis_name(name: &str) -> Result<Axis> {
    Ok(match name {
        "ancestor" => Axis::Ancestor,
        "ancestor-or-self" => Axis::AncestorOrSelf,
        "attribute" => Axis::Attribute,
        "child" => Axis::Child,
        "descendant" => Axis::Descendant,
        "descendant-or-self" => Axis::DescendantOrSelf,
        "following" => Axis::Following,
        "following-sibling" => Axis::FollowingSibling,
        "namespace" => Axis::Namespace,
        "parent" => Axis::Parent,
        "preceding" => Axis::Preceding,
        "preceding-sibling" => Axis::PrecedingSibling,
        "self" => Axis::Self_,
        _ => return Err(parse_err(format!("unknown axis: {name}"))),
    })
}

/// Recognised XPath 2.0 KindTest keywords.  A `Name LParen` sequence
/// whose name is one of these is parsed as a kind-test step, not a
/// function-call step.
fn is_kind_test_name(n: &str) -> bool {
    matches!(n, "node" | "text" | "comment" | "processing-instruction"
        | "element" | "attribute" | "schema-element" | "schema-attribute"
        | "document-node")
}

fn desc_or_self_step() -> Step {
    Step {
        axis: Axis::DescendantOrSelf,
        node_test: NodeTest::AnyNode,
        predicates: vec![],
        filter: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse_xpath;

    /// Security regression: a malicious XPath with deeply nested
    /// parentheses must not stack-overflow the recursive-descent
    /// parser.  The fuzzer found this — input like
    /// `((((...((1))...))))` blows through the call chain
    /// `parse_or → parse_and → ... → parse_primary → parse_expr`
    /// once per `(`, exhausting the stack within a few hundred
    /// levels.  Post-fix the parser tracks recursion depth and
    /// returns a clean parse error instead.
    #[test]
    fn deep_paren_nesting_errors_cleanly() {
        // Rust test threads default to a 2 MB stack; the recursive-
        // descent precedence chain pushes roughly twenty frames per
        // depth level, so reaching `MAX_PARSE_DEPTH` and triggering
        // the depth check needs more headroom than that default
        // provides.  Run the parse on a thread sized for the real
        // production stack (8 MB is the main-thread default on most
        // platforms) so we're testing the depth-limit path rather
        // than the test-harness's stack ceiling.
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                let n = 10_000;
                let mut src = String::with_capacity(2 * n + 1);
                for _ in 0..n { src.push('('); }
                src.push('1');
                for _ in 0..n { src.push(')'); }
                let result = parse_xpath(&src);
                assert!(result.is_err(),
                    "expected depth-limit error, got Ok — parser accepted {n}-deep nesting");
                let msg = result.unwrap_err().to_string().to_lowercase();
                assert!(
                    msg.contains("nesting") || msg.contains("depth") || msg.contains("recursion"),
                    "expected depth-related error message, got: {msg}"
                );
            })
            .expect("spawn deep-nesting test thread")
            .join()
            .expect("deep-nesting test thread panicked");
    }

    /// Same property via predicate nesting (`[…]`), since the
    /// XPath grammar reaches `parse_expr` recursively from
    /// inside step predicates as well as parenthesised
    /// sub-expressions.
    #[test]
    fn deep_predicate_nesting_errors_cleanly() {
        // Same stack-headroom rationale as `deep_paren_nesting_errors_cleanly`.
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(|| {
                let n = 10_000;
                let mut src = String::with_capacity(2 + 2 * n);
                src.push_str("a");
                for _ in 0..n { src.push_str("[a"); }
                for _ in 0..n { src.push(']'); }
                let result = parse_xpath(&src);
                assert!(result.is_err(),
                    "expected depth-limit error, got Ok — parser accepted {n}-deep predicates");
            })
            .expect("spawn deep-predicate test thread")
            .join()
            .expect("deep-predicate test thread panicked");
    }

    /// Security regression for the XPath evaluator's algorithmic
    /// DoS where nested predicates that each contain a `//` query
    /// have N^k complexity in document size N × nesting depth k.
    /// At depth 5 against a 30-node doc the fuzzer hit 6+ seconds
    /// per evaluation; the slow-unit it produced reached 604s.
    #[test]
    #[cfg_attr(miri, ignore = "wall-clock deadline test; Miri's interpreter is 50–200× \
                               slower than native and breaches the 5s budget despite the \
                               parser's step counter aborting correctly")]
    fn nested_predicate_dos_returns_budget_error() {
        use crate::{parse_str, ParseOptions};
        use crate::xpath::XPathContext;

        // Small fixture — same shape as the fuzzer's, enough to
        // make 30^5 = 24M operations theoretically required, which
        // would take seconds without the budget.
        let fixture = r#"<catalog>
            <book><title>a</title><author>x</author></book>
            <book><title>b</title><author>y</author></book>
            <book><title>c</title><author>z</author></book>
        </catalog>"#;
        let doc = parse_str(fixture, &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        // Depth-7 nested predicate, each level enumerating
        // descendants — easily blows past 30^7 = 22B ops without
        // a budget.
        let expr = "//*[.=//*[.=//*[.=//*[.=//*[.=//*[.=//*[.=.]]]]]]]";
        let t0 = std::time::Instant::now();
        let result = ctx.eval(expr);
        let dt = t0.elapsed();
        // The real invariant is that the budget *bounds* the work and
        // returns an error (asserted below).  This wall-clock check is a
        // secondary guard against the multi-minute hang we regressed
        // against; keep it generous, since a debug build under the
        // parallel-test load of `cargo test-all` (~µs per charged step ×
        // the [`DEFAULT_MAX_EVAL_STEPS`] cap) can take tens of seconds
        // while release aborts sub-second.  60s still catches a true hang.
        assert!(
            dt < std::time::Duration::from_secs(60),
            "expected budget to abort in bounded time, took {dt:?}"
        );
        let err = result.expect_err("expected budget-exceeded error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("budget") || msg.contains("step"),
            "expected budget-related error message, got: {msg}"
        );
    }

    /// Counterpart: shallow nested predicates that are well below
    /// the budget continue to evaluate normally, returning their
    /// natural result rather than a budget error.
    #[test]
    fn shallow_predicates_evaluate_normally() {
        use crate::{parse_str, ParseOptions};
        use crate::xpath::XPathContext;
        let doc = parse_str("<r><a><b/></a><a><c/></a></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        // Two-deep predicate — easily under the budget.
        let _ = ctx.eval("//a[count(./*)=1]").expect("legit query");
    }

    /// Predicate-nesting depth ceiling (independent of the parser's
    /// stack-recursion limit): depth 8 is the threshold above which
    /// expressions like `//*[//*[//*[//*[//*[//*[//*[//*[//*[…]]]]]]]]]`
    /// are rejected at parse time — short-circuiting the evaluator's
    /// step budget which would otherwise burn ~500k charges before
    /// bailing.  See `MAX_PREDICATE_NESTING_DEPTH` in
    /// `crates/core/src/xpath/mod.rs`.
    #[test]
    fn predicate_nesting_depth_limit_rejects_at_parse_time() {
        use crate::xpath::ast::max_predicate_nesting;

        // Depth 7 — borderline pathological, but legal at parse time.
        // (The eval step budget catches it; see
        // `nested_predicate_dos_returns_budget_error`.)
        let ok = "//*[//*[//*[//*[//*[//*[//*[.='x']]]]]]]";
        let parsed = parse_xpath(ok).expect("depth-7 should parse");
        assert_eq!(max_predicate_nesting(&parsed), 7);

        // Depth 9 — beyond the ceiling; parse must reject.
        let bad = "//*[//*[//*[//*[//*[//*[//*[//*[//*[.='x']]]]]]]]]";
        let err = parse_xpath(bad).expect_err("depth-9 must be rejected");
        let msg = err.message.to_lowercase();
        assert!(
            msg.contains("predicate") && msg.contains("nesting"),
            "expected predicate-nesting error message, got: {msg}",
        );
    }

    /// Sanity: realistic nesting depths still parse fine — the
    /// depth limit is high enough not to break normal XPath.
    #[test]
    fn moderate_nesting_still_parses() {
        // 20 levels of parens around a literal — deeper than any
        // legitimate XPath I've ever seen in the wild.
        let n = 20;
        let mut src = String::new();
        for _ in 0..n { src.push('('); }
        src.push('1');
        for _ in 0..n { src.push(')'); }
        assert!(parse_xpath(&src).is_ok());
    }

    /// Error-snippet truncation must respect UTF-8 boundaries.  The
    /// `expect` path truncates the source span to keep the error
    /// message short; the fuzzer found a case where the previous
    /// byte-based cut landed inside a `ß` (a 2-byte code point)
    /// and panicked in `core::str::slice_error_fail`.
    ///
    /// We force the cut into a multi-byte char by placing a long
    /// run of unexpected name characters where the parser expects
    /// a closing token.  After parsing `1` as a function argument,
    /// the parser calls `expect(&RParen)` and instead encounters
    /// the multi-byte name token — whose span is the substring we
    /// truncate.
    ///
    /// Sweep 2/3/4-byte code points so the cut lands at a different
    /// offset within a glyph for each; a byte-based truncation
    /// would panic for at least one.
    #[test]
    fn parse_error_snippet_truncates_on_char_boundary() {
        for ch in ['ß', '中', '𝛼'] {
            let mut src = String::from("count(1 ");
            for _ in 0..50 { src.push(ch); }
            src.push(')');
            let err = parse_xpath(&src).expect_err(
                "malformed input must error, not parse",
            );
            // Reaching `expect_err` at all proves the panic is gone
            // (slice_error_fail would unwind past this frame).  The
            // message check confirms the snippet path actually ran.
            assert!(
                err.message.contains(ch),
                "error snippet should quote the offending {ch:?}; \
                 got: {msg}",
                msg = err.message,
            );
        }
    }

    /// Direct regression for the libFuzzer crash artifact
    /// `crash-4d3745946ae1f278285ab808487532d5f3ed3257`.  The
    /// input is a malformed `normalize-space(...)` call whose
    /// argument contains embedded NULs and `ß` characters at
    /// byte offsets that previously straddled the snippet cut.
    #[test]
    fn fuzz_crash_4d374594_no_panic() {
        let bytes: &[u8] = &[
            0x6e, 0x6f, 0x72, 0x6d, 0x61, 0x6c, 0x69, 0x7a, 0x65, 0x2d,
            0x73, 0x70, 0x61, 0x63, 0x65, 0x28, 0x2a, 0x22, 0x00, 0x00,
            0x63, 0x2f, 0x31, 0x2f, 0x74, 0xc3, 0x9f, 0x30, 0x30, 0x71,
            0xc3, 0x9f, 0x35, 0x61, 0x61, 0x41, 0x71, 0xc3, 0x9f, 0xc3,
            0x9f, 0x30, 0x61, 0x3a, 0x30, 0x41, 0x61, 0x71, 0x61, 0x3d,
            0xc3, 0x9f, 0x30, 0x30, 0x6e, 0x61, 0xc3, 0x9f, 0x30, 0x20,
            0x32, 0x2c, 0x61, 0x3c, 0x6a, 0x3a, 0x02, 0x00, 0x71, 0xc3,
            0x9f, 0x30, 0x6e, 0x61, 0x41, 0x71, 0xc3, 0x9f, 0x21, 0x22,
            0x29,
        ];
        let src = std::str::from_utf8(bytes).expect("fuzz input is valid UTF-8");
        let _ = parse_xpath(src); // must not panic; error vs. ok both fine
    }

    // ── XPath 2.0 grammar gate ─────────────────────────────────────

    fn parse_2_0(src: &str) -> super::super::Result<super::super::ast::Expr> {
        let mut opts = super::super::XPathOptions::default();
        opts.xpath_2_0 = true;
        super::super::parse_xpath_with(src, &opts)
    }

    /// `if (cond) then a else b` parses only when XPath 2.0 mode is
    /// on; the same expression must be rejected in default (1.0) mode
    /// so 1.0 stylesheets don't silently sprout 2.0 semantics.
    #[test]
    fn if_then_else_is_2_0_only() {
        // 1.0 mode: `if (1) then 2 else 3` should fail because
        // unquoted `if` is parsed as an XPath name test and the
        // surrounding `(...)` would be a function-call argument list.
        let r1 = parse_xpath("if (1) then 2 else 3");
        assert!(r1.is_err(), "XPath 1.0 must reject `if (...) then ... else ...`");

        // 2.0 mode: same source parses cleanly.
        let r2 = parse_2_0("if (1) then 2 else 3");
        assert!(r2.is_ok(), "XPath 2.0 must accept if-then-else; got {:?}", r2);
        use super::super::ast::Expr;
        assert!(matches!(r2.unwrap(), Expr::IfThenElse { .. }));
    }

    /// `for $v in 1 to 3 return $v * $v` parses only when XPath 2.0
    /// mode is on.  (`1 to 3` itself isn't supported yet — the grammar
    /// just needs `for $v in <expr> return <expr>` to work; the test
    /// uses a path expression instead.)
    #[test]
    fn for_return_is_2_0_only() {
        // Use a path so we don't depend on `to` (range) being implemented.
        assert!(parse_xpath("for $v in /a return $v").is_err(),
            "XPath 1.0 must reject `for $v in ... return ...`");
        let r = parse_2_0("for $v in /a return $v");
        assert!(r.is_ok(), "XPath 2.0 must accept for-return; got {:?}", r);
        use super::super::ast::Expr;
        let Expr::For { bindings, .. } = r.unwrap() else { panic!("expected For") };
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].0, "v");
    }
}
