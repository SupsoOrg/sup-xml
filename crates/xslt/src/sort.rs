//! `xsl:sort` engine — sort a node sequence by N keys before
//! `xsl:apply-templates` / `xsl:for-each` iterates it.
//!
//! Each `xsl:sort` element contributes one key.  When multiple
//! sort elements precede an iteration, they apply in document
//! order: first is the primary key, second is the secondary
//! tie-break, etc.
//!
//! Per-key attributes (XSLT 1.0 §10):
//!
//! * `select=` — XPath whose string-value-of-result becomes the key.
//!   Defaults to `.` (the current node's string value).
//! * `data-type=` — `text` (default) compares as strings; `number`
//!   coerces to f64 and compares numerically.  Other values (QName-
//!   shaped extension types) fall back to text.
//! * `order=` — `ascending` (default) or `descending`.
//! * `lang=` / `case-order=` — collation hints.  We honour
//!   `case-order` (upper-first / lower-first) but ignore `lang`
//!   pending a real collation; the spec lets us.

use sup_xml_core::xpath::eval::{
    DateKind, Value, date_value_to_utc_micros, value_to_number, value_to_string,
};
use sup_xml_core::xpath::{DocIndexLike, NodeId};

use crate::ast::Sort;
use crate::error::XsltError;

type Result<T> = std::result::Result<T, XsltError>;

/// One materialised sort key — the value plus the per-key
/// settings that decide how to compare it.
#[derive(Clone, Debug)]
struct Key {
    value:     KeyValue,
    descending: bool,
    upper_first: Option<bool>, // None → don't care; Some(true) → upper-first
    /// When true, text values compare by raw Unicode codepoints
    /// (XPath 2.0 codepoint collation: no case folding); when false,
    /// the default Unicode-aware comparator runs.
    codepoint:  bool,
    /// When true, ASCII letters are folded to lower case before
    /// comparison (XPath F&O html-ascii-case-insensitive collation).
    /// Non-ASCII codepoints pass through unchanged.
    ascii_ci:   bool,
}

#[derive(Clone, Debug)]
enum KeyValue {
    Text(String),
    Number(f64),
    /// Default sort with no `data-type=` — the comparator picks
    /// numeric / typed compare when both operands parsed cleanly,
    /// falling back to text otherwise.  `num` is `Some` iff the
    /// `text` parses cleanly as an `xs:double`; `typed_key` carries
    /// a UTC-normalised microsecond count (xs:date / xs:dateTime /
    /// xs:time) or a duration-in-seconds value, so date sequences
    /// sort by their XPath value rather than lexically (which would
    /// place `10000-…` before `1995-…`).
    Auto { text: String, num: Option<f64>, typed_key: Option<i128> },
}

/// Public entry: given a list of nodes, a list of compiled sort
/// directives, and a closure that can evaluate the sort's `select`
/// expression at a given node, return a new vector containing the
/// same nodes in the spec-defined sort order.
///
/// Stable: equal-keyed nodes preserve their original (document)
/// order — a property the XSLT spec requires.
pub fn sort_nodes<I, F>(
    nodes:    &[NodeId],
    sorts:    &[Sort],
    idx:      &I,
    mut eval: F,
) -> Result<Vec<NodeId>>
where
    I: DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId, usize, usize) -> Result<Value>,
{
    if sorts.is_empty() {
        return Ok(nodes.to_vec());
    }
    // Materialise each (node, [keys]) pair, then sort lexicographically.
    let size = nodes.len();
    let mut indexed: Vec<(usize, NodeId, Vec<Key>)> = Vec::with_capacity(size);
    for (i, &n) in nodes.iter().enumerate() {
        let mut keys = Vec::with_capacity(sorts.len());
        for s in sorts {
            keys.push(make_key(s, n, i + 1, size, idx, &mut eval)?);
        }
        indexed.push((i, n, keys));
    }
    indexed.sort_by(|a, b| compare_key_lists(&a.2, &b.2).then(a.0.cmp(&b.0)));
    Ok(indexed.into_iter().map(|(_, n, _)| n).collect())
}

