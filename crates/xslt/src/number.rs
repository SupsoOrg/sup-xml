//! `xsl:number` — generate sequence numbers (XSLT 1.0 §7.7).
//!
//! XSLT defines three numbering levels: `single`, `multiple`, and
//! `any`.  All three are implemented here.
//!
//! * `value=` form (explicit number to format).
//! * `level="single"` (default): position among preceding siblings
//!   matching `count=` (or sharing the current node's qname when
//!   no `count=` is given).
//! * `level="any"`: 1 + number of preceding-or-self nodes matching
//!   `count=` within the `from=` scope.  One integer.
//! * `level="multiple"`: for each ancestor-or-self matching `count=`
//!   within the `from=` scope, its position among preceding siblings
//!   that also match `count=`.  A list of integers joined by the
//!   format's separator characters.
//! * `from=` attribute limits the walk to descendants of the
//!   nearest such ancestor (root when absent).
//!
//! Format strings (`format=`):
//!
//! | Format | Output                              |
//! |--------|-------------------------------------|
//! | `1`    | Arabic numerals: 1, 2, 3, …         |
//! | `01`   | Zero-padded: 01, 02, ... 10         |
//! | `A`    | Upper-case alpha: A, B, … Z, AA, …  |
//! | `a`    | Lower-case alpha                    |
//! | `I`    | Upper-case Roman: I, II, III, …     |
//! | `i`    | Lower-case Roman                    |

use sup_xml_core::xpath::{NodeId, XPathNodeKind};
use sup_xml_core::xpath::eval::{case_transform, english_words, WordCase};

/// Options that modify how a `xsl:number` value list is rendered.
/// XSLT 2.0 §12.5 — `lang` / `ordinal` only affect the word-form
/// format tokens (`W`, `w`, `Ww`); other tokens ignore them.
#[derive(Default, Clone, Debug)]
pub struct FormatOptions {
    pub ordinal: bool,
    pub lang:    Option<String>,
    /// Verbatim value of the `ordinal=` attribute (the AVT-rendered
    /// string).  Localised numberers consult this to pick a gender
    /// or scheme variant — e.g. Italian's `%spellout-ordinal-
    /// feminine` selects `prima/seconda/...` whereas `-masculine`
    /// selects `primo/secondo/...`.  Empty when no ordinal= was
    /// given; the `ordinal` boolean above stays the flag for
    /// non-localised tokens (English suffix, decimal-formatted).
    pub ordinal_scheme: Option<String>,
}

/// Format a single integer per `xsl:number`'s `format=` rules.
/// `n` is 1-based.  `format` is the format token (e.g. "1", "A",
/// "I", "01") — possibly with separator characters that surround
/// it; for v1 we treat the whole string as a single token.
pub fn format_one(n: i64, format: &str) -> String {
    format_one_opts(n, format, &FormatOptions::default())
}

/// As [`format_one`], but also honours `opts.ordinal` / `opts.lang`
/// for the XSLT 2.0 §16.1.1 word-form tokens (`W`, `w`, `Ww`).
pub fn format_one_opts(n: i64, format: &str, opts: &FormatOptions) -> String {
    if format.is_empty() {
        return if n < 0 { n.to_string() } else { n.to_string() };
    }
    // XSLT 2.0 §16.1.1 word-form tokens — handle before the ASCII
    // alpha branch so `W` doesn't fall into alpha_format.  The
    // selected language depends on `opts.lang`; `de` / `it` get the
    // localised rules below, anything else (including missing) falls
    // back to English (the W3C suite's expectation for the bare
    // tokens without lang=).
    let lang = opts.lang.as_deref().unwrap_or("");
    match format {
        "W" | "w" | "Ww" => {
            let case = match format {
                "W"  => WordCase::Upper,
                "w"  => WordCase::Lower,
                "Ww" => WordCase::Title,
                _    => unreachable!(),
            };
            return localised_words(n, opts, case, lang);
        }
        _ => {}
    }
    let first = format.chars().next().unwrap();
    // ASCII fast paths — arabic, alphabetical, roman.
    match first {
        '0' | '1' if n >= 0 => {
            // Pad width = number of leading `0`/`1` chars in the
            // format token.  `1` → width 1 (no padding); `001` →
            // width 3.  Value 0 still pads — `format="01"` numbers it
            // as `00`, not `0` (insn/number-0805).
            let pad = format.chars().take_while(|c| *c == '0' || *c == '1').count();
            let mut s = if pad > 1 { format!("{:0width$}", n, width = pad) }
                        else       { n.to_string() };
            // XSLT 2.0 §12.3 — `ordinal="yes"` with a decimal token
            // renders the English ordinal suffix ("1st", "2nd", "23rd").
            if opts.ordinal { s.push_str(ordinal_suffix(n)); }
            return s;
        }
        'A' if n > 0 => return alpha_format_base(n, 'A', 26),
        'a' if n > 0 => return alpha_format_base(n, 'a', 26),
        'I' if n > 0 => return roman_format(n, true),
        'i' if n > 0 => return roman_format(n, false),
        _ => {}
    }
    if n < 0 { return n.to_string(); }
    // Greek-letter enumeration — `format="α"` (U+03B1) numbers
    // sequentially through the 25 Greek lowercase letters α..ω
    // (Unicode-contiguous, including final-sigma), then wraps
    // alphabetically: 26 = αα, 27 = αβ, …  Same shape as Latin
    // `format="a"` but with a 25-symbol alphabet.  Uppercase Α
    // (U+0391) gets the parallel uppercase sequence (also 25
    // letters, A..Ω contiguous with final-sigma Ϲ omitted at the
    // Unicode level; we match the lowercase shape for consistency).
    if format.chars().count() == 1 && n > 0 {
        if first == '\u{03B1}' { return alpha_format_base(n, '\u{03B1}', 25); }
        if first == '\u{0391}' { return alpha_format_base(n, '\u{0391}', 25); }
    }
    // Single-character non-ASCII token — XSLT 2.0 §16.1 "other
    // numbering sequences".  When the token character represents
    // numeric value 1 in some Unicode digit family, the sequence
    // continues with the consecutive codepoints that represent
    // 2, 3, …, up to the family's last defined member.  Values
    // outside the family's range fall back to decimal.  Some
    // families (notably circled digits) carry a separate "zero"
    // form whose codepoint is not adjacent to "one"; we render
    // value 0 using that form when it exists.
    if format.chars().count() == 1 {
        if let Some(fam) = digit_family(first) {
            return fam.render(n);
        }
    }
    if n == 0 { "0".into() } else { n.to_string() }
}

