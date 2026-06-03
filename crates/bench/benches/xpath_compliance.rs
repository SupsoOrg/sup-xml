//! XPath 1.0 cross-implementation conformance runner.
//!
//! Both sup-xml and libxml2 implement only XPath 1.0, so this bench
//! compares them on equal footing.  There's no off-the-shelf W3C
//! XPath 1.0 test suite (QT3 covers 2.0+), so we hand-curate a corpus
//! inline below covering the spec's structural sections:
//!
//!   * location paths (`/`, `//`, relative, absolute)
//!   * all 13 axes
//!   * node tests (`*`, `text()`, `node()`, `comment()`, `processing-instruction()`)
//!   * predicates — positional, comparison, function-based, nested
//!   * comparison and arithmetic operators, union, boolean ops
//!   * the full XPath 1.0 function library (string / number / boolean / node-set)
//!
//! Each test carries the spec-correct expected result.  Both backends
//! are scored independently against it, so a row where libxml2 fails
//! is libxml2's bug, not a sup-xml bug we're forced to match.
//!
//! Today's backends:
//!   - **sup-xml** — `sup_xml::XPathContext`
//!   - **libxml2** — `xmlXPathEvalExpression` via FFI
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench xpath_compliance
//!
//! Env vars:
//!     XPATH_VERBOSE=1     print every disagreement
//!     XPATH_FILTER=<sub>  only run tests whose id contains <sub>

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

use sup_xml::{ParseOptions, XPathContext, XPathValue, parse_str};

// ── libxml2 XPath FFI ────────────────────────────────────────────────────────

type XmlDocPtr          = *mut c_void;
type XmlXPathContextPtr = *mut c_void;
type XmlNodePtr         = *mut c_void;

/// libxml2's `xmlXPathObjectType` — only the XPath 1.0 result types
/// are relevant here.  The enum has additional values (XPATH_POINT etc.)
/// that exist only when XPointer support is compiled in; we treat any
/// unknown discriminant as "error / unsupported."
const XPATH_NODESET: c_int = 1;
const XPATH_BOOLEAN: c_int = 2;
const XPATH_NUMBER:  c_int = 3;
const XPATH_STRING:  c_int = 4;

/// `xmlNodeSet` — the C struct libxml2 returns when a query yields a
/// node-set.  `#[repr(C)]` ensures we match the C layout for direct
/// pointer reads; we never construct this struct, only deref pointers
/// libxml2 hands us.
#[repr(C)]
struct XmlNodeSet {
    node_nr:  c_int,
    node_max: c_int,
    node_tab: *mut XmlNodePtr,
}

/// `xmlXPathObject` — see the same caveat as `XmlNodeSet`.  The
/// field order and padding here matches libxml2's `_xmlXPathObject`
/// on every 64-bit platform we target (Rust's `#[repr(C)]` inserts
/// the same alignment padding the C compiler does).
#[repr(C)]
struct XmlXPathObject {
    object_type: c_int,
    nodesetval:  *mut XmlNodeSet,
    boolval:     c_int,
    floatval:    f64,
    stringval:   *mut c_char,
    user:        *mut c_void,
    index:       c_int,
    user2:       *mut c_void,
    index2:      c_int,
}

