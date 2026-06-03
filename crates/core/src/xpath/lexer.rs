use crate::charsets::{ASCII_NCNAME, NC, NS, is_name_char_unicode, is_name_start_char};
use crate::error::{ErrorDomain, ErrorLevel, XmlError};

pub type LexResult<T> = std::result::Result<T, XmlError>;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    ColonColon,
    /// `:=` — XPath 3.0 `let`-binding assignment operator.
    ColonEq,
    /// `:` — emitted only when the lone form appears (without a
    /// trailing `:` for the axis `::`).  Most parser positions
    /// will reject it; the wildcard-name path (`*:NCName`) is the
    /// one place it's legal in XPath 2.0.
    Colon,
    Slash,
    DoubleSlash,
    Dot,
    DotDot,
    At,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Pipe,
    /// `||` — XPath 3.0 string-concatenation operator.  Equivalent
    /// to `fn:concat($a, $b)` after each operand is atomised to a
    /// single xs:string.
    PipePipe,
    /// `<<` — XPath 2.0 node-before (document order) operator.
    /// `$a << $b` is true iff `$a` precedes `$b` in document order.
    LtLt,
    /// `>>` — XPath 2.0 node-after (document order) operator.
    GtGt,
    /// `=>` — XPath 3.1 arrow operator.  `e => f(x, y)` is sugar
    /// for `f(e, x, y)`.  The lexer emits this distinct from
    /// `Ge` (`>=`) so the parser can distinguish `e >= x` from
    /// `e => x()`.
    Arrow,
    /// `!` — XPath 3.0 simple-map operator.  `E1 ! E2` evaluates
    /// `E2` with each item produced by `E1` as the context item
    /// (position, size set per the sequence).  Result is the
    /// concatenation in iteration order, NOT document order.
    Bang,
    Plus,
    Minus,
    Star,
    /// `?` — XPath 2.0 occurrence indicator (zero or one) used in
    /// SequenceType / SingleType.  Not valid in XPath 1.0 syntax;
    /// the parser only consumes it where the grammar allows.
    Question,
    /// `#` — XPath 3.1 named function reference separator (`name#arity`).
    Hash,
    /// `{` — XPath 3.1 try/catch braces, plus map constructors.
    LBrace,
    /// `}` — matching closer for `{`.
    RBrace,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Dollar,
    Name(String),
    Literal(String),
    /// Integer literal — no `.` and no exponent (`42`), an `xs:integer`.
    /// Carries the parsed `i64`; a literal too large for `i64` lexes as
    /// [`Token::Decimal`] instead (the engine's numerics are `f64`-backed).
    Integer(i64),
    /// Decimal literal — a `.` but no exponent (`3.14`, `.5`, `5.`),
    /// an `xs:decimal`.  Parsed from the lexical form to preserve
    /// exact value (`0.1` is exactly 1/10, not the f64 nearest).
    Decimal(rust_decimal::Decimal),
    /// Numeric literal carrying an exponent (`1.5e0`) — an xs:double.
    Double(f64),
    Eof,
}

fn xpath_err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

/// Source byte range `(start, end)` for one token — used by the
/// parser to point errors at the right substring of the original
/// expression text.  Ranges are half-open: `&src[start..end]`
/// reconstructs the lexeme verbatim.
pub type Span = (usize, usize);

/// Tokenize an XPath 1.0 expression using strict spec rules.
/// Number literals match XPath 1.0 § 3.5 (no exponent notation).
/// Returns the token stream paired with each token's source span;
/// use [`tokenize_only`] if spans aren't needed.
#[allow(dead_code)]
pub fn tokenize(src: &str) -> LexResult<(Vec<Token>, Vec<Span>)> {
    tokenize_with(src, false)
}

/// Drop the span vector — convenient for callers that only need
/// the token stream (most internal tests).
#[allow(dead_code)]
pub fn tokenize_only(src: &str) -> LexResult<Vec<Token>> {
    tokenize(src).map(|(t, _)| t)
}