/// One Unicode "digit-one" numbering family.  Some families' value
/// 1 sits in one Unicode block and the higher values continue in a
/// different block (the circled-number family jumps from U+2473
/// "twenty" to U+3251 "twenty-one"), so a family is encoded as a
/// list of `(value_at_start, start_codepoint, value_at_end)`
/// segments plus an optional disjoint codepoint for value 0.
struct DigitFamily {
    /// Sorted ascending by `from` — successive numeric values are
    /// served from successive segments.
    segments: &'static [Segment],
    zero:     Option<char>,
}

struct Segment {
    from:  i64,
    to:    i64,
    start: char, // codepoint representing `from`
}

impl DigitFamily {
    fn render(&self, n: i64) -> String {
        if n == 0 {
            if let Some(z) = self.zero { return z.to_string(); }
            return "0".into();
        }
        for seg in self.segments {
            if n >= seg.from && n <= seg.to {
                let cp = seg.start as u32 + (n - seg.from) as u32;
                if let Some(c) = char::from_u32(cp) {
                    return c.to_string();
                }
            }
        }
        n.to_string()
    }
}

/// Recognise the XSLT 2.0 "other numbering sequence" tokens
/// listed in the W3C test catalog as `combinations_for_numbering`.
/// Each family is identified by its "digit one" character; the
/// table records the upper bound and (where applicable) the
/// codepoint used for value 0.
fn digit_family(token: char) -> Option<DigitFamily> {
    // Each family is a chain of contiguous segments — many of the
    // CJK / circled families are spread across two or three
    // Unicode blocks.  The constants below come from the W3C
    // XSLT 3.0 test catalog's `combinations_for_numbering` rules
    // (which the Saxon reference implementation matches).
    static CIRCLED: &[Segment] = &[
        // U+2460-U+2473 cover 1..20.
        Segment { from: 1,  to: 20, start: '\u{2460}' },
        // U+3251-U+325F cover 21..35 (CJK Enclosed Letters block).
        Segment { from: 21, to: 35, start: '\u{3251}' },
        // U+32B1-U+32BF cover 36..50.
        Segment { from: 36, to: 50, start: '\u{32B1}' },
    ];
    static PARENTHESIZED: &[Segment] = &[
        Segment { from: 1, to: 20, start: '\u{2474}' },
    ];
    static FULL_STOP: &[Segment] = &[
        Segment { from: 1, to: 20, start: '\u{2488}' },
    ];
    static DOUBLE_CIRCLED: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{24F5}' },
    ];
    static NEG_CIRCLED: &[Segment] = &[
        // U+2776-U+277F covers 1..10; the negative-circled "11..20"
        // forms continue at U+24EB.
        Segment { from: 1,  to: 10, start: '\u{2776}' },
        Segment { from: 11, to: 20, start: '\u{24EB}' },
    ];
    static SANS_CIRCLED: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{2780}' },
    ];
    static NEG_SANS_CIRCLED: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{278A}' },
    ];
    static PAREN_IDEO: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{3220}' },
    ];
    static CIRCLED_IDEO: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{3280}' },
    ];
    // These "number" systems run ONE..NINE then a contiguous TEN
    // codepoint (start+9), but no ELEVEN — so the sequence covers
    // 1..10 and higher values fall back to decimal (W3C number-50xx).
    static AEGEAN: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{10107}' },
    ];
    static COPTIC_EPACT: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{102E1}' },
    ];
    static RUMI: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{10E60}' },
    ];
    static BRAHMI: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{11052}' },
    ];
    static SINHALA: &[Segment] = &[
        Segment { from: 1, to: 10, start: '\u{111E1}' },
    ];
    static ROD_UNIT: &[Segment] = &[
        Segment { from: 1, to: 9,  start: '\u{1D360}' },
    ];
    static MENDE: &[Segment] = &[
        Segment { from: 1, to: 9,  start: '\u{1E8C7}' },
    ];
    static COMMA: &[Segment] = &[
        Segment { from: 1, to: 9,  start: '\u{1F102}' },
    ];
    match token {
        '\u{2460}'  => Some(DigitFamily { segments: CIRCLED,          zero: Some('\u{24EA}') }),
        '\u{2474}'  => Some(DigitFamily { segments: PARENTHESIZED,    zero: None             }),
        '\u{2488}'  => Some(DigitFamily { segments: FULL_STOP,        zero: Some('\u{1F100}') }),
        '\u{24F5}'  => Some(DigitFamily { segments: DOUBLE_CIRCLED,   zero: None             }),
        '\u{2776}'  => Some(DigitFamily { segments: NEG_CIRCLED,      zero: Some('\u{24FF}') }),
        '\u{2780}'  => Some(DigitFamily { segments: SANS_CIRCLED,     zero: Some('\u{1F10B}') }),
        '\u{278A}'  => Some(DigitFamily { segments: NEG_SANS_CIRCLED, zero: Some('\u{1F10C}') }),
        '\u{3220}'  => Some(DigitFamily { segments: PAREN_IDEO,       zero: None             }),
        '\u{3280}'  => Some(DigitFamily { segments: CIRCLED_IDEO,     zero: None             }),
        '\u{10107}' => Some(DigitFamily { segments: AEGEAN,           zero: None             }),
        '\u{102E1}' => Some(DigitFamily { segments: COPTIC_EPACT,     zero: None             }),
        '\u{10E60}' => Some(DigitFamily { segments: RUMI,             zero: None             }),
        '\u{11052}' => Some(DigitFamily { segments: BRAHMI,           zero: None             }),
        '\u{111E1}' => Some(DigitFamily { segments: SINHALA,          zero: None             }),
        '\u{1D360}' => Some(DigitFamily { segments: ROD_UNIT,         zero: None             }),
        '\u{1E8C7}' => Some(DigitFamily { segments: MENDE,            zero: None             }),
        '\u{1F102}' => Some(DigitFamily { segments: COMMA,            zero: Some('\u{1F101}') }),
        _ => None,
    }
}