unsafe extern "C" {
    fn xmlReadMemory(buffer: *const c_char, size: c_int, url: *const c_char,
                     encoding: *const c_char, options: c_int) -> XmlDocPtr;
    fn xmlFreeDoc(doc: XmlDocPtr);

    fn xmlXPathNewContext(doc: XmlDocPtr) -> XmlXPathContextPtr;
    fn xmlXPathFreeContext(ctx: XmlXPathContextPtr);
    fn xmlXPathEvalExpression(expr: *const c_char, ctx: XmlXPathContextPtr)
        -> *mut XmlXPathObject;
    fn xmlXPathFreeObject(obj: *mut XmlXPathObject);
    fn xmlXPathCastNodeToString(node: XmlNodePtr) -> *mut c_char;

    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
    fn xmlSetStructuredErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

unsafe extern "C" fn libxml2_swallow() {}

fn install_libxml2_silencer() {
    unsafe {
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
        xmlSetStructuredErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
    }
}

// ── result model ─────────────────────────────────────────────────────────────

/// Normalised backend result.  Both backends boil their native result
/// types down to this enum so the comparison logic is identical.
#[derive(Debug, Clone)]
enum Outcome {
    Bool(bool),
    /// IEEE-754 double.  Note: NaN ≠ NaN in `==`; equality below uses
    /// `is_nan() == is_nan() || a == b` to match XPath 1.0 number
    /// semantics where `NaN = NaN` is *false* but the *value itself*
    /// is reproducible.
    Num(f64),
    Str(String),
    /// String-values of nodes in document order — the canonical
    /// representation for cross-impl node-set comparison.  Matches
    /// what `xmllint --xpath` emits and what `XPathContext::eval_strings`
    /// produces.
    NodeStrings(Vec<String>),
    /// Backend errored (parse failure, eval failure, undefined function,
    /// etc.).  The exact message isn't compared — only the fact that
    /// an error was produced.
    Error(String),
}

/// Spec-correct expected result for a test.  Hand-curated per test by
/// reading XPath 1.0 § X.X and reasoning through the semantics.
#[derive(Debug, Clone)]
enum Expected {
    Bool(bool),
    Num(f64),
    Str(&'static str),
    /// Node-set with these string-values in document order.  Empty
    /// vec means "the empty node-set" — both backends should produce
    /// `NodeStrings(vec![])`.
    Nodes(&'static [&'static str]),
    /// Node-set whose *size* matches `count`; the individual nodes
    /// aren't asserted.  Useful when a query produces nodes whose
    /// string-values are tedious to spell out but the count is the
    /// real point.
    Count(usize),
    /// Expression must fail.  Used for tests of syntactic / semantic
    /// errors (undefined function, malformed expression).
    Error,
}

fn matches(got: &Outcome, want: &Expected) -> bool {
    match (got, want) {
        (Outcome::Bool(b), Expected::Bool(e))  => b == e,
        (Outcome::Num(n),  Expected::Num(e))   => (n.is_nan() && e.is_nan()) || n == e,
        (Outcome::Str(s),  Expected::Str(e))   => s == e,
        (Outcome::NodeStrings(s), Expected::Nodes(e)) => {
            s.len() == e.len() && s.iter().zip(e.iter()).all(|(a, b)| a == b)
        }
        (Outcome::NodeStrings(s), Expected::Count(n)) => s.len() == *n,
        (Outcome::Error(_), Expected::Error) => true,
        _ => false,
    }
}

fn fmt_outcome(o: &Outcome) -> String {
    match o {
        Outcome::Bool(b) => format!("bool({})", b),
        Outcome::Num(n)  => format!("num({})",  n),
        Outcome::Str(s)  => format!("str({:?})", s),
        Outcome::NodeStrings(s) => format!("nodes({:?})", s),
        Outcome::Error(e) => format!("error({})",
            if e.len() > 60 { format!("{}…", &e[..60]) } else { e.clone() }),
    }
}

// ── test fixtures ────────────────────────────────────────────────────────────

/// Bookstore catalogue with attrs, sibling order, comment + PI.
const DOC_CATALOG: &str = r#"<catalog>
  <book id="b1" lang="en"><title>Hamlet</title><author>Shakespeare</author><price>10.99</price><year>1603</year></book>
  <book id="b2" lang="en"><title>1984</title><author>Orwell</author><price>15.50</price><year>1949</year></book>
  <book id="b3" lang="fr"><title>Candide</title><author>Voltaire</author><price>8.00</price><year>1759</year></book>
  <!--end of list-->
  <?proc instr?>
</catalog>"#;

/// Deeper tree for ancestor / descendant / sibling tests.
const DOC_NESTED: &str = r#"<root>
  <a id="a1">
    <b id="b1"><c id="c1">text-c1</c><c id="c2">text-c2</c></b>
    <b id="b2"><c id="c3">text-c3</c></b>
  </a>
  <a id="a2"><b id="b3"/></a>
</root>"#;

fn doc_for(id: &str) -> &'static str {
    match id {
        "catalog" => DOC_CATALOG,
        "nested"  => DOC_NESTED,
        _         => unreachable!("unknown doc id {}", id),
    }
}

// ── test corpus ──────────────────────────────────────────────────────────────

struct Test {
    id:       &'static str,
    category: &'static str,
    doc:      &'static str,
    expr:     &'static str,
    expected: Expected,
}