/// Like [`tokenize`] but with an optional compat-mode escape hatch.
/// When `allow_exponent` is `true`, the number-literal recogniser
/// also accepts `[eE][+-]?[0-9]+` — matching libxml2's lenient
/// behaviour.  XPath 1.0 § 3.5 does not allow this; XPath 2.0 does.
pub fn tokenize_with(src: &str, allow_exponent: bool) -> LexResult<(Vec<Token>, Vec<Span>)> {
    let bytes = src.as_bytes();
    let mut pos = 0;
    let mut tokens = Vec::new();
    let mut spans:  Vec<Span> = Vec::new();
    // Token-start cursor, captured at the top of each loop iteration
    // so every `emit!` below can stamp the (start, pos) range for
    // the just-finished token without having to thread `start`
    // through each match arm.  The initial 0 is overwritten before
    // any read (each iteration assigns before the first emit), but
    // the compiler can't see that across the macro expansion.
    #[allow(unused_assignments)]
    let mut tok_start: usize = 0;

    macro_rules! peek {
        () => { bytes.get(pos).copied().unwrap_or(0) };
        ($n:expr) => { bytes.get(pos + $n).copied().unwrap_or(0) };
    }
    macro_rules! advance { () => {{ pos += 1; }}; }
    macro_rules! emit {
        ($t:expr) => {{
            tokens.push($t);
            spans.push((tok_start, pos));
        }};
    }

    while pos < bytes.len() {
        // Skip whitespace and XPath 2.0 comments (`(: … :)`).
        // Comments nest; we scan with a depth counter so
        // `(: outer (: inner :) :)` consumes correctly.
        loop {
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos + 1 < bytes.len() && bytes[pos] == b'(' && bytes[pos + 1] == b':' {
                pos += 2;
                let mut depth: u32 = 1;
                while pos + 1 < bytes.len() && depth > 0 {
                    if bytes[pos] == b'(' && bytes[pos + 1] == b':' {
                        depth += 1;
                        pos += 2;
                    } else if bytes[pos] == b':' && bytes[pos + 1] == b')' {
                        depth -= 1;
                        pos += 2;
                    } else {
                        pos += 1;
                    }
                }
                if depth != 0 {
                    return Err(xpath_err("unterminated XPath 2.0 comment `(: … :)`"));
                }
                continue;
            }
            break;
        }
        if pos >= bytes.len() {
            break;
        }
        tok_start = pos;

        let b = peek!();

        // Number: digit or '.' followed by digit
        if b.is_ascii_digit() || (b == b'.' && peek!(1).is_ascii_digit()) {
            let start = pos;
            while pos < bytes.len() && bytes[pos].is_ascii_digit() { pos += 1; }
            let mut has_fraction = false;
            if pos < bytes.len() && bytes[pos] == b'.' {
                has_fraction = true;
                pos += 1;
                while pos < bytes.len() && bytes[pos].is_ascii_digit() { pos += 1; }
            }
            // libxml2-compat exponent suffix.  Only consumed when the
            // immediately-following character is a digit — bare `e`
            // is a valid name-start char and must not be eaten here.
            let mut has_exponent = false;
            if allow_exponent
                && (peek!() == b'e' || peek!() == b'E')
                && (peek!(1).is_ascii_digit()
                    || ((peek!(1) == b'+' || peek!(1) == b'-')
                        && peek!(2).is_ascii_digit()))
            {
                has_exponent = true;
                pos += 1; // 'e' / 'E'
                if peek!() == b'+' || peek!() == b'-' { pos += 1; }
                while pos < bytes.len() && bytes[pos].is_ascii_digit() { pos += 1; }
            }
            let s = &src[start..pos];
            // An exponent makes the literal an xs:double (XPath 2.0
            // §3.1.1); a `.` without one makes it xs:decimal; bare
            // digits are xs:integer.  An integer literal too large for
            // `i64` falls back to a decimal `f64` — the engine's
            // integers are `i64`-backed (no arbitrary precision).
            if has_exponent {
                let n: f64 = s.parse().map_err(|_| xpath_err(format!("invalid number: {s}")))?;
                emit!(Token::Double(n));
            } else if has_fraction {
                // Parse from the lexical form for exact xs:decimal
                // value — `0.1` stores as 1/10 exactly, not the f64
                // nearest neighbour.  `from_str_exact` errors when
                // the lexical form overflows `Decimal`'s 28-digit
                // mantissa or scale (the default `from_str` would
                // silently round, turning a literal like
                // `0.000…01` (51 fractional digits, well above
                // scale-28) into `0`).  Fall back to xs:double so
                // tiny / oversize literals keep their value.
                match rust_decimal::Decimal::from_str_exact(s) {
                    Ok(d)  => emit!(Token::Decimal(d)),
                    Err(_) => {
                        let n: f64 = s.parse()
                            .map_err(|_| xpath_err(format!("invalid number: {s}")))?;
                        emit!(Token::Double(n));
                    }
                }
            } else if let Ok(i) = s.parse::<i64>() {
                emit!(Token::Integer(i));
            } else {
                // Integer too large for i64 — try xs:decimal first
                // (preserves up to ~28 digits exactly), then fall back
                // to xs:double for anything larger.  Same exact-parse
                // rule as above: a 50-digit integer literal should
                // not silently round.
                match rust_decimal::Decimal::from_str_exact(s) {
                    Ok(d)  => emit!(Token::Decimal(d)),
                    Err(_) => {
                        let n: f64 = s.parse()
                            .map_err(|_| xpath_err(format!("invalid number: {s}")))?;
                        emit!(Token::Double(n));
                    }
                }
            }
            continue;
        }

        // String literal — XPath 2.0 §3.1.1.1 treats two consecutive
        // quote characters of the same kind inside the literal as an
        // escape for a single occurrence of that quote (`'He isn''t'`
        // → `He isn't`).  Strict XPath 1.0 had no such escape but
        // `'a''b'` was a syntax error there anyway, so accepting the
        // escape in both modes only expands the set of valid inputs.
        if b == b'\'' || b == b'"' {
            let quote = b;
            advance!();
            let mut s = String::new();
            loop {
                if pos >= bytes.len() {
                    return Err(xpath_err(
                        "unterminated string literal in XPath expression"));
                }
                if bytes[pos] == quote {
                    if pos + 1 < bytes.len() && bytes[pos + 1] == quote {
                        s.push(quote as char);
                        pos += 2;
                        continue;
                    }
                    break;
                }
                // Copy the next UTF-8 code unit verbatim — building
                // the string char-by-char would mis-handle multi-byte
                // scalars by treating each byte as Latin-1.
                let chunk_start = pos;
                pos += 1;
                while pos < bytes.len() && (bytes[pos] & 0xC0) == 0x80 {
                    pos += 1;
                }
                s.push_str(&src[chunk_start..pos]);
            }
            advance!(); // closing quote
            emit!(Token::Literal(s));
            continue;
        }

        // Multi-char operators and punctuation
        match b {
            b'/' => {
                advance!();
                if peek!() == b'/' {
                    advance!();
                    emit!(Token::DoubleSlash);
                } else {
                    emit!(Token::Slash);
                }
            }
            b':' => {
                if peek!(1) == b':' {
                    pos += 2;
                    emit!(Token::ColonColon);
                } else if peek!(1) == b'=' {
                    pos += 2;
                    emit!(Token::ColonEq);
                } else {
                    // Lone `:` is only valid in the XPath 2.0
                    // `*:NCName` wildcard-name form; the parser
                    // accepts it there and errors out elsewhere.
                    advance!();
                    emit!(Token::Colon);
                }
            }
            b'.' => {
                advance!();
                if peek!() == b'.' {
                    advance!();
                    emit!(Token::DotDot);
                } else {
                    emit!(Token::Dot);
                }
            }
            b'@' => { advance!(); emit!(Token::At); }
            b',' => { advance!(); emit!(Token::Comma); }
            b'[' => { advance!(); emit!(Token::LBracket); }
            b']' => { advance!(); emit!(Token::RBracket); }
            b'(' => { advance!(); emit!(Token::LParen); }
            b')' => { advance!(); emit!(Token::RParen); }
            b'|' => {
                advance!();
                // XPath 3.0 `||` — string-concatenation operator.
                if pos < bytes.len() && bytes[pos] == b'|' {
                    advance!();
                    emit!(Token::PipePipe);
                } else {
                    emit!(Token::Pipe);
                }
            }
            b'+' => { advance!(); emit!(Token::Plus); }
            b'-' => { advance!(); emit!(Token::Minus); }
            b'*' => { advance!(); emit!(Token::Star); }
            b'?' => { advance!(); emit!(Token::Question); }
            b'{' => { advance!(); emit!(Token::LBrace); }
            b'}' => { advance!(); emit!(Token::RBrace); }
            b'$' => { advance!(); emit!(Token::Dollar); }
            b'#' => { advance!(); emit!(Token::Hash); }
            b'=' => {
                advance!();
                // XPath 3.1 `=>` — arrow operator.  `e => f(...)` is
                // sugar for `f(e, ...)` (function-call sugar that
                // reads left-to-right).
                if pos < bytes.len() && bytes[pos] == b'>' {
                    advance!();
                    emit!(Token::Arrow);
                } else {
                    emit!(Token::Eq);
                }
            }
            b'!' => {
                advance!();
                if peek!() == b'=' {
                    advance!();
                    emit!(Token::Ne);
                } else {
                    // XPath 3.0 `!` — simple-map operator (`E1 ! E2`
                    // evaluates E2 with each item of E1 as context
                    // item and concatenates the results).
                    emit!(Token::Bang);
                }
            }
            b'<' => {
                advance!();
                // XPath 2.0 `<<` — node-before (document order).
                if peek!() == b'<' { advance!(); emit!(Token::LtLt); }
                else if peek!() == b'=' { advance!(); emit!(Token::Le); }
                else { emit!(Token::Lt); }
            }
            b'>' => {
                advance!();
                // XPath 2.0 `>>` — node-after (document order).
                if peek!() == b'>' { advance!(); emit!(Token::GtGt); }
                else if peek!() == b'=' { advance!(); emit!(Token::Ge); }
                else { emit!(Token::Gt); }
            }
            _ if {
                let b = bytes[pos];
                if b < 0x80 { ASCII_NCNAME[b as usize] & NS != 0 }
                else { src[pos..].chars().next().is_some_and(is_name_start_char) }
            } => {
                let start = pos;
                loop {
                    match bytes.get(pos) {
                        None => break,
                        Some(&b) if b < 0x80 => {
                            if ASCII_NCNAME[b as usize] & (NS | NC) == 0 { break; }
                            pos += 1;
                        }
                        Some(_) => {
                            // pos is at a char boundary (we only ever
                            // advance by 1 ASCII byte or by a full
                            // char width), so chars().next() is Some.
                            let c = src[pos..].chars().next().unwrap();
                            if !is_name_char_unicode(c) { break; }
                            pos += c.len_utf8();
                        }
                    }
                }

                // Check for QName: prefix:local or prefix:*
                // (but not axis-name::, which uses ColonColon)
                if pos < bytes.len()
                    && bytes[pos] == b':'
                    && bytes.get(pos + 1).copied().unwrap_or(0) != b':'
                {
                    let after_b = bytes.get(pos + 1).copied().unwrap_or(0);
                    let after_start = if after_b < 0x80 {
                        ASCII_NCNAME[after_b as usize] & NS != 0
                    } else {
                        // pos+1 is at a char boundary because bytes[pos]
                        // is ':' (ASCII, 1 byte).
                        src[pos + 1..].chars().next().is_some_and(is_name_start_char)
                    };
                    let after_star = after_b == b'*';
                    if after_start || after_star {
                        pos += 1; // consume ':'
                        if bytes.get(pos).copied() == Some(b'*') {
                            pos += 1;
                        } else {
                            loop {
                                match bytes.get(pos) {
                                    None => break,
                                    Some(&b) if b < 0x80 => {
                                        if ASCII_NCNAME[b as usize] & (NS | NC) == 0 { break; }
                                        pos += 1;
                                    }
                                    Some(_) => {
                                        let c = src[pos..].chars().next().unwrap();
                                        if !is_name_char_unicode(c) { break; }
                                        pos += c.len_utf8();
                                    }
                                }
                            }
                        }
                    }
                }

                let name = src[start..pos].to_string();
                emit!(Token::Name(name));
            }
            _ => {
                return Err(xpath_err(format!(
                    "unexpected character {:?} in XPath expression",
                    b as char
                )));
            }
        }
    }

    tok_start = pos;
    emit!(Token::Eof);
    Ok((tokens, spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Token> {
        tokenize_only(src).expect("tokenize failed")
    }

    fn lex_err(src: &str) -> String {
        tokenize_only(src).expect_err("expected lex error").message
    }

    #[test]
    fn empty_input_yields_only_eof() {
        assert_eq!(lex(""), vec![Token::Eof]);
    }

    #[test]
    fn whitespace_only_yields_only_eof() {
        assert_eq!(lex("   \t\n\r  "), vec![Token::Eof]);
    }

    #[test]
    fn whitespace_between_tokens_is_skipped() {
        assert_eq!(
            lex("  /  foo  "),
            vec![Token::Slash, Token::Name("foo".into()), Token::Eof],
        );
    }

    // ── numbers ────────────────────────────────────────────────────────────

    #[test]
    fn integer_number() {
        assert_eq!(lex("42"), vec![Token::Integer(42), Token::Eof]);
    }

    #[test]
    fn fractional_number_with_leading_digits() {
        assert_eq!(lex("3.14"), vec![Token::Decimal("3.14".parse().unwrap()), Token::Eof]);
    }

    #[test]
    fn fractional_number_with_leading_dot() {
        assert_eq!(lex(".5"), vec![Token::Decimal("0.5".parse().unwrap()), Token::Eof]);
    }

    #[test]
    fn trailing_dot_after_digits_is_part_of_number() {
        // "5." is parsed as Token::Decimal — the trailing `.` is consumed by
        // the fractional branch even with no following digits.
        assert_eq!(lex("5."), vec![Token::Decimal("5".parse().unwrap()), Token::Eof]);
    }

    // ── string literals ────────────────────────────────────────────────────

    #[test]
    fn single_quoted_literal() {
        assert_eq!(
            lex("'hello'"),
            vec![Token::Literal("hello".into()), Token::Eof],
        );
    }

    #[test]
    fn double_quoted_literal() {
        assert_eq!(
            lex("\"hello\""),
            vec![Token::Literal("hello".into()), Token::Eof],
        );
    }

    #[test]
    fn empty_string_literal() {
        assert_eq!(lex("''"), vec![Token::Literal(String::new()), Token::Eof]);
    }

    #[test]
    fn literal_with_other_quote_inside() {
        assert_eq!(
            lex("'it\"s'"),
            vec![Token::Literal("it\"s".into()), Token::Eof],
        );
    }

    #[test]
    fn unterminated_literal_errors() {
        assert!(lex_err("'oops").contains("unterminated"));
        assert!(lex_err("\"oops").contains("unterminated"));
    }

    // ── single-char punctuation ────────────────────────────────────────────

    #[test]
    fn single_char_punctuation() {
        assert_eq!(
            lex("@,[]()|+-*$="),
            vec![
                Token::At,
                Token::Comma,
                Token::LBracket,
                Token::RBracket,
                Token::LParen,
                Token::RParen,
                Token::Pipe,
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::Dollar,
                Token::Eq,
                Token::Eof,
            ],
        );
    }

    // ── slash / double-slash ───────────────────────────────────────────────

    #[test]
    fn slash_and_double_slash() {
        assert_eq!(
            lex("/ //"),
            vec![Token::Slash, Token::DoubleSlash, Token::Eof],
        );
    }

    // ── dot / dot-dot ──────────────────────────────────────────────────────

    #[test]
    fn dot_and_dotdot() {
        assert_eq!(lex("."), vec![Token::Dot, Token::Eof]);
        assert_eq!(lex(".."), vec![Token::DotDot, Token::Eof]);
        assert_eq!(
            lex(". ..  ."),
            vec![Token::Dot, Token::DotDot, Token::Dot, Token::Eof],
        );
    }

    // ── colon / colon-colon ────────────────────────────────────────────────

    #[test]
    fn colon_colon() {
        assert_eq!(
            lex("child::"),
            vec![Token::Name("child".into()), Token::ColonColon, Token::Eof],
        );
    }

    #[test]
    fn lone_colon_tokenises_to_colon() {
        // XPath 2.0 wildcard-name (`*:NCName`) needs the bare
        // colon to flow through to the parser; downstream
        // positions reject it where it's not legal.
        assert_eq!(lex(":"), vec![Token::Colon, Token::Eof]);
    }

    // ── comparison operators ───────────────────────────────────────────────

    #[test]
    fn less_than_and_le() {
        assert_eq!(lex("<"), vec![Token::Lt, Token::Eof]);
        assert_eq!(lex("<="), vec![Token::Le, Token::Eof]);
    }

    #[test]
    fn greater_than_and_ge() {
        assert_eq!(lex(">"), vec![Token::Gt, Token::Eof]);
        assert_eq!(lex(">="), vec![Token::Ge, Token::Eof]);
    }

    #[test]
    fn not_equal() {
        assert_eq!(lex("!="), vec![Token::Ne, Token::Eof]);
    }

    #[test]
    fn lone_bang_emits_simple_map_token() {
        // XPath 3.0 — bare `!` is the simple-map operator.  The
        // lexer no longer rejects it; the parser decides whether
        // the surrounding grammar accepts a simple-map here.
        assert_eq!(lex("!"), vec![Token::Bang, Token::Eof]);
    }

    // ── names and QNames ───────────────────────────────────────────────────

    #[test]
    fn ascii_name() {
        assert_eq!(lex("foo"), vec![Token::Name("foo".into()), Token::Eof]);
    }

    #[test]
    fn name_with_internal_dot_dash_underscore() {
        // '.' inside a name still terminates it (it's not in NC); but '-'
        // and '_' continue the name.
        assert_eq!(
            lex("foo-bar_baz"),
            vec![Token::Name("foo-bar_baz".into()), Token::Eof],
        );
    }

    #[test]
    fn qname_prefix_local() {
        assert_eq!(
            lex("xs:string"),
            vec![Token::Name("xs:string".into()), Token::Eof],
        );
    }

    #[test]
    fn qname_prefix_star() {
        assert_eq!(
            lex("ns:*"),
            vec![Token::Name("ns:*".into()), Token::Eof],
        );
    }

    #[test]
    fn name_then_axis_separator_not_qname() {
        // "foo::bar" — 'foo' is a Name, then '::' is ColonColon, not a QName.
        assert_eq!(
            lex("foo::bar"),
            vec![
                Token::Name("foo".into()),
                Token::ColonColon,
                Token::Name("bar".into()),
                Token::Eof,
            ],
        );
    }

    #[test]
    fn name_followed_by_lone_colon_is_separate() {
        // After a Name, a lone ':' (not followed by another name char or '*'
        // or ':') is NOT consumed as part of a QName — the tokenizer emits
        // it as a standalone `Token::Colon` so wildcard-name parsing
        // (`*:NCName`) can pick it up downstream.
        assert_eq!(
            lex("foo: "),
            vec![Token::Name("foo".into()), Token::Colon, Token::Eof],
        );
    }

    #[test]
    fn unicode_name_start_and_continuation() {
        // "café" — 'c','a','f' ASCII; 'é' (U+00E9, 0xC3 0xA9) is a valid
        // XML NameChar (NameStartChar in fact).  Exercises the non-ASCII
        // branch in the name loop.
        assert_eq!(
            lex("café"),
            vec![Token::Name("café".into()), Token::Eof],
        );
    }

    #[test]
    fn unicode_name_start_char() {
        // Greek lowercase alpha 'α' (U+03B1) is a NameStartChar.  Exercises
        // the non-ASCII branch of the dispatch guard.
        assert_eq!(
            lex("αβ"),
            vec![Token::Name("αβ".into()), Token::Eof],
        );
    }

    // ── unexpected characters ──────────────────────────────────────────────

    #[test]
    fn unexpected_char_errors() {
        let msg = lex_err("^");
        assert!(msg.contains("unexpected"), "got {msg:?}");
    }

    // ── realistic expressions ──────────────────────────────────────────────

    #[test]
    fn full_predicate_expression() {
        // /a/b[@id='x' and position()>1]
        assert_eq!(
            lex("/a/b[@id='x' and position()>1]"),
            vec![
                Token::Slash,
                Token::Name("a".into()),
                Token::Slash,
                Token::Name("b".into()),
                Token::LBracket,
                Token::At,
                Token::Name("id".into()),
                Token::Eq,
                Token::Literal("x".into()),
                Token::Name("and".into()),
                Token::Name("position".into()),
                Token::LParen,
                Token::RParen,
                Token::Gt,
                Token::Integer(1),
                Token::RBracket,
                Token::Eof,
            ],
        );
    }

    #[test]
    fn descendant_or_self_with_qname() {
        // //ns:elem
        assert_eq!(
            lex("//ns:elem"),
            vec![
                Token::DoubleSlash,
                Token::Name("ns:elem".into()),
                Token::Eof,
            ],
        );
    }

    // ── char_at helper (indirectly) ────────────────────────────────────────

    #[test]
    fn three_byte_utf8_name() {
        // '中' (U+4E2D, 3 bytes 0xE4 0xB8 0xAD) is a NameStartChar.
        // Exercises the 0xE0..=0xEF arm in char_at.
        assert_eq!(lex("中"), vec![Token::Name("中".into()), Token::Eof]);
    }

    #[test]
    fn four_byte_utf8_name() {
        // '𝛼' (U+1D6FC, 4 bytes 0xF0 0x9D 0x9B 0xBC) — mathematical bold
        // small alpha, in [#x10000-#xEFFFF], a NameStartChar.  Exercises
        // the 0xF0..=0xF7 arm in char_at.
        assert_eq!(lex("𝛼"), vec![Token::Name("𝛼".into()), Token::Eof]);
    }

    #[test]
    fn qname_local_terminated_by_non_ascii_non_namechar() {
        // QName local part starts with a NameStartChar then hits a non-ASCII
        // non-NameChar — exercises the `_ => break` arm in the QName local
        // loop's non-ASCII branch.  Whole expression fails at the arrow.
        let msg = lex_err("ns:foo→");
        assert!(msg.contains("unexpected"), "got {msg:?}");
    }

    #[test]
    fn qname_with_unicode_local_part() {
        // Non-ASCII local part forces the QName lookahead to go through
        // char_at (the after_b >= 0x80 branch).
        assert_eq!(
            lex("ns:中"),
            vec![Token::Name("ns:中".into()), Token::Eof],
        );
    }

    #[test]
    fn name_terminated_by_non_ascii_non_namechar() {
        // '→' (U+2192, 3 bytes 0xE2 0x86 0x92) is NOT a NameChar.  Exercises
        // the non-ASCII `_ => break` arm in the name loop: char_at returns
        // Some((c, _)) but is_name_char_unicode(c) is false, so we break,
        // emit Name("foo"), re-enter the outer loop, and then '→' is also
        // not a NameStartChar so we hit the catch-all unexpected-char error.
        let msg = lex_err("foo→");
        assert!(msg.contains("unexpected"), "got {msg:?}");
    }

    #[test]
    fn four_byte_utf8_in_literal() {
        // "𝛼" is U+1D6FC, 4 bytes in UTF-8 (0xF0 0x9D 0x9B 0xBC).  Inside a
        // string literal the bytes pass through unchanged, but this still
        // covers the 4-byte len path the lexer would take if such a char
        // appeared in a name position.
        assert_eq!(
            lex("'𝛼'"),
            vec![Token::Literal("𝛼".into()), Token::Eof],
        );
    }
}