/// Stable sort permutation for a merged stream whose nodes come from
/// different sources, each contributing its own key directives.
///
/// `xsl:merge` differs from a plain sort: every `xsl:merge-source`
/// declares its own `xsl:merge-key` selects, so node `i` is keyed by
/// `per_node_sorts[i]` rather than by one shared key list.  The spec
/// (XSLT 3.0 §15.2) requires the keys to be consistent across sources
/// in count and comparison semantics (order, data-type, collation), so
/// keys built from different sources remain mutually comparable.
///
/// Returns a permutation of `0..nodes.len()` giving the sorted order,
/// letting the caller reorder its own source-tagged view of the nodes.
pub fn sort_order_keyed<I, F>(
    nodes:          &[NodeId],
    per_node_sorts: &[&[Sort]],
    idx:            &I,
    mut eval:       F,
) -> Result<Vec<usize>>
where
    I: DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId, usize, usize) -> Result<Value>,
{
    let size = nodes.len();
    let mut indexed: Vec<(usize, Vec<Key>)> = Vec::with_capacity(size);
    for (i, &n) in nodes.iter().enumerate() {
        let mut keys = Vec::with_capacity(per_node_sorts[i].len());
        for s in per_node_sorts[i] {
            keys.push(make_key(s, n, i + 1, size, idx, &mut eval)?);
        }
        indexed.push((i, keys));
    }
    indexed.sort_by(|a, b| compare_key_lists(&a.1, &b.1).then(a.0.cmp(&b.0)));
    Ok(indexed.into_iter().map(|(i, _)| i).collect())
}

/// Generalised counterpart of [`sort_nodes`] for arbitrary `Value`
/// items — the form `xsl:perform-sort` produces when its source
/// sequence contains atomic values rather than nodes (XSLT 2.0
/// §13.3).  The caller's `eval` closure receives the *current item*
/// (as a `&Value`) along with the unsorted position / size so it can
/// route bare `.` to the right context-item — typically by setting
/// the atomic-context-item thread-local around the inner XPath
/// `eval_expr` call.
///
/// Returns the items reordered per the sort keys; the input order
/// breaks ties (stable sort, as XSLT requires).
pub fn sort_items<I, F>(
    items:    Vec<Value>,
    sorts:    &[Sort],
    idx:      &I,
    mut eval: F,
) -> Result<Vec<Value>>
where
    I: DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, &Value, usize, usize) -> Result<Value>,
{
    if sorts.is_empty() { return Ok(items); }
    let size = items.len();
    let mut indexed: Vec<(usize, Value, Vec<Key>)> = Vec::with_capacity(size);
    for (i, it) in items.into_iter().enumerate() {
        let mut keys = Vec::with_capacity(sorts.len());
        for s in sorts {
            keys.push(make_key_for_item(s, &it, i + 1, size, idx, &mut eval)?);
        }
        indexed.push((i, it, keys));
    }
    indexed.sort_by(|a, b| compare_key_lists(&a.2, &b.2).then(a.0.cmp(&b.0)));
    Ok(indexed.into_iter().map(|(_, v, _)| v).collect())
}

fn make_key_for_item<I, F>(
    s: &Sort, item: &Value, pos: usize, size: usize, idx: &I, eval: &mut F,
) -> Result<Key>
where
    I: DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, &Value, usize, usize) -> Result<Value>,
{
    let resolve_avt = |a: Option<&crate::ast::Avt>, eval: &mut F| -> Result<Option<String>> {
        let Some(avt) = a else { return Ok(None); };
        if let Some(s) = resolve_literal_avt(Some(avt)) { return Ok(Some(s)); }
        let mut s = String::new();
        for p in &avt.parts {
            match p {
                crate::ast::AvtPart::Literal(lit) => s.push_str(lit),
                crate::ast::AvtPart::Expr(e) => {
                    let v = eval(e, item, pos, size)?;
                    s.push_str(&value_to_string(&v, idx));
                }
            }
        }
        Ok(Some(s))
    };
    let order_str = resolve_avt(s.order.as_ref(), eval)?.unwrap_or_else(|| "ascending".into());
    let case_str  = resolve_avt(s.case_order.as_ref(), eval)?;
    let dtype_str = resolve_avt(s.data_type.as_ref(), eval)?;
    let coll_str  = resolve_avt(s.collation.as_ref(), eval)?;

    let descending  = order_str == "descending";
    let upper_first = match case_str.as_deref() {
        Some("upper-first") => Some(true),
        Some("lower-first") => Some(false),
        _ => None,
    };
    // XSLT 2.0 §13.1 / XTDE1035 — a collation= that resolves (after AVT
    // expansion) to a URI the processor doesn't recognise is a dynamic
    // error.  Empty / codepoint / html-ascii-case-insensitive are the
    // collations we implement.
    if let Some(c) = coll_str.as_deref() {
        if !crate::compiler::is_recognised_collation(c) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:sort collation '{c}' is not recognised by the processor (XTDE1035)"
            )));
        }
    }
    let codepoint = matches!(coll_str.as_deref(),
        Some("http://www.w3.org/2005/xpath-functions/collation/codepoint"));
    let ascii_ci  = matches!(coll_str.as_deref(),
        Some("http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive"));

    // No `select=` → the key value IS the current item.
    let v = match &s.select {
        Some(e) => eval(e, item, pos, size)?,
        None    => item.clone(),
    };

    let value = match dtype_str.as_deref() {
        Some("number")                => KeyValue::Number(value_to_number(&v, idx)),
        Some(_)                       => KeyValue::Text(value_to_string(&v, idx)),
        None if value_is_numeric_atomic(&v) =>
            KeyValue::Number(value_to_number(&v, idx)),
        None                          => auto_key(&v, idx),
    };
    Ok(Key { value, descending, upper_first, codepoint, ascii_ci })
}