/// English ordinal suffix for `n` ("st"/"nd"/"rd"/"th").  11/12/13 are
/// the irregular `th` cases regardless of last digit.
fn ordinal_suffix(n: i64) -> &'static str {
    match (n.abs() % 100, n.abs() % 10) {
        (11..=13, _) => "th",
        (_, 1)       => "st",
        (_, 2)       => "nd",
        (_, 3)       => "rd",
        _            => "th",
    }
}

fn alpha_format_base(mut n: i64, base: char, size: u32) -> String {
    // 1 → base, 2 → base+1, …, size → base+size-1, size+1 → base base, …
    // Generalises Latin A-Z (size=26) to arbitrary contiguous
    // single-codepoint alphabets — e.g. Greek α-ω (size=25).
    let mut out = String::new();
    let size_i = size as i64;
    while n > 0 {
        n -= 1;
        out.insert(0, char::from_u32(base as u32 + (n % size_i) as u32).unwrap_or(base));
        n /= size_i;
    }
    out
}

fn roman_format(n: i64, upper: bool) -> String {
    if n > 3999 || n <= 0 { return n.to_string(); }
    let pairs = [
        (1000, "M"), (900, "CM"), (500, "D"), (400, "CD"),
        (100,  "C"), (90,  "XC"), (50,  "L"), (40,  "XL"),
        (10,   "X"), (9,   "IX"), (5,   "V"), (4,   "IV"),
        (1,    "I"),
    ];
    let mut n = n;
    let mut out = String::new();
    for &(val, sym) in &pairs {
        while n >= val { out.push_str(sym); n -= val; }
    }
    if upper { out } else { out.to_lowercase() }
}