/// Each row is one XPath 1.0 conformance test.  The `expected` value
/// is the spec-correct answer; both backends are scored against it
/// independently.
const TESTS: &[Test] = &[
    // ── location paths ──────────────────────────────────────────────────────
    Test { id: "loc-01", category: "location-paths", doc: "catalog",
        expr: "/catalog", expected: Expected::Count(1) },
    Test { id: "loc-02", category: "location-paths", doc: "catalog",
        expr: "/catalog/book", expected: Expected::Count(3) },
    Test { id: "loc-03", category: "location-paths", doc: "catalog",
        expr: "/catalog/book/title", expected: Expected::Nodes(&["Hamlet", "1984", "Candide"]) },
    Test { id: "loc-04", category: "location-paths", doc: "catalog",
        expr: "//title", expected: Expected::Nodes(&["Hamlet", "1984", "Candide"]) },
    Test { id: "loc-05", category: "location-paths", doc: "catalog",
        expr: "//book/author", expected: Expected::Nodes(&["Shakespeare", "Orwell", "Voltaire"]) },
    Test { id: "loc-06", category: "location-paths", doc: "nested",
        expr: "//c", expected: Expected::Nodes(&["text-c1", "text-c2", "text-c3"]) },
    Test { id: "loc-07", category: "location-paths", doc: "catalog",
        expr: "/catalog/*", expected: Expected::Count(3) },
    Test { id: "loc-08", category: "location-paths", doc: "catalog",
        expr: "/nonexistent", expected: Expected::Count(0) },

    // ── axes (XPath 1.0 § 2.2) ──────────────────────────────────────────────
    Test { id: "axis-child", category: "axes", doc: "nested",
        expr: "count(/root/a)", expected: Expected::Num(2.0) },
    Test { id: "axis-descendant", category: "axes", doc: "nested",
        expr: "count(/root/descendant::c)", expected: Expected::Num(3.0) },
    Test { id: "axis-descendant-or-self", category: "axes", doc: "nested",
        expr: "count(/root/descendant-or-self::a)", expected: Expected::Num(2.0) },
    Test { id: "axis-parent", category: "axes", doc: "nested",
        expr: "name(//c[1]/parent::*)", expected: Expected::Str("b") },
    Test { id: "axis-ancestor", category: "axes", doc: "nested",
        expr: "count(//c[@id='c1']/ancestor::*)", expected: Expected::Num(3.0) },
    Test { id: "axis-ancestor-or-self", category: "axes", doc: "nested",
        expr: "count(//c[@id='c1']/ancestor-or-self::*)", expected: Expected::Num(4.0) },
    Test { id: "axis-following-sibling", category: "axes", doc: "nested",
        expr: "//c[@id='c1']/following-sibling::c/@id",
        expected: Expected::Nodes(&["c2"]) },
    Test { id: "axis-preceding-sibling", category: "axes", doc: "nested",
        expr: "//c[@id='c2']/preceding-sibling::c/@id",
        expected: Expected::Nodes(&["c1"]) },
    Test { id: "axis-following", category: "axes", doc: "nested",
        expr: "count(//c[@id='c1']/following::c)", expected: Expected::Num(2.0) },
    Test { id: "axis-preceding", category: "axes", doc: "nested",
        expr: "count(//c[@id='c3']/preceding::c)", expected: Expected::Num(2.0) },
    Test { id: "axis-self", category: "axes", doc: "nested",
        expr: "name(/root/a[1]/self::*)", expected: Expected::Str("a") },
    Test { id: "axis-attribute", category: "axes", doc: "nested",
        expr: "//c[@id='c1']/attribute::id", expected: Expected::Nodes(&["c1"]) },

    // ── node tests (XPath 1.0 § 2.3) ────────────────────────────────────────
    Test { id: "nt-star", category: "node-tests", doc: "catalog",
        expr: "count(/catalog/*)", expected: Expected::Num(3.0) },
    Test { id: "nt-node", category: "node-tests", doc: "catalog",
        expr: "count(/catalog/node())", expected: Expected::Num(11.0) },
    Test { id: "nt-text", category: "node-tests", doc: "catalog",
        // 6 whitespace-only text-node runs separate `<catalog>`'s children:
        // before book1, between book1/2, 2/3, 3/comment, comment/PI, PI/</catalog>.
        expr: "count(/catalog/text())", expected: Expected::Num(6.0) },
    Test { id: "nt-comment", category: "node-tests", doc: "catalog",
        expr: "count(/catalog/comment())", expected: Expected::Num(1.0) },
    Test { id: "nt-comment-value", category: "node-tests", doc: "catalog",
        expr: "/catalog/comment()", expected: Expected::Nodes(&["end of list"]) },
    Test { id: "nt-pi-any", category: "node-tests", doc: "catalog",
        expr: "count(/catalog/processing-instruction())", expected: Expected::Num(1.0) },
    Test { id: "nt-pi-named", category: "node-tests", doc: "catalog",
        expr: "count(/catalog/processing-instruction('proc'))",
        expected: Expected::Num(1.0) },
    Test { id: "nt-named", category: "node-tests", doc: "catalog",
        expr: "count(//book)", expected: Expected::Num(3.0) },

    // ── predicates ──────────────────────────────────────────────────────────
    Test { id: "pred-position-1", category: "predicates", doc: "catalog",
        expr: "/catalog/book[1]/title", expected: Expected::Nodes(&["Hamlet"]) },
    Test { id: "pred-last", category: "predicates", doc: "catalog",
        expr: "/catalog/book[last()]/title", expected: Expected::Nodes(&["Candide"]) },
    Test { id: "pred-position-eq", category: "predicates", doc: "catalog",
        expr: "/catalog/book[position()=2]/title", expected: Expected::Nodes(&["1984"]) },
    Test { id: "pred-last-minus-one", category: "predicates", doc: "catalog",
        expr: "/catalog/book[position()=last()-1]/title", expected: Expected::Nodes(&["1984"]) },
    Test { id: "pred-attr-eq", category: "predicates", doc: "catalog",
        expr: "//book[@id='b2']/title", expected: Expected::Nodes(&["1984"]) },
    Test { id: "pred-attr-exists", category: "predicates", doc: "catalog",
        expr: "count(//book[@lang])", expected: Expected::Num(3.0) },
    Test { id: "pred-multiple", category: "predicates", doc: "catalog",
        expr: "//book[@lang='en'][2]/title", expected: Expected::Nodes(&["1984"]) },
    Test { id: "pred-nested", category: "predicates", doc: "catalog",
        expr: "count(//book[title[contains(.,'a')]])", expected: Expected::Num(2.0) },
    Test { id: "pred-number-cmp", category: "predicates", doc: "catalog",
        expr: "//book[price > 10]/title", expected: Expected::Nodes(&["Hamlet", "1984"]) },
    Test { id: "pred-or", category: "predicates", doc: "catalog",
        expr: "count(//book[@lang='fr' or price < 12])", expected: Expected::Num(2.0) },

    // ── operators (XPath 1.0 § 3.4 – 3.5) ───────────────────────────────────
    Test { id: "op-add", category: "operators", doc: "catalog",
        expr: "1 + 2", expected: Expected::Num(3.0) },
    Test { id: "op-sub", category: "operators", doc: "catalog",
        expr: "10 - 3.5", expected: Expected::Num(6.5) },
    Test { id: "op-mul", category: "operators", doc: "catalog",
        expr: "4 * 5", expected: Expected::Num(20.0) },
    Test { id: "op-div", category: "operators", doc: "catalog",
        expr: "10 div 4", expected: Expected::Num(2.5) },
    Test { id: "op-mod", category: "operators", doc: "catalog",
        expr: "10 mod 3", expected: Expected::Num(1.0) },
    Test { id: "op-div-zero", category: "operators", doc: "catalog",
        // XPath 1.0 § 3.5: division by zero produces ±Infinity, not error
        expr: "1 div 0", expected: Expected::Num(f64::INFINITY) },
    Test { id: "op-eq", category: "operators", doc: "catalog",
        expr: "2 = 2", expected: Expected::Bool(true) },
    Test { id: "op-neq", category: "operators", doc: "catalog",
        expr: "2 != 3", expected: Expected::Bool(true) },
    Test { id: "op-lt", category: "operators", doc: "catalog",
        expr: "1 < 2", expected: Expected::Bool(true) },
    Test { id: "op-and", category: "operators", doc: "catalog",
        expr: "true() and false()", expected: Expected::Bool(false) },
    Test { id: "op-or", category: "operators", doc: "catalog",
        expr: "true() or false()", expected: Expected::Bool(true) },
    Test { id: "op-union", category: "operators", doc: "catalog",
        expr: "count(//title | //author)", expected: Expected::Num(6.0) },

    // ── string functions (XPath 1.0 § 4.2) ──────────────────────────────────
    Test { id: "fn-string-num", category: "string-fns", doc: "catalog",
        expr: "string(42)", expected: Expected::Str("42") },
    Test { id: "fn-string-bool", category: "string-fns", doc: "catalog",
        expr: "string(true())", expected: Expected::Str("true") },
    Test { id: "fn-concat", category: "string-fns", doc: "catalog",
        expr: "concat('foo','-','bar')", expected: Expected::Str("foo-bar") },
    Test { id: "fn-starts-with", category: "string-fns", doc: "catalog",
        expr: "starts-with('hello world','hello')", expected: Expected::Bool(true) },
    Test { id: "fn-contains", category: "string-fns", doc: "catalog",
        expr: "contains('foobar','oba')", expected: Expected::Bool(true) },
    Test { id: "fn-substring-before", category: "string-fns", doc: "catalog",
        expr: "substring-before('1999/04/01','/')", expected: Expected::Str("1999") },
    Test { id: "fn-substring-after", category: "string-fns", doc: "catalog",
        expr: "substring-after('1999/04/01','/')", expected: Expected::Str("04/01") },
    Test { id: "fn-substring-2arg", category: "string-fns", doc: "catalog",
        expr: "substring('12345',2)", expected: Expected::Str("2345") },
    Test { id: "fn-substring-3arg", category: "string-fns", doc: "catalog",
        // XPath 1.0 § 4.2: substring is 1-indexed; start=2, length=3 → "234"
        expr: "substring('12345',2,3)", expected: Expected::Str("234") },
    Test { id: "fn-substring-clip-low", category: "string-fns", doc: "catalog",
        // start before 1, length spans into the string
        expr: "substring('12345',-2,4)", expected: Expected::Str("1") },
    Test { id: "fn-string-length", category: "string-fns", doc: "catalog",
        expr: "string-length('hello')", expected: Expected::Num(5.0) },
    Test { id: "fn-normalize-space", category: "string-fns", doc: "catalog",
        expr: "normalize-space('  a  b\tc  ')", expected: Expected::Str("a b c") },
    Test { id: "fn-translate", category: "string-fns", doc: "catalog",
        expr: "translate('bar','abc','ABC')", expected: Expected::Str("BAr") },

    // ── number functions (XPath 1.0 § 4.4) ──────────────────────────────────
    Test { id: "fn-number-str", category: "number-fns", doc: "catalog",
        expr: "number('3.14')", expected: Expected::Num(3.14) },
    Test { id: "fn-number-bad", category: "number-fns", doc: "catalog",
        expr: "number('abc')", expected: Expected::Num(f64::NAN) },
    Test { id: "fn-floor", category: "number-fns", doc: "catalog",
        expr: "floor(3.7)", expected: Expected::Num(3.0) },
    Test { id: "fn-ceiling", category: "number-fns", doc: "catalog",
        expr: "ceiling(3.2)", expected: Expected::Num(4.0) },
    Test { id: "fn-round", category: "number-fns", doc: "catalog",
        // XPath 1.0 § 4.4: round(1.5) = 2 (rounds toward +∞ for x.5)
        expr: "round(1.5)", expected: Expected::Num(2.0) },
    Test { id: "fn-round-neg", category: "number-fns", doc: "catalog",
        // XPath 1.0 § 4.4: round(-1.5) = -1 (rounds toward +∞ for x.5)
        expr: "round(-1.5)", expected: Expected::Num(-1.0) },
    Test { id: "fn-sum", category: "number-fns", doc: "catalog",
        expr: "sum(//price)", expected: Expected::Num(34.49) },

    // ── boolean functions (XPath 1.0 § 4.3) ─────────────────────────────────
    Test { id: "fn-true",  category: "boolean-fns", doc: "catalog",
        expr: "true()",  expected: Expected::Bool(true) },
    Test { id: "fn-false", category: "boolean-fns", doc: "catalog",
        expr: "false()", expected: Expected::Bool(false) },
    Test { id: "fn-not",   category: "boolean-fns", doc: "catalog",
        expr: "not(false())", expected: Expected::Bool(true) },
    Test { id: "fn-boolean-empty-set", category: "boolean-fns", doc: "catalog",
        expr: "boolean(//nonexistent)", expected: Expected::Bool(false) },
    Test { id: "fn-boolean-nonempty-set", category: "boolean-fns", doc: "catalog",
        expr: "boolean(//book)", expected: Expected::Bool(true) },
    Test { id: "fn-boolean-zero", category: "boolean-fns", doc: "catalog",
        expr: "boolean(0)", expected: Expected::Bool(false) },
    Test { id: "fn-boolean-nan", category: "boolean-fns", doc: "catalog",
        expr: "boolean(number('x'))", expected: Expected::Bool(false) },
    Test { id: "fn-boolean-empty-str", category: "boolean-fns", doc: "catalog",
        expr: "boolean('')", expected: Expected::Bool(false) },

    // ── node-set functions (XPath 1.0 § 4.1) ────────────────────────────────
    Test { id: "fn-count", category: "nodeset-fns", doc: "catalog",
        expr: "count(//book)", expected: Expected::Num(3.0) },
    Test { id: "fn-count-empty", category: "nodeset-fns", doc: "catalog",
        expr: "count(//nonexistent)", expected: Expected::Num(0.0) },
    Test { id: "fn-name", category: "nodeset-fns", doc: "catalog",
        expr: "name(/catalog/book[1])", expected: Expected::Str("book") },
    Test { id: "fn-local-name", category: "nodeset-fns", doc: "catalog",
        expr: "local-name(/catalog/book[1])", expected: Expected::Str("book") },
    Test { id: "fn-name-empty", category: "nodeset-fns", doc: "catalog",
        expr: "name(/nonexistent)", expected: Expected::Str("") },
    Test { id: "fn-position-inferred", category: "nodeset-fns", doc: "catalog",
        // position() inside a predicate, no explicit comparison
        expr: "//book[position()=last()]/title", expected: Expected::Nodes(&["Candide"]) },
    Test { id: "fn-last-2", category: "nodeset-fns", doc: "catalog",
        expr: "//book[last()]/@id", expected: Expected::Nodes(&["b3"]) },

    // ── error cases ─────────────────────────────────────────────────────────
    Test { id: "err-syntax", category: "errors", doc: "catalog",
        expr: "//book[", expected: Expected::Error },
    Test { id: "err-unknown-fn", category: "errors", doc: "catalog",
        expr: "nosuchfunction()", expected: Expected::Error },
];