fn make_key<I, F>(
    s: &Sort, node: NodeId, pos: usize, size: usize, idx: &I, eval: &mut F,
) -> Result<Key>
where
    I: DocIndexLike,
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId, usize, usize) -> Result<Value>,
{
    // Resolve per-attribute settings.  AVTs in sort attributes
    // *are* re-evaluated per node (spec §10).  Literal-only AVTs
    // (the common case) take a fast path; mixed `{expr}` ones
    // re-evaluate against the current sort-key context node.
    let resolve_avt = |a: Option<&crate::ast::Avt>, eval: &mut F| -> Result<Option<String>> {
        let Some(avt) = a else { return Ok(None); };
        if let Some(s) = resolve_literal_avt(Some(avt)) { return Ok(Some(s)); }
        let mut s = String::new();
        for p in &avt.parts {
            match p {
                crate::ast::AvtPart::Literal(lit) => s.push_str(lit),
                crate::ast::AvtPart::Expr(e) => {
                    let v = eval(e, node, pos, size)?;
                    s.push_str(&value_to_string(&v, idx));
                }
            }
        }
        Ok(Some(s))
    };
    // XSLT 2.0 §13.1 / XTDE0030 — runtime check for a lang= AVT
    // whose effective value isn't a valid xml:lang token.  Literal
    // values are caught at compile time; this fires for forms like
    // `lang="{$var}"` where the string isn't known until runtime.
    if let Some(lang) = resolve_avt(s.lang.as_ref(), eval)? {
        if !crate::compiler::is_valid_xml_lang(&lang) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:sort lang='{lang}' is not a valid xml:lang value (XTDE0030)"
            )));
        }
    }
    let order_str = resolve_avt(s.order.as_ref(), eval)?.unwrap_or_else(|| "ascending".into());
    let case_str  = resolve_avt(s.case_order.as_ref(), eval)?;
    // `data-type` left as `Option<String>` so the binding-time
    // distinction between "explicitly text" and "default" survives
    // for the type-aware XSLT 2.0 default below.
    let dtype_str = resolve_avt(s.data_type.as_ref(), eval)?;

    let descending  = order_str == "descending";
    let upper_first = match case_str.as_deref() {
        Some("upper-first") => Some(true),
        Some("lower-first") => Some(false),
        _ => None,
    };
    let coll_str  = resolve_avt(s.collation.as_ref(), eval)?;
    // XSLT 2.0 §13.1 / XTDE1035 — a collation= that resolves (after AVT
    // expansion) to a URI the processor doesn't recognise is a dynamic
    // error.  Empty / codepoint / html-ascii-case-insensitive are the
    // collations we implement.
    if let Some(c) = coll_str.as_deref() {
        if !crate::compiler::is_recognised_collation(c) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:sort collation '{c}' is not recognised by the processor (XTDE1035)"
            )));
        }
    }
    let codepoint = matches!(coll_str.as_deref(),
        Some("http://www.w3.org/2005/xpath-functions/collation/codepoint"));
    let ascii_ci  = matches!(coll_str.as_deref(),
        Some("http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive"));

    // Evaluate `select=` (defaults to `.`).  XSLT 1.0 §10: the sort
    // key is evaluated with the current node as the XPath context node
    // and its position/size set to the node's position/size in the
    // *unsorted* node list — so `position()` inside `select=` reflects
    // the source order, which `xsl:sort select="position()"` relies on.
    let v = match &s.select {
        Some(e) => eval(e, node, pos, size)?,
        None    => Value::String(idx.string_value(node)),
    };

    // XSLT 2.0 §13.1 / XTTE1020 — outside backwards-compatibility
    // mode, each sort-key value must atomise to a singleton: a
    // multi-item sequence (e.g. `select="(.,.,.)"`) raises a type
    // error rather than silently sorting on the first item.  Both
    // shapes count — `Value::Sequence` for typed/mixed sequences,
    // `Value::NodeSet` for all-node sequences.  In XSLT 1.0 / 2.0
    // backwards-compatibility mode the first item is used instead.
    let multi_item = match &v {
        Value::Sequence(items) => items.len() > 1,
        Value::NodeSet(ns)     => ns.len() > 1,
        _ => false,
    };
    if multi_item && !sup_xml_core::xpath::eval::in_xpath_1_0_compat() {
        return Err(XsltError::Xpath(
            sup_xml_core::xpath::eval::xpath_err(
                "xsl:sort key value is a sequence of more than one item (XTTE1020)"
            ).with_xpath_code("XTTE1020")));
    }

    // Data-type defaults differ between XSLT 1.0 (always
    // `"text"`) and XSLT 2.0 (`"#default"` — type-aware: a
    // numeric atomic sorts numerically, everything else as
    // text).  We mirror the 2.0 default for select expressions
    // that produce a numeric `Value` so e.g. `xsl:sort
    // select="number(.)"` sorts numerically without an explicit
    // `data-type="number"`.  Stylesheets that want the legacy
    // 1.0 lexicographic order can still ask for it explicitly
    // via `data-type="text"`.
    let value = match dtype_str.as_deref() {
        Some("number")               => KeyValue::Number(value_to_number(&v, idx)),
        Some(_) /* "text" or other */ => KeyValue::Text(value_to_string(&v, idx)),
        None if value_is_numeric_atomic(&v) =>
            // No explicit `data-type` and the select expression
            // produced a numeric atomic — XSLT 2.0 §13.1 default
            // sort uses XPath value comparison, which routes to a
            // numeric compare for numeric values.
            KeyValue::Number(value_to_number(&v, idx)),
        None                         => auto_key(&v, idx),
    };
    Ok(Key { value, descending, upper_first, codepoint, ascii_ci })
}