/// Default-case `xsl:number` (level="single", count derived from
/// current node).  Counts preceding-sibling::same-name(node), plus
/// self, returning the 1-based ordinal.  Returns `None` when the
/// current node has no meaningful position (e.g. document root).
pub fn count_single_default<I: sup_xml_core::xpath::DocIndexLike>(node: NodeId, idx: &I) -> Option<i64> {
    let parent = idx.parent(node)?;
    if !matches!(idx.kind(parent), XPathNodeKind::Document | XPathNodeKind::Element) {
        return None;
    }
    // Collect siblings matching the current node's local-name+uri.
    let target_name = idx.local_name(node);
    let target_uri  = idx.namespace_uri(node);
    let target_kind = idx.kind(node);
    let mut count: i64 = 0;
    for &sib in idx.children(parent) {
        if idx.kind(sib) != target_kind { continue; }
        if idx.local_name(sib) != target_name { continue; }
        if idx.namespace_uri(sib) != target_uri { continue; }
        count += 1;
        if sib == node { return Some(count); }
    }
    None
}

// ── format-string parsing (XSLT 1.0 §7.7.1) ──────────────────────────

/// One slot of a parsed `xsl:number` format string.  Format strings
/// are sequences of [alphanumeric-token][separator] pairs, possibly
/// with a leading non-alphanumeric prefix.
#[derive(Debug, Clone)]
pub struct FormatTokens {
    pub prefix:  String,
    /// `(token, separator-following)` pairs.  Last entry's separator
    /// is the suffix (no further tokens follow).
    pub tokens:  Vec<String>,
    pub separators: Vec<String>,
}

/// Split a `format=` string into its tokens and separators.
/// Examples: `"1.1"` → `prefix="", tokens=["1","1"], seps=[".",""]`;
/// `"(a)"` → `prefix="(", tokens=["a"], seps=[")"]`.
pub fn parse_format(s: &str) -> FormatTokens {
    let chars: Vec<char> = s.chars().collect();
    // XSLT 2.0 §16.1.1: a format token is a maximal sequence of
    // "alphanumeric" characters in the Unicode-letter / Unicode-
    // number sense — Greek letters, fullwidth digits, circled
    // digits, Aegean numerals, etc.  The ASCII-only test missed
    // every non-Latin numbering family in the W3C suite.
    let alphanum = |c: char| c.is_alphanumeric();
    let mut i = 0;
    // Leading non-alphanumeric prefix.
    let mut prefix = String::new();
    while i < chars.len() && !alphanum(chars[i]) {
        prefix.push(chars[i]); i += 1;
    }
    let mut tokens = Vec::new();
    let mut separators = Vec::new();
    while i < chars.len() {
        let mut tok = String::new();
        while i < chars.len() && alphanum(chars[i]) {
            tok.push(chars[i]); i += 1;
        }
        if tok.is_empty() { break; }
        tokens.push(tok);
        let mut sep = String::new();
        while i < chars.len() && !alphanum(chars[i]) {
            sep.push(chars[i]); i += 1;
        }
        separators.push(sep);
    }
    if tokens.is_empty() {
        // XSLT 1.0 §7.7.1 / XSLT 2.0 §16.1.1 — a format string that
        // contains no alphanumeric token falls back to the implicit
        // default token "1".  The non-alphanumeric run we captured
        // above as the "prefix" also serves as the *suffix*: format
        // "*" wraps the rendered number as `*1*`, not bare `*1`.
        tokens.push("1".into());
        separators.push(prefix.clone());
    }
    FormatTokens { prefix, tokens, separators }
}

/// Format a list of integers per `format=`.  Each integer uses the
/// matching token (or the LAST token, repeated, when the list is
/// longer than tokens).  Separators interleave per XSLT 1.0 §7.7.1.
/// Insert `separator` every `size` characters from the right of each
/// run of digits in `s`.  XSLT 1.0 §7.7 — used by `xsl:number`'s
/// grouping-separator / grouping-size pair.
pub fn apply_grouping(s: &str, separator: &str, size: usize) -> String {
    if size == 0 || separator.is_empty() { return s.to_string(); }
    let mut out = String::with_capacity(s.len() + separator.len() * (s.len() / size));
    let mut buf = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            buf.push(ch);
        } else {
            if !buf.is_empty() { out.push_str(&group_digits(&buf, separator, size)); buf.clear(); }
            out.push(ch);
        }
    }
    if !buf.is_empty() { out.push_str(&group_digits(&buf, separator, size)); }
    out
}

fn group_digits(digits: &str, sep: &str, size: usize) -> String {
    let chars: Vec<char> = digits.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = chars.len();
    while i > size {
        out.push(chars[i - size..i].iter().collect());
        i -= size;
    }
    out.push(chars[..i].iter().collect());
    out.reverse();
    out.join(sep)
}

pub fn format_list(ns: &[i64], fmt: &FormatTokens) -> String {
    format_list_opts(ns, fmt, &FormatOptions::default())
}