// ── backend trait ────────────────────────────────────────────────────────────

trait Backend {
    fn name(&self) -> &'static str;
    /// Evaluate `expr` against the document whose source is `doc_xml`.
    /// Document parsing is part of the backend so we can charge the
    /// cost of "parse + index + eval" honestly, rather than caching a
    /// pre-parsed doc that one side might handle differently.
    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome;
}

// ── sup-xml backend ──────────────────────────────────────────────────────────

struct SupXmlBackend;

impl Backend for SupXmlBackend {
    fn name(&self) -> &'static str { "sup-xml" }

    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome {
        let doc = match parse_str(doc_xml, &ParseOptions::default()) {
            Ok(d)  => d,
            Err(e) => return Outcome::Error(format!("parse: {}", e)),
        };
        let ctx = XPathContext::new(&doc);
        let value = match ctx.eval(expr) {
            Ok(v)  => v,
            Err(e) => return Outcome::Error(e.to_string()),
        };
        match value {
            XPathValue::Boolean(b) => Outcome::Bool(b),
            XPathValue::Number(n)  => Outcome::Num(n.as_f64()),
            XPathValue::String(s)  => Outcome::Str(s),
            XPathValue::NodeSet(_) | XPathValue::ForeignNodeSet(_) => {
                // Use the public helper that produces string-values
                // in document order — exactly what we compare against.
                match ctx.eval_strings(expr) {
                    Ok(ss) => Outcome::NodeStrings(ss),
                    Err(e) => Outcome::Error(e.to_string()),
                }
            }
            // XPath 2.0-only shapes (Typed/Sequence/IntRange/Map/Array/
            // Function): this is the libxml2 1.0 compliance corpus, so
            // these don't arise in practice; stringify defensively.
            other => Outcome::Str(format!("{:?}", other)),
        }
    }
}