/// True iff `v` is a single numeric atomic value — the heuristic
/// the XSLT 2.0 `data-type` default uses to pick numeric vs
/// lexical sort comparison.  Numeric sequences (`Sequence` of
/// `Number`s) also qualify if every item is numeric, since
/// `xsl:sort` operates on the first item anyway.
fn value_is_numeric_atomic(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::Typed(t)  => t.numeric.is_some(),
        Value::Sequence(items) => items.first().map_or(false, value_is_numeric_atomic),
        _ => false,
    }
}

/// Build the `KeyValue::Auto` default — captures the item's string
/// value plus, when applicable, a typed numeric/date sort key.  The
/// comparator picks the most specific common key the pair shares.
fn auto_key<I: DocIndexLike>(v: &Value, idx: &I) -> KeyValue {
    let text = value_to_string(v, idx);
    let typed_key = typed_sort_key(v);
    let num = if typed_key.is_some() { None } else { parse_xs_double(&text) };
    KeyValue::Auto { text, num, typed_key }
}

/// Extract an integer sort key from typed date/time/duration values
/// so the default sort compares them by XPath value, not lexically.
/// `xs:date` / `xs:dateTime` / `xs:time` collapse to microseconds
/// since epoch (UTC-normalised); duration values collapse to total
/// seconds (dayTime) or months (yearMonth).  Returns `None` for any
/// value that isn't a typed temporal — caller falls back to numeric
/// or lexical compare.
fn typed_sort_key(v: &Value) -> Option<i128> {
    match v {
        Value::Typed(t) => match t.kind {
            "date" | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay" =>
                date_value_to_utc_micros(&t.lexical, DateKind::Date),
            "dateTime" => date_value_to_utc_micros(&t.lexical, DateKind::DateTime),
            "time"     => date_value_to_utc_micros(&t.lexical, DateKind::Time),
            "dayTimeDuration"   => parse_day_time_secs(&t.lexical).map(|s| s as i128),
            "yearMonthDuration" => parse_year_month_months(&t.lexical).map(|m| m as i128),
            _ => None,
        },
        Value::Sequence(items) => items.first().and_then(typed_sort_key),
        _ => None,
    }
}

/// `xs:dayTimeDuration` → signed total seconds.  Lenient parser
/// matching the core engine's behaviour; returns `None` on shapes
/// that aren't `[-]P[nD][T[nH][nM][nS]]`.
fn parse_day_time_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None       => (1i64, s),
    };
    let body = body.strip_prefix('P')?;
    let (day_part, time_part) = match body.find('T') {
        Some(i) => (&body[..i], &body[i + 1..]),
        None    => (body, ""),
    };
    let pull = |part: &str, marker: char| -> i64 {
        let Some(i) = part.find(marker) else { return 0; };
        let start = part[..i].rfind(|c: char| !c.is_ascii_digit() && c != '.')
            .map(|n| n + 1).unwrap_or(0);
        part[start..i].parse().unwrap_or(0)
    };
    let days  = pull(day_part,  'D');
    let hours = pull(time_part, 'H');
    let mins  = pull(time_part, 'M');
    let secs  = pull(time_part, 'S');
    Some(sign * (days * 86_400 + hours * 3600 + mins * 60 + secs))
}