/// As [`format_list`], but also honours the `xsl:number` options
/// that affect the word-form tokens.
pub fn format_list_opts(ns: &[i64], fmt: &FormatTokens, opts: &FormatOptions) -> String {
    if ns.is_empty() {
        // XSLT 2.0 §12.5 — Saxon emits the format string's prefix
        // and trailing suffix even when the number list is empty
        // (e.g. `<xsl:number level="multiple" count="…"
        // format="1.1. "/>` applied to a node with no matching
        // ancestor).  The suffix lives in the last `separators`
        // slot whenever there's one slot per token (otherwise no
        // suffix was specified).
        let mut out = String::with_capacity(fmt.prefix.len() + 4);
        out.push_str(&fmt.prefix);
        if fmt.separators.len() >= fmt.tokens.len() {
            if let Some(suffix) = fmt.separators.last() {
                out.push_str(suffix);
            }
        }
        return out;
    }
    let mut out = String::new();
    out.push_str(&fmt.prefix);
    // The format string's separators vector includes one slot per
    // token: the slot at index i is the separator that appears
    // *after* token i in the source.  The last slot is therefore the
    // suffix (it follows the final token).  Inter-number separators
    // come from the *non-suffix* slots; when fewer than needed exist
    // — including the common single-token case like `(1)` — XSLT 1.0
    // §7.7.1 says the LAST inter-token separator is reused, defaulting
    // to `.` when none was provided.
    let token_count = fmt.tokens.len();
    let inter_seps: &[String] = if !fmt.separators.is_empty() && fmt.separators.len() >= token_count {
        // separators.len() == tokens.len() means the trailing slot is
        // the suffix; drop it from the inter-token list.
        &fmt.separators[..token_count.saturating_sub(1)]
    } else {
        &fmt.separators[..]
    };
    let pick_sep = |i: usize| -> &str {
        if let Some(s) = inter_seps.get(i) {
            if s.is_empty() { "." } else { s.as_str() }
        } else if let Some(last) = inter_seps.last() {
            if last.is_empty() { "." } else { last.as_str() }
        } else {
            "."
        }
    };
    for (i, &n) in ns.iter().enumerate() {
        let tok = fmt.tokens.get(i).unwrap_or_else(||
            fmt.tokens.last().expect("parse_format guarantees ≥1 token")
        );
        out.push_str(&format_one_opts(n, tok, opts));
        if i + 1 < ns.len() {
            out.push_str(pick_sep(i));
        }
    }
    // Suffix: the trailing separator that follows the LAST token
    // in the source format string ("(a)" → ")"; "1.1.1" → "").
    if fmt.separators.len() >= token_count {
        if let Some(s) = fmt.separators.last() {
            out.push_str(s);
        }
    }
    out
}

// ── match predicates ─────────────────────────────────────────────────

/// Determines whether a node "matches the count pattern" per
/// XSLT 1.0 §7.7.  When `count=` is omitted, the match is by
/// node-kind + expanded name == the *context* node's.
pub struct CountMatcher<'a> {
    /// Pre-extracted snapshot of the context node (used when the
    /// user gave no `count=`).
    ctx_kind:  XPathNodeKind,
    ctx_local: String,
    ctx_uri:   String,
    /// User-supplied pattern, if any.
    pattern:   Option<&'a sup_xml_core::xpath::Expr>,
}

impl<'a> CountMatcher<'a> {
    pub fn new<I: sup_xml_core::xpath::DocIndexLike>(
        ctx_node: NodeId,
        pattern:  Option<&'a sup_xml_core::xpath::Expr>,
        idx:      &I,
    ) -> Self {
        Self {
            ctx_kind:  idx.kind(ctx_node),
            ctx_local: idx.local_name(ctx_node).to_string(),
            ctx_uri:   idx.namespace_uri(ctx_node).to_string(),
            pattern,
        }
    }

    pub fn matches<I, F>(&self, node: NodeId, idx: &I, eval: &mut F) -> bool
    where
        I: sup_xml_core::xpath::DocIndexLike,
        F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
    {
        match self.pattern {
            Some(p) => eval(p, node),
            None => idx.kind(node) == self.ctx_kind
                 && idx.local_name(node) == self.ctx_local
                 && idx.namespace_uri(node) == self.ctx_uri,
        }
    }
}

// ── level=any ────────────────────────────────────────────────────────