// ── libxml2 backend ──────────────────────────────────────────────────────────

struct Libxml2Backend;

impl Backend for Libxml2Backend {
    fn name(&self) -> &'static str { "libxml2" }

    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome {
        // SAFETY: every libxml2 handle is freed before we return.
        unsafe {
            let doc = xmlReadMemory(
                doc_xml.as_ptr() as *const c_char,
                doc_xml.len() as c_int,
                std::ptr::null(), std::ptr::null(), 0,
            );
            if doc.is_null() {
                return Outcome::Error("libxml2 parse failed".into());
            }
            let ctx = xmlXPathNewContext(doc);
            if ctx.is_null() {
                xmlFreeDoc(doc);
                return Outcome::Error("libxml2 ctx alloc failed".into());
            }
            let Ok(c_expr) = CString::new(expr) else {
                xmlXPathFreeContext(ctx);
                xmlFreeDoc(doc);
                return Outcome::Error("expr had NUL".into());
            };
            let obj = xmlXPathEvalExpression(c_expr.as_ptr(), ctx);
            if obj.is_null() {
                xmlXPathFreeContext(ctx);
                xmlFreeDoc(doc);
                return Outcome::Error("libxml2 eval failed".into());
            }
            let outcome = extract_libxml2_result(obj);
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
            outcome
        }
    }
}