/// `xs:yearMonthDuration` → signed total months.
fn parse_year_month_months(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, body) = match s.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None       => (1i64, s),
    };
    let body = body.strip_prefix('P')?;
    let pull = |marker: char| -> i64 {
        let Some(i) = body.find(marker) else { return 0; };
        let start = body[..i].rfind(|c: char| !c.is_ascii_digit())
            .map(|n| n + 1).unwrap_or(0);
        body[start..i].parse().unwrap_or(0)
    };
    let years  = pull('Y');
    let months = pull('M');
    Some(sign * (years * 12 + months))
}

/// XPath 2.0 `xs:double` lexical form — `INF`, `-INF`, `NaN`, and
/// the usual decimal / exponential forms.  Returns `None` for any
/// string that isn't a valid double lexical, so the sort-default
/// can fall back to lexical comparison.
fn parse_xs_double(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() { return None; }
    match t {
        "INF"   =>  Some(f64::INFINITY),
        "-INF"  =>  Some(f64::NEG_INFINITY),
        "+INF"  =>  Some(f64::INFINITY),
        "NaN"   =>  Some(f64::NAN),
        _ => t.parse::<f64>().ok(),
    }
}

fn resolve_literal_avt(a: Option<&crate::ast::Avt>) -> Option<String> {
    let avt = a?;
    if !avt.is_literal() { return None; }
    let mut s = String::new();
    for p in &avt.parts {
        if let crate::ast::AvtPart::Literal(lit) = p { s.push_str(lit); }
    }
    Some(s)
}

fn compare_key_lists(a: &[Key], b: &[Key]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (ka, kb) in a.iter().zip(b.iter()) {
        let mut ord = compare_one_key(ka, kb);
        if ka.descending {
            ord = ord.reverse();
        }
        if ord != Ordering::Equal { return ord; }
    }
    Ordering::Equal
}

fn compare_one_key(a: &Key, b: &Key) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    /// NaN sorts FIRST under both ascending and descending —
    /// XSLT 1.0 §10 leaves NaN order implementation-defined,
    /// and the W3C XSLT test suite / Saxon convention is to
    /// place non-numeric values before the numeric ones.
    fn cmp_num(x: f64, y: f64) -> Ordering {
        match (x.is_nan(), y.is_nan()) {
            (true,  true)  => Ordering::Equal,
            (true,  false) => Ordering::Less,
            (false, true)  => Ordering::Greater,
            _ => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        }
    }
    match (&a.value, &b.value) {
        (KeyValue::Number(x), KeyValue::Number(y)) => cmp_num(*x, *y),
        (KeyValue::Text(x),   KeyValue::Text(y))   => text_compare_opts(x, y, a.upper_first, a.codepoint, a.ascii_ci),
        // Mixed: stringify the number.
        (KeyValue::Number(n), KeyValue::Text(s)) =>
            text_compare_opts(&format_num(*n), s, a.upper_first, a.codepoint, a.ascii_ci),
        (KeyValue::Text(s), KeyValue::Number(n)) =>
            text_compare_opts(s, &format_num(*n), a.upper_first, a.codepoint, a.ascii_ci),

        // Auto / Auto: pick the most specific common ordering —
        // typed (date/duration) key first, then numeric, then text.
        // Matches XSLT 2.0 §13.1's type-aware default sort.  An
        // explicit codepoint / ascii-CI collation forces text
        // comparison even when both items parse as numbers —
        // `xsl:sort collation="codepoint"` says "compare strings",
        // and `-13` vs `-47` must order by codepoint (`-13` < `-47`).
        (KeyValue::Auto { text: ta, num: na, typed_key: ka },
         KeyValue::Auto { text: tb, num: nb, typed_key: kb }) => {
            if a.codepoint || a.ascii_ci {
                return text_compare_opts(ta, tb, a.upper_first, a.codepoint, a.ascii_ci);
            }
            match (ka, kb) {
                (Some(x), Some(y)) => x.cmp(y),
                _ => match (na, nb) {
                    (Some(x), Some(y)) => cmp_num(*x, *y),
                    _                  => text_compare_opts(ta, tb, a.upper_first, a.codepoint, a.ascii_ci),
                },
            }
        }
        // Auto vs explicit Number / Text: prefer the explicit
        // typing.  Numeric wins if Auto parsed; otherwise lexical.
        (KeyValue::Auto { num: Some(x), .. }, KeyValue::Number(y)) => cmp_num(*x, *y),
        (KeyValue::Number(x), KeyValue::Auto { num: Some(y), .. }) => cmp_num(*x, *y),
        (KeyValue::Auto { text, .. }, KeyValue::Number(n)) =>
            text_compare_opts(text, &format_num(*n), a.upper_first, a.codepoint, a.ascii_ci),
        (KeyValue::Number(n), KeyValue::Auto { text, .. }) =>
            text_compare_opts(&format_num(*n), text, a.upper_first, a.codepoint, a.ascii_ci),
        (KeyValue::Auto { text: ta, .. }, KeyValue::Text(tb)) =>
            text_compare_opts(ta, tb, a.upper_first, a.codepoint, a.ascii_ci),
        (KeyValue::Text(ta), KeyValue::Auto { text: tb, .. }) =>
            text_compare_opts(ta, tb, a.upper_first, a.codepoint, a.ascii_ci),
    }
}