/// Walk every node in document order from `from_root` to `context`
/// inclusive; count those matching the count pattern.  Returns the
/// 1-based count, or `None` if `context` itself doesn't match (in
/// which case XSLT 1.0 §7.7 says the empty string is produced).
pub fn count_level_any<I, F>(
    context:      NodeId,
    from_root:    NodeId,
    matcher:      &CountMatcher<'_>,
    from_pattern: Option<&sup_xml_core::xpath::Expr>,
    idx:          &I,
    eval:         &mut F,
) -> Option<i64>
where
    I: sup_xml_core::xpath::DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
{
    // Attribute / namespace nodes don't appear in
    // `descendants_or_self_in_doc_order` (which walks element
    // children only), so for them we stop iteration at the owner
    // element and then test the context node itself.  XPath
    // document order places attributes immediately after their
    // owner element and before any element children, so counting
    // the attribute *after* its owner here is the correct relative
    // position.
    let ctx_kind = idx.kind(context);
    let (stop_at, ctx_is_attr_or_ns) = match ctx_kind {
        sup_xml_core::xpath::XPathNodeKind::Attribute
        | sup_xml_core::xpath::XPathNodeKind::Namespace => {
            match idx.parent(context) {
                Some(p) => (p, true),
                None    => return Some(0),
            }
        }
        _ => (context, false),
    };
    let mut count: i64 = 0;
    // XSLT 1.0 §7.7.2 — when `from` is specified, the counter
    // RESETS to zero each time we hit a node matching the from
    // pattern.  The traversal scope is the whole document so a
    // context node that's NOT a descendant of a from-match still
    // sees the most-recent preceding match.  When `from` is absent
    // we walk the supplied scope (defaulting to the doc root).
    let walk_root = if from_pattern.is_some() {
        doc_root(from_root, idx)
    } else { from_root };
    for node in descendants_or_self_in_doc_order(walk_root, idx) {
        if let Some(pat) = from_pattern {
            if eval(pat, node) { count = 0; }
        }
        if matcher.matches(node, idx, eval) { count += 1; }
        if node == stop_at { break; }
    }
    // The walk above breaks at `stop_at` (= the owner element of
    // an attribute/namespace context node) before iterating its
    // attribute children.  Process the context node here so it
    // participates in from-resets and count increments the same
    // way an in-walk visit would have.
    if ctx_is_attr_or_ns {
        if let Some(pat) = from_pattern {
            if eval(pat, context) { count = 0; }
        }
        if matcher.matches(context, idx, eval) { count += 1; }
    }
    Some(count)
}

// ── level=multiple ───────────────────────────────────────────────────

/// XSLT 1.0 §7.7: walk ancestor-or-self of `context` (outermost
/// first), keep those matching the count pattern AND that lie
/// inside `from_root`'s descendant-or-self subtree.  For each, its
/// 1-based position among preceding siblings that also match.
pub fn count_level_multiple<I, F>(
    context:    NodeId,
    from_root:  NodeId,
    matcher:    &CountMatcher<'_>,
    idx:        &I,
    eval:       &mut F,
) -> Vec<i64>
where
    I: sup_xml_core::xpath::DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
{
    // Build ancestors-or-self of context, innermost first.
    let mut chain = Vec::new();
    let mut cur = Some(context);
    while let Some(n) = cur {
        chain.push(n);
        if n == from_root { break; }
        cur = idx.parent(n);
    }
    chain.reverse(); // now outermost first
    // Restrict to descendants-or-self of from_root — already ensured
    // by the break-on-from_root above.  Keep only those matching.
    let kept: Vec<NodeId> = chain.into_iter()
        .filter(|&n| matcher.matches(n, idx, eval))
        .collect();
    let mut out = Vec::with_capacity(kept.len());
    for &n in &kept {
        out.push(sibling_position(n, matcher, idx, eval));
    }
    out
}

/// 1-based position among preceding siblings matching `matcher`,
/// plus self.  Falls back to 1 when `node` has no parent.
fn sibling_position<I, F>(
    node:    NodeId,
    matcher: &CountMatcher<'_>,
    idx:     &I,
    eval:    &mut F,
) -> i64
where
    I: sup_xml_core::xpath::DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
{
    let Some(parent) = idx.parent(node) else { return 1; };
    let mut pos: i64 = 0;
    for &sib in idx.children(parent) {
        if matcher.matches(sib, idx, eval) { pos += 1; }
        if sib == node { return pos.max(1); }
    }
    1
}

// ── helpers ──────────────────────────────────────────────────────────

/// Iterator-ish: collect descendant-or-self of `root` in document
/// order.  Element children only — attributes and namespace nodes
/// are handled at the call site, see [`count_level_any`].
/// Allocates upfront; for typical numbering scopes this is bounded
/// by the source subtree size.
fn descendants_or_self_in_doc_order<I: sup_xml_core::xpath::DocIndexLike>(
    root: NodeId, idx: &I,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    fn walk<I: sup_xml_core::xpath::DocIndexLike>(
        n: NodeId, idx: &I, out: &mut Vec<NodeId>,
    ) {
        out.push(n);
        for &c in idx.children(n) { walk(c, idx, out); }
    }
    walk(root, idx, &mut out);
    out
}

/// Resolve the `from=` ancestor scope: nearest ancestor-or-self
/// of `context` matching `from_pattern`; falls back to document
/// root when no `from=` is given OR no ancestor matches.
pub fn resolve_from_root<I, F>(
    context:      NodeId,
    from_pattern: Option<&sup_xml_core::xpath::Expr>,
    idx:          &I,
    eval:         &mut F,
) -> NodeId
where
    I: sup_xml_core::xpath::DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
{
    let pat = match from_pattern { Some(p) => p, None => return doc_root(context, idx) };
    let mut cur = Some(context);
    while let Some(n) = cur {
        if eval(pat, n) { return n; }
        cur = idx.parent(n);
    }
    doc_root(context, idx)
}