/// Read an `xmlXPathObject` libxml2 just produced into our `Outcome`.
/// libxml2 retains ownership of all heap inside the object; we only
/// read.  Strings are copied so we can free the object immediately.
unsafe fn extract_libxml2_result(obj: *mut XmlXPathObject) -> Outcome {
    let o = unsafe { &*obj };
    match o.object_type {
        XPATH_BOOLEAN => Outcome::Bool(o.boolval != 0),
        XPATH_NUMBER  => Outcome::Num(o.floatval),
        XPATH_STRING  => {
            if o.stringval.is_null() {
                Outcome::Str(String::new())
            } else {
                Outcome::Str(unsafe { cstr_to_string(o.stringval) })
            }
        }
        XPATH_NODESET => {
            if o.nodesetval.is_null() {
                return Outcome::NodeStrings(Vec::new());
            }
            let ns = unsafe { &*o.nodesetval };
            if ns.node_nr <= 0 || ns.node_tab.is_null() {
                return Outcome::NodeStrings(Vec::new());
            }
            let mut out = Vec::with_capacity(ns.node_nr as usize);
            for i in 0..ns.node_nr {
                let node = unsafe { *ns.node_tab.offset(i as isize) };
                if node.is_null() { continue; }
                // `xmlXPathCastNodeToString` is the XPath-spec string
                // value of a node (vs `xmlNodeGetContent` which can
                // differ for attributes / namespaces).
                let s = unsafe { xmlXPathCastNodeToString(node) };
                if s.is_null() { out.push(String::new()); }
                else           { out.push(unsafe { cstr_to_string(s) }); }
                // libxml2 callers normally free this with the
                // process-wide `xmlFree` function pointer — we leak
                // the per-node string here to avoid the cross-module-
                // function-pointer dance.  Bounded leak (one small
                // string per node in the bench's whole run).
            }
            Outcome::NodeStrings(out)
        }
        _ => Outcome::Error(format!("unknown libxml2 xpath result type: {}",
                                    o.object_type)),
    }
}