#[cfg(test)]
fn text_compare(
    a: &str, b: &str,
    upper_first: Option<bool>, codepoint: bool,
) -> std::cmp::Ordering {
    text_compare_opts(a, b, upper_first, codepoint, false)
}

fn text_compare_opts(
    a: &str, b: &str,
    upper_first: Option<bool>, codepoint: bool, ascii_ci: bool,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if ascii_ci {
        // XPath F&O html-ascii-case-insensitive — A..Z fold to a..z
        // before comparison; non-ASCII codepoints pass through.
        let fold = |c: char| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c };
        let ai = a.chars().map(fold);
        let bi = b.chars().map(fold);
        return ai.cmp(bi);
    }
    if codepoint {
        // XPath 2.0 codepoint collation — raw Unicode codepoint
        // order, no case folding.  'a' (U+0061) > 'A' (U+0041),
        // 'z' < CJK starts (U+4E00).  upper-first / lower-first
        // are ignored since the codepoint ordering already fixes
        // every tie.
        return a.chars().cmp(b.chars());
    }
    // Two-pass collation: first compare case-folded forms so e.g.
    // "URI" sorts before "use" by the `r` vs `s` distinction.  Only
    // when the case-folded forms are equal does case-order break the
    // tie — that's where `upper-first` / `lower-first` matters.
    let ai = a.chars().flat_map(|c| c.to_lowercase());
    let bi = b.chars().flat_map(|c| c.to_lowercase());
    let primary = ai.cmp(bi);
    if primary != Ordering::Equal {
        return primary;
    }
    // Case-fold equal — tie-break on the original strings' case.
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None)         => return Ordering::Equal,
            (None, Some(_))      => return Ordering::Less,
            (Some(_), None)      => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca == cb { continue; }
                let ca_is_upper = ca.is_uppercase();
                let cb_is_upper = cb.is_uppercase();
                match (upper_first, ca_is_upper, cb_is_upper) {
                    (Some(true),  true,  false) => return Ordering::Less,
                    (Some(true),  false, true)  => return Ordering::Greater,
                    (Some(false), true,  false) => return Ordering::Greater,
                    (Some(false), false, true)  => return Ordering::Less,
                    _ => return ca.cmp(&cb),
                }
            }
        }
    }
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) }
    else { format!("{n}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Avt, AvtPart, Sort};
    use sup_xml_core::{parse_str, ParseOptions, XPathContext};
    use sup_xml_core::xpath::eval::NoBindings;
    use sup_xml_core::xpath::eval::eval_expr;
    use sup_xml_core::xpath::eval::EvalCtx;

    fn lit_avt(s: &str) -> Avt {
        Avt { parts: vec![AvtPart::Literal(s.into())] }
    }

    fn doc_nodes(xml: &str, query: &str) -> (sup_xml_tree::dom::Document, XPathContext<'static>, Vec<NodeId>) {
        // Test helper — leak the doc to get 'static lifetimes for
        // the index, then return it back so the caller can drop in
        // the right order.  Acceptable since these are unit tests.
        let doc = parse_str(xml, &ParseOptions::default()).unwrap();
        // SAFETY-NOTE: we transmute the lifetime here for test scaffolding;
        // the leaked Box prevents UAF.  Actual production users go via
        // apply_stylesheet which owns the doc inside its frame.
        let leaked: &'static sup_xml_tree::dom::Document = Box::leak(Box::new(doc));
        let ctx: XPathContext<'static> = XPathContext::new(leaked);
        let nodes = match ctx.eval(query).unwrap() {
            Value::NodeSet(ns) => ns,
            _ => panic!(),
        };
        // Return a fake doc so the signature compiles; consumer doesn't
        // use it (real doc is leaked).
        let fake = parse_str("<x/>", &ParseOptions::default()).unwrap();
        (fake, ctx, nodes)
    }

    #[test]
    fn sort_text_ascending_default() {
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>banana</i><i>apple</i><i>cherry</i></r>",
            "/r/i",
        );
        let s = Sort {
            select: None, lang: None, data_type: None,
            order: None, case_order: None, collation: None,
        };
        let result = sort_nodes(&nodes, std::slice::from_ref(&s), &ctx.index, |e, n, _, _| {
            let v = eval_expr(e, &EvalCtx { context_node: n, pos: 1, size: 1, bindings: &NoBindings, static_ctx: &sup_xml_core::xpath::eval::DEFAULT_STATIC_CTX }, &ctx.index)
                .map_err(XsltError::from)?;
            Ok(v)
        }).unwrap();
        // Expected order: apple, banana, cherry.
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        assert_eq!(strings, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn sort_descending_reverses() {
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>a</i><i>c</i><i>b</i></r>", "/r/i",
        );
        let s = Sort {
            select: None, lang: None, data_type: None,
            order: Some(lit_avt("descending")), case_order: None, collation: None,
        };
        let result = sort_nodes(&nodes, std::slice::from_ref(&s), &ctx.index, |e, n, _, _| {
            let v = eval_expr(e, &EvalCtx { context_node: n, pos: 1, size: 1, bindings: &NoBindings, static_ctx: &sup_xml_core::xpath::eval::DEFAULT_STATIC_CTX }, &ctx.index)
                .map_err(XsltError::from)?;
            Ok(v)
        }).unwrap();
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        assert_eq!(strings, vec!["c", "b", "a"]);
    }

    #[test]
    fn sort_number_uses_numeric_comparison() {
        // Text sort would put 10 < 2; number sort should put 2 < 10.
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>10</i><i>2</i><i>1</i></r>", "/r/i",
        );
        let s = Sort {
            select: None, lang: None,
            data_type: Some(lit_avt("number")),
            order: None, case_order: None, collation: None,
        };
        let result = sort_nodes(&nodes, std::slice::from_ref(&s), &ctx.index, |e, n, _, _| {
            let v = eval_expr(e, &EvalCtx { context_node: n, pos: 1, size: 1, bindings: &NoBindings, static_ctx: &sup_xml_core::xpath::eval::DEFAULT_STATIC_CTX }, &ctx.index)
                .map_err(XsltError::from)?;
            Ok(v)
        }).unwrap();
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        assert_eq!(strings, vec!["1", "2", "10"]);
    }

    #[test]
    fn no_sort_keys_returns_input_unchanged() {
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>c</i><i>a</i><i>b</i></r>", "/r/i",
        );
        let result = sort_nodes(&nodes, &[], &ctx.index, |_, _, _, _| panic!()).unwrap();
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        assert_eq!(strings, vec!["c", "a", "b"]);
    }

    // ── helpers under test directly ─────────────────────────────────────

    #[test]
    fn format_num_integers_stringify_without_decimals() {
        assert_eq!(format_num(0.0),     "0");
        assert_eq!(format_num(42.0),   "42");
        assert_eq!(format_num(-7.0),   "-7");
    }

    #[test]
    fn format_num_fractions_use_default_format() {
        assert_eq!(format_num(3.5),  "3.5");
        assert_eq!(format_num(-1.25), "-1.25");
    }

    #[test]
    fn format_num_large_values_fall_back_to_float_format() {
        // 1e20 is outside i64 range — should NOT use the integer branch.
        let s = format_num(1e20);
        assert!(!s.contains("9223") && !s.contains("-9223"), "got {s}");
    }

    // ── text_compare edge cases ─────────────────────────────────────────

    #[test]
    fn text_compare_prefix_vs_full() {
        use std::cmp::Ordering;
        // (None, Some(_)) → Less
        assert_eq!(text_compare("ab", "abc", None, false), Ordering::Less);
        // (Some(_), None) → Greater
        assert_eq!(text_compare("abc", "ab", None, false), Ordering::Greater);
        // (None, None) → Equal
        assert_eq!(text_compare("", "", None, false), Ordering::Equal);
        // identical strings exhaust both iterators equally.
        assert_eq!(text_compare("xyz", "xyz", None, false), Ordering::Equal);
    }

    #[test]
    fn text_compare_case_order_upper_first() {
        use std::cmp::Ordering;
        // 'A' vs 'a' — same letter, different case.
        assert_eq!(text_compare("A", "a", Some(true), false),  Ordering::Less);
        assert_eq!(text_compare("a", "A", Some(true), false),  Ordering::Greater);
        assert_eq!(text_compare("A", "a", Some(false), false), Ordering::Greater);
        assert_eq!(text_compare("a", "A", Some(false), false), Ordering::Less);
        // upper_first = None → fall back to raw codepoint comparison
        // ('A'=0x41 < 'a'=0x61).
        assert_eq!(text_compare("A", "a", None, false), Ordering::Less);
    }

    // ── compare_one_key paths ───────────────────────────────────────────

    fn key_num(n: f64) -> Key {
        Key { value: KeyValue::Number(n), descending: false, upper_first: None, codepoint: false, ascii_ci: false }
    }
    fn key_text(s: &str) -> Key {
        Key { value: KeyValue::Text(s.into()), descending: false, upper_first: None, codepoint: false, ascii_ci: false }
    }

    #[test]
    fn number_compare_nan_handling() {
        use std::cmp::Ordering;
        // NaN sorts FIRST (XSLT 1.0 §10 lets us pick; W3C suite and
        // Saxon place non-numeric values before numeric ones).
        assert_eq!(compare_one_key(&key_num(f64::NAN), &key_num(f64::NAN)), Ordering::Equal);
        assert_eq!(compare_one_key(&key_num(f64::NAN), &key_num(1.0)),      Ordering::Less);
        assert_eq!(compare_one_key(&key_num(1.0),      &key_num(f64::NAN)), Ordering::Greater);
        assert_eq!(compare_one_key(&key_num(2.0),      &key_num(3.0)),      Ordering::Less);
    }

    #[test]
    fn mixed_number_text_comparison_stringifies_number() {
        use std::cmp::Ordering;
        // 10 vs "10" — should compare equal after stringification.
        assert_eq!(compare_one_key(&key_num(10.0), &key_text("10")), Ordering::Equal);
        assert_eq!(compare_one_key(&key_text("10"), &key_num(10.0)), Ordering::Equal);
        // 5 vs "abc" — "5" < "abc" lexicographically.
        assert_eq!(compare_one_key(&key_num(5.0), &key_text("abc")), Ordering::Less);
        assert_eq!(compare_one_key(&key_text("abc"), &key_num(5.0)), Ordering::Greater);
    }

    // ── compare_key_lists: all keys equal returns Ordering::Equal ────────

    #[test]
    fn all_keys_equal_returns_equal() {
        use std::cmp::Ordering;
        let a = vec![key_text("x"), key_num(1.0)];
        let b = vec![key_text("x"), key_num(1.0)];
        assert_eq!(compare_key_lists(&a, &b), Ordering::Equal);
    }

    // ── case-order in real sort runs ────────────────────────────────────

    #[test]
    fn sort_case_order_upper_first() {
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>apple</i><i>Apple</i><i>BANANA</i></r>", "/r/i",
        );
        let s = Sort {
            select: None, lang: None, data_type: None,
            order: None, case_order: Some(lit_avt("upper-first")), collation: None,
        };
        let result = sort_nodes(&nodes, std::slice::from_ref(&s), &ctx.index, |e, n, _, _| {
            let v = eval_expr(e, &EvalCtx { context_node: n, pos: 1, size: 1, bindings: &NoBindings, static_ctx: &sup_xml_core::xpath::eval::DEFAULT_STATIC_CTX }, &ctx.index)
                .map_err(XsltError::from)?;
            Ok(v)
        }).unwrap();
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        // Apple before apple (upper-first), BANANA after both apples.
        assert_eq!(strings, vec!["Apple", "apple", "BANANA"]);
    }

    #[test]
    fn sort_case_order_lower_first() {
        let (_doc, ctx, nodes) = doc_nodes(
            "<r><i>apple</i><i>Apple</i></r>", "/r/i",
        );
        let s = Sort {
            select: None, lang: None, data_type: None,
            order: None, case_order: Some(lit_avt("lower-first")), collation: None,
        };
        let result = sort_nodes(&nodes, std::slice::from_ref(&s), &ctx.index, |e, n, _, _| {
            let v = eval_expr(e, &EvalCtx { context_node: n, pos: 1, size: 1, bindings: &NoBindings, static_ctx: &sup_xml_core::xpath::eval::DEFAULT_STATIC_CTX }, &ctx.index)
                .map_err(XsltError::from)?;
            Ok(v)
        }).unwrap();
        let strings: Vec<_> = result.iter().map(|&n| ctx.index.string_value(n)).collect();
        assert_eq!(strings, vec!["apple", "Apple"]);
    }
}