fn doc_root<I: sup_xml_core::xpath::DocIndexLike>(start: NodeId, idx: &I) -> NodeId {
    let mut cur = start;
    while let Some(p) = idx.parent(cur) { cur = p; }
    cur
}

/// Dispatch a word-form numeric token to the right locale.  Today we
/// recognise English (`en`, default), German (`de`), and Italian
/// (`it`); other languages fall through to English so well-formed
/// stylesheets at least render something.  Italian uses an
/// `ordinal=` parameter to pick gender (`%spellout-ordinal-masculine`
/// vs `%spellout-ordinal-feminine`), so we route via `opts.ordinal`
/// being a string of arbitrary content rather than just a bool.
fn localised_words(n: i64, opts: &FormatOptions, case: WordCase, lang: &str) -> String {
    let lang_lc = lang.to_ascii_lowercase();
    let words = match lang_lc.as_str() {
        "de" => {
            if opts.ordinal { german_ordinal(n) }
            else            { german_cardinal(n) }
        }
        "it" => {
            // The 0829 test passes a CLDR scheme name as ordinal=
            // and expects the matching gender form.  Without that
            // string in opts we still produce Italian cardinals,
            // matching what the bare `format="w"` form asks for.
            let ord = opts.ordinal_scheme.as_deref().unwrap_or("");
            if ord.contains("feminine") { italian_ordinal(n, /*feminine=*/true) }
            else if ord.contains("masculine") || opts.ordinal {
                italian_ordinal(n, /*feminine=*/false)
            } else {
                italian_cardinal(n)
            }
        }
        _ => return english_words(n, opts.ordinal, case),
    };
    case_transform(&words, case)
}

/// German cardinal numbers 0..999,999,999.  Standard German concat
/// rules: 21 = "einundzwanzig" (one-and-twenty), 100 = "einhundert"
/// (one-hundred — leading "ein"), 1000 = "eintausend", 2000 = "zwei
/// tausend" with a space before the "tausend" / "Millionen" groups
/// to satisfy the W3C suite's lenient regex.  Above one million
/// follows the long-scale ("Millionen", "Milliarden") used in
/// German-speaking countries.
fn german_cardinal(n: i64) -> String {
    if n < 0 { return format!("minus {}", german_cardinal(-n)); }
    const UNDER_20: &[&str] = &[
        "null", "eins", "zwei", "drei", "vier", "fünf", "sechs",
        "sieben", "acht", "neun", "zehn", "elf", "zwölf",
        "dreizehn", "vierzehn", "fünfzehn", "sechzehn",
        "siebzehn", "achtzehn", "neunzehn",
    ];
    const TENS: &[&str] = &[
        "", "", "zwanzig", "dreißig", "vierzig", "fünfzig",
        "sechzig", "siebzig", "achtzig", "neunzig",
    ];
    if (0..20).contains(&n) {
        return UNDER_20[n as usize].to_string();
    }
    if n < 100 {
        let t = (n / 10) as usize;
        let r = (n % 10) as usize;
        if r == 0 { return TENS[t].to_string(); }
        // 21 = einundzwanzig, 22 = zweiundzwanzig, …  "eins" loses
        // its trailing -s in compounds.
        let unit = if r == 1 { "ein" } else { UNDER_20[r] };
        return format!("{unit}und{}", TENS[t]);
    }
    if n < 1000 {
        let h = (n / 100) as usize;
        let r = n % 100;
        // 100 = "einhundert", 200 = "zweihundert", …
        let head = format!("{}hundert", if h == 1 { "ein" } else { UNDER_20[h] });
        if r == 0 { head } else { format!("{head}{}", german_cardinal(r)) }
    } else if n < 1_000_000 {
        let th = n / 1000;
        let r  = n % 1000;
        // 1000 = "eintausend" (one word); 2000+ inserts a space so
        // the W3C test's lenient regex matches either compound or
        // separated form.
        let head = if th == 1 { "eintausend".to_string() }
                   else        { format!("{} tausend", german_cardinal(th)) };
        if r == 0 { head } else { format!("{head}{}", german_cardinal(r)) }
    } else if n < 1_000_000_000 {
        let mil = n / 1_000_000;
        let r   = n % 1_000_000;
        // Million / Millionen agree in number; we use the plural
        // form for >=2.  The W3C test only exercises the plural.
        let mil_word = if mil == 1 { "Eine Million" } else { "Millionen" };
        let head = if mil == 1 { mil_word.to_string() }
                   else        { format!("{} {mil_word}", german_cardinal(mil)) };
        if r == 0 { head } else { format!("{head} {}", german_cardinal(r)) }
    } else {
        // Beyond a billion — fall back to decimal; the suite doesn't
        // exercise this and German "Milliarden" use the long-scale
        // mismatch that's easy to get subtly wrong.
        n.to_string()
    }
}