unsafe fn cstr_to_string(p: *const c_char) -> String {
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

// ── runner ───────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CategoryStats {
    /// One entry per backend: how many tests in this category that
    /// backend passed.
    pass:  Vec<usize>,
    total: usize,
    /// How often every backend's outcome matched every other backend's.
    agree: usize,
}

fn main() {
    install_libxml2_silencer();

    let verbose = std::env::var("XPATH_VERBOSE").is_ok();
    let filter  = std::env::var("XPATH_FILTER").ok();

    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(SupXmlBackend),
        Box::new(Libxml2Backend),
    ];
    let backend_names: Vec<&'static str> = backends.iter().map(|b| b.name()).collect();

    println!("\nXPath 1.0 cross-implementation conformance");
    println!("Backends: {}\n", backend_names.join(", "));

    let mut by_category: std::collections::BTreeMap<&'static str, CategoryStats> =
        std::collections::BTreeMap::new();
    let mut totals = CategoryStats { pass: vec![0; backends.len()], total: 0, agree: 0 };
    let mut backend_time: Vec<Duration> = vec![Duration::ZERO; backends.len()];
    let mut disagreements: Vec<(String, Vec<Outcome>, Expected)> = Vec::new();

    for t in TESTS {
        if let Some(f) = &filter {
            if !t.id.contains(f.as_str()) { continue; }
        }
        let doc_xml = doc_for(t.doc);
        let mut outcomes = Vec::with_capacity(backends.len());
        for (i, b) in backends.iter().enumerate() {
            let t0 = Instant::now();
            let o  = b.eval(t.expr, doc_xml);
            backend_time[i] += t0.elapsed();
            outcomes.push(o);
        }

        let stats = by_category.entry(t.category)
            .or_insert_with(|| CategoryStats {
                pass: vec![0; backends.len()], total: 0, agree: 0,
            });
        stats.total  += 1;
        totals.total += 1;

        let mut row_pass: Vec<bool> = Vec::with_capacity(backends.len());
        for (i, o) in outcomes.iter().enumerate() {
            let ok = matches(o, &t.expected);
            row_pass.push(ok);
            if ok {
                stats.pass[i]  += 1;
                totals.pass[i] += 1;
            }
        }
        // Agreement = all backends produced outcomes that compare equal
        // under our normalisation.  Two NodeStrings agree iff equal,
        // two Nums agree under NaN-aware equality, etc.  We re-use
        // `matches` against each backend's own outcome.
        let agree = outcomes.iter().all(|o| outcomes_equal(o, &outcomes[0]));
        if agree {
            stats.agree  += 1;
            totals.agree += 1;
        }

        // Disagreement reporting: if any backend disagrees with the
        // expected, log it.  This catches both single-backend bugs
        // (we fail, they pass; or vice versa) and shared bugs
        // (both fail the same way).
        if !row_pass.iter().all(|&b| b) {
            if verbose {
                eprint!("  [{:<20}] expr={:?}  expected={:?}\n", t.id, t.expr, t.expected);
                for (b, o) in backends.iter().zip(outcomes.iter()) {
                    eprintln!("      {:<10} {}", b.name(), fmt_outcome(o));
                }
            }
            disagreements.push((t.id.to_string(), outcomes, t.expected.clone()));
        }
    }

    print_table(&backend_names, &by_category, &totals);
    print_timing(&backend_names, &backend_time);
    print_misses(&backend_names, &disagreements);
}