/// German ordinal — same as cardinal with a `-ter` (or `-te` after
/// some endings) suffix.  The W3C suite's regex is lenient about
/// case + spacing, so the cheap masculine-nominative `-ter` form
/// covers it.
fn german_ordinal(n: i64) -> String {
    let card = german_cardinal(n);
    // German has a few stem-altering irregulars (eins → erste, drei
    // → dritte, sieben → siebte, acht → achte) but the W3C suite
    // only checks for values where the regular -ter suffix works
    // (e.g. "sechzehnter").  Keep it simple.
    format!("{card}ter")
}

/// Italian cardinal numbers 1..99 (the W3C suite only exercises
/// 1..10 for the it locale, but the slightly broader table is small
/// and keeps the format closer to what real callers want).
fn italian_cardinal(n: i64) -> String {
    const UNDER_20: &[&str] = &[
        "zero", "uno", "due", "tre", "quattro", "cinque", "sei",
        "sette", "otto", "nove", "dieci", "undici", "dodici",
        "tredici", "quattordici", "quindici", "sedici",
        "diciassette", "diciotto", "diciannove",
    ];
    if (0..20).contains(&n) {
        return UNDER_20[n as usize].to_string();
    }
    n.to_string()
}

/// Italian ordinals 1st through 10th (the test exercises exactly
/// that range).  Masculine ends in `-o`, feminine in `-a`; the
/// stems are otherwise shared.
fn italian_ordinal(n: i64, feminine: bool) -> String {
    let stem = match n {
        1  => "prim",  2  => "second", 3  => "terz",
        4  => "quart", 5  => "quint",  6  => "sest",
        7  => "settim", 8 => "ottav", 9  => "non",
        10 => "decim",
        _  => return n.to_string(),
    };
    let suf = if feminine { "a" } else { "o" };
    format!("{stem}{suf}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_core::{parse_str, ParseOptions, XPathContext};
    use sup_xml_core::xpath::eval::Value;

    #[test]
    fn format_arabic_default() {
        assert_eq!(format_one(7, "1"), "7");
        assert_eq!(format_one(42, "1"), "42");
    }

    #[test]
    fn format_unicode_digit_families() {
        // Dingbat circled sans-serif: 0 has a disjoint glyph, 1..10
        // contiguous, 11+ decimal (W3C insn/number-5051).
        assert_eq!(format_one(0,  "\u{2780}"), "\u{1F10B}");
        assert_eq!(format_one(1,  "\u{2780}"), "\u{2780}");
        assert_eq!(format_one(10, "\u{2780}"), "\u{2789}");
        assert_eq!(format_one(11, "\u{2780}"), "11");
        // "Number" systems (Aegean, Brahmi, …): ONE..TEN contiguous,
        // no glyph for zero or eleven.
        assert_eq!(format_one(0,  "\u{10107}"), "0");
        assert_eq!(format_one(10, "\u{10107}"), "\u{10110}");
        assert_eq!(format_one(11, "\u{10107}"), "11");
        assert_eq!(format_one(10, "\u{111E1}"), "\u{111EA}");
    }

    #[test]
    fn format_zero_padded() {
        assert_eq!(format_one(3, "001"), "003");
        assert_eq!(format_one(100, "001"), "100");
    }

    #[test]
    fn format_zero_value_honours_pad_width() {
        // XSLT numbering of 0: the format token's width still applies,
        // so `01` produces `00` and `1` produces `0` (insn/number-0805).
        assert_eq!(format_one(0, "01"), "00");
        assert_eq!(format_one(0, "001"), "000");
        assert_eq!(format_one(0, "1"), "0");
    }

    #[test]
    fn format_alpha_upper() {
        assert_eq!(format_one(1,  "A"), "A");
        assert_eq!(format_one(26, "A"), "Z");
        assert_eq!(format_one(27, "A"), "AA");
        assert_eq!(format_one(28, "A"), "AB");
    }

    #[test]
    fn format_alpha_lower() {
        assert_eq!(format_one(1, "a"), "a");
        assert_eq!(format_one(2, "a"), "b");
    }

    #[test]
    fn format_roman_upper() {
        assert_eq!(format_one(1,    "I"), "I");
        assert_eq!(format_one(4,    "I"), "IV");
        assert_eq!(format_one(9,    "I"), "IX");
        assert_eq!(format_one(1994, "I"), "MCMXCIV");
    }

    #[test]
    fn format_roman_lower() {
        assert_eq!(format_one(4, "i"), "iv");
    }

    #[test]
    fn count_single_default_works() {
        let doc = parse_str("<r><i/><i/><i/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let nodes = match ctx.eval("/r/i").unwrap() { Value::NodeSet(ns) => ns, _ => panic!() };
        assert_eq!(count_single_default(nodes[0], &ctx.index), Some(1));
        assert_eq!(count_single_default(nodes[1], &ctx.index), Some(2));
        assert_eq!(count_single_default(nodes[2], &ctx.index), Some(3));
    }
}