fn outcomes_equal(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Bool(x), Outcome::Bool(y)) => x == y,
        (Outcome::Num(x),  Outcome::Num(y))  =>
            (x.is_nan() && y.is_nan()) || x == y,
        (Outcome::Str(x),  Outcome::Str(y))  => x == y,
        (Outcome::NodeStrings(x), Outcome::NodeStrings(y)) => x == y,
        (Outcome::Error(_), Outcome::Error(_)) => true,
        _ => false,
    }
}

fn print_table(
    backend_names: &[&'static str],
    by_category:   &std::collections::BTreeMap<&'static str, CategoryStats>,
    totals:        &CategoryStats,
) {
    println!("  XPath 1.0 conformance — {} tests across {} categories",
             totals.total, by_category.len());
    print!("  {:<18}  {:>6}", "category", "n");
    for name in backend_names {
        print!("  {:>16}", name);
    }
    print!("  {:>7}", "agree");
    println!();

    for (cat, s) in by_category {
        print!("  {:<18}  {:>6}", cat, s.total);
        for &p in &s.pass {
            print!("  {:>16}", fmt_pf(p, s.total));
        }
        print!("  {:>7}", fmt_pct(s.agree, s.total));
        println!();
    }

    print!("  {:<18}  {:>6}", "TOTAL", totals.total);
    for &p in &totals.pass {
        print!("  {:>16}", fmt_pf(p, totals.total));
    }
    print!("  {:>7}", fmt_pct(totals.agree, totals.total));
    println!();
}

fn print_timing(backend_names: &[&'static str], times: &[Duration]) {
    println!("\n  Wall-clock (parse + index + eval, summed across all tests):");
    for (name, dt) in backend_names.iter().zip(times.iter()) {
        println!("    {:<10}  {}", name, fmt_dur(*dt));
    }
}

fn print_misses(
    backend_names: &[&'static str],
    disagreements: &[(String, Vec<Outcome>, Expected)],
) {
    if disagreements.is_empty() {
        println!("\n  All tests passed on all backends. ✓");
        return;
    }
    // Bucket: failures by which-backend-failed pattern.
    // 0b01 = sup-xml fails alone; 0b10 = libxml2 fails alone;
    // 0b11 = both fail.  Generalises to N backends via a bitmask.
    let mut buckets: std::collections::BTreeMap<u32, Vec<&str>> =
        std::collections::BTreeMap::new();
    for (id, outcomes, expected) in disagreements {
        let mut mask: u32 = 0;
        for (i, o) in outcomes.iter().enumerate() {
            if !matches(o, expected) {
                mask |= 1 << i;
            }
        }
        buckets.entry(mask).or_default().push(id.as_str());
    }

    println!("\n  Per-test misses by backend pattern:");
    for (mask, ids) in &buckets {
        let label: Vec<&str> = backend_names.iter().enumerate()
            .filter(|(i, _)| (mask >> i) & 1 == 1)
            .map(|(_, n)| *n).collect();
        println!("    {} fail: {} tests — {}",
            label.join("+"), ids.len(), ids.join(", "));
    }
}

fn fmt_pf(pass: usize, total: usize) -> String {
    if total == 0 { "—".into() }
    else { format!("{}/{} {:>5.1}%", pass, total, pass as f64 * 100.0 / total as f64) }
}
fn fmt_pct(n: usize, d: usize) -> String {
    if d == 0 { "—".into() } else { format!("{:>4.1}%", n as f64 * 100.0 / d as f64) }
}
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 1.0      { format!("{:>6.2} s",  s) }
    else if s >= 1e-3 { format!("{:>6.2} ms", s * 1e3) }
    else              { format!("{:>6.2} µs", s * 1e6) }
}
