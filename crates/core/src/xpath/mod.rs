//! XPath 1.0 expression parsing and evaluation.
//!
//! For repeated queries against the same document, create an
//! [`XPathContext`] once — it builds the document index on construction
//! and amortises that cost across every subsequent `eval_*` call.  The free
//! functions (`xpath_eval`, `xpath_str`, etc.) are one-shot
//! convenience wrappers that build a fresh context each time; use them when
//! you only need a single result.
//!
//! # Example — one-shot helpers
//! ```
//! use sup_xml_core::{parse_str, xpath_str, xpath_count, xpath_bool, ParseOptions};
//!
//! let doc = parse_str(r#"<catalog><book id="1"/><book id="2"/></catalog>"#, &ParseOptions::default()).unwrap();
//!
//! assert_eq!(xpath_count(&doc, "/catalog/book").unwrap(), 2);
//! assert_eq!(xpath_str(&doc, "name(/catalog)").unwrap(), "catalog");
//! assert!(xpath_bool(&doc, "/catalog/book[@id='1']").unwrap());
//! ```
//!
//! # Example — reusable context
//! ```
//! use sup_xml_core::{parse_str, XPathContext, ParseOptions};
//!
//! let doc = parse_str(r#"<catalog><book id="1"/><book id="2"/></catalog>"#, &ParseOptions::default()).unwrap();
//! let ctx = XPathContext::new(&doc);
//!
//! assert_eq!(ctx.eval_count("/catalog/book").unwrap(), 2);
//! assert!(ctx.eval_bool("/catalog/book[@id='1']").unwrap());
//! ```

#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

pub mod ast;
pub mod context;
pub mod eval;
pub mod exslt;
pub mod pattern;
pub mod rtf;
mod index;
mod lexer;
mod parser;
mod bindings_builder;

pub use bindings_builder::XPathBindingsBuilder;

pub use ast::{Axis, Expr, LocationPath, NodeTest, Step};
pub use ast::{FunctionSig, ItemType, Occurrence, SequenceType};
pub use parser::parse_sequence_type_str;
pub use context::{DocIndex, INodeKind, is_synthetic_id};
pub use eval::Value as XPathValue;
pub use eval::compile_xpath_2_0_regex;
pub use index::{DocIndexLike, NodeId, XPathNodeKind};

use crate::error::Result;
use sup_xml_tree::dom::Document as ArenaDocument;

/// Knobs for the XPath 1.0 lexer + evaluator.  Default is strict
/// XPath 1.0 spec conformance; flipping `libxml2_compatible` relaxes
/// three points where libxml2 historically deviates from the spec:
///
/// 1. **Number literals with exponents** — XPath 1.0 § 3.5 forbids
///    `1e10` / `1.5e-3` style exponent notation in number literals;
///    libxml2 accepts them.  In compat mode we accept too.
/// 2. **`number('-')`** — XPath 1.0 § 4.4 says the result is `NaN`
///    when the argument is not a lexical number; libxml2 returns
///    `-0`.  In compat mode we return `-0`.
/// 3. **`string()` of large numbers** — XPath 1.0 § 4.2 mandates
///    decimal-only output; libxml2 emits `1.23456789012346e+19` in
///    scientific form for very large / very small magnitudes.  In
///    compat mode we emit libxml2's scientific form.
///
/// Default: `false` (strict — recommended).  Set to `true` only when
/// porting from libxml2-flavoured XPath corpora or pipelines.
#[derive(Debug, Clone)]
pub struct XPathOptions {
    pub libxml2_compatible: bool,
    /// Enable XPath 2.0 syntax additions that XPath 1.0 forbids:
    ///
    /// * `if (cond) then a else b` conditional expression
    /// * `for $v in seq return body` (with comma-chained bindings)
    /// * (more 2.0 grammar will land here as it grows)
    ///
    /// Off by default — XPath 1.0 is the spec contract callers
    /// depend on.  The XSLT compiler flips this on when the
    /// stylesheet declares `version="2.0"` (or higher).
    pub xpath_2_0: bool,
    /// Per-evaluation step ceiling — the cap on charged eval steps that
    /// bounds adversarial nested-predicate complexity (the `//*[//*[…]]`
    /// O(N^k) shape).  When exceeded, evaluation aborts with an error
    /// rather than hanging.  Defaults to [`eval::DEFAULT_MAX_EVAL_STEPS`]
    /// (20M) — comfortable for ordinary and generated XPath.  Lower it
    /// (e.g. 1–2M) when evaluating untrusted expressions to tighten the
    /// worst-case CPU bound; raise it for trusted, legitimately-expensive
    /// generated XPath.  A value of 0 makes every evaluation fail on its
    /// first step.
    pub max_eval_steps: u64,
}

impl Default for XPathOptions {
    fn default() -> Self {
        // Hand-written (not derived) because `max_eval_steps` must
        // default to the budget constant, not `u64`'s `0` — a derived
        // `Default` would silently zero the budget and reject everything.
        Self {
            libxml2_compatible: false,
            xpath_2_0: false,
            max_eval_steps: eval::DEFAULT_MAX_EVAL_STEPS,
        }
    }
}

/// Parse an XPath 1.0 expression string into its AST representation
/// using the default strict options.  See [`parse_xpath_with`] to
/// opt into libxml2-compatible lexing.
pub fn parse_xpath(src: &str) -> Result<Expr> {
    parse_xpath_with(src, &XPathOptions::default())
}

/// Parse an XPath expression with explicit [`XPathOptions`].  Today
/// the only knob that affects parsing is `libxml2_compatible`, which
/// makes the lexer accept exponent-notation in number literals.
pub fn parse_xpath_with(src: &str, opts: &XPathOptions) -> Result<Expr> {
    // XPath 2.0 §3.1.1 specifies `[E[sign]]Digits` exponents on
    // numeric literals (`xs:double(1.1234E99)`); XPath 1.0 doesn't.
    // Allow either when libxml2-compat is asked for OR when the
    // caller opted into XPath 2.0 — neither flag accidentally turns
    // a 1.0-only document's lex into a 2.0 lex because 1.0 doesn't
    // produce such tokens.
    let allow_exponent = opts.libxml2_compatible || opts.xpath_2_0;
    let (tokens, spans) = lexer::tokenize_with(src, allow_exponent)?;
    let mut p = parser::Parser::new_with_spans(tokens, spans, src);
    p.set_xpath_2_0(opts.xpath_2_0);
    let expr = p.parse_expr()?;
    p.expect_eof()?;
    let depth = ast::max_predicate_nesting(&expr);
    if depth > MAX_PREDICATE_NESTING_DEPTH {
        use crate::error::{ErrorDomain, ErrorLevel, XmlError};
        return Err(XmlError::new(
            ErrorDomain::XPath,
            ErrorLevel::Error,
            format!(
                "XPath predicate nesting depth ({depth}) exceeds limit \
                 ({MAX_PREDICATE_NESTING_DEPTH}); evaluator complexity is \
                 O(N^k) in document size N and predicate-nesting depth k"
            ),
        ));
    }
    Ok(expr)
}

/// Maximum predicate-nesting depth accepted by [`parse_xpath_with`].
///
/// XPath eval is O(N^k) in document size N and predicate-nesting depth
/// k.  Realistic queries rarely exceed depth 3 (`//section[chapter[
/// paragraph[contains(., 'x')]]]`); legitimate generated XPath
/// (e.g., SPARQL→XPath translation) sometimes reaches depth 4-5.  Eight
/// is a generous ceiling that lets every legitimate pattern through
/// while rejecting the obviously-adversarial inputs the fuzzer finds
/// (`//*[//*[//*[//*[//*[//*[//*[.='x']]]]]]]` at depth 7 burns the
/// full 500k step budget in tens of milliseconds — caught here in
/// microseconds at parse time).
pub const MAX_PREDICATE_NESTING_DEPTH: u32 = 8;

// ── arena tree variants ─────────────────────────────────────────────────────

/// Reusable XPath context for an arena-allocated [`ArenaDocument`].
///
/// Build once, evaluate many times.  Internally the XPath evaluator is
/// generic over [`DocIndexLike`].
pub struct XPathContext<'doc> {
    /// The flat index used by the evaluator.  Exposed so external
    /// adapters (e.g. the libxml2 C-ABI shim in `sup-xml-compat`)
    /// can build their own result-object wrappers without rebuilding
    /// the index.
    pub index: DocIndex<'doc>,
    /// Original document reference, retained so callers that need to
    /// serialize the synthetic Document node (the result of XPath `/`)
    /// can do so via [`Self::eval_node_xml`].
    doc: &'doc ArenaDocument,
    /// Strict / libxml2-compat knobs applied to both the lexer
    /// (exponent-notation acceptance) and the evaluator (`number('-')`
    /// and `string(big)` formatting), plus the per-evaluation step
    /// budget ([`XPathOptions::max_eval_steps`]).
    options: XPathOptions,
}

impl<'doc> XPathContext<'doc> {
    /// Build a strict-mode evaluation context for `doc`.  O(n) in
    /// the number of nodes.  Equivalent to
    /// [`new_with`](Self::new_with) with default [`XPathOptions`].
    pub fn new(doc: &'doc ArenaDocument) -> Self {
        Self::new_with(doc, XPathOptions::default())
    }

    /// Build an evaluation context for `doc` with explicit options.
    /// Use this to opt into libxml2-compatible behaviour for the
    /// three spec deviations called out on [`XPathOptions`].
    pub fn new_with(doc: &'doc ArenaDocument, options: XPathOptions) -> Self {
        Self { index: DocIndex::build(doc), doc, options }
    }

    pub fn eval(&self, src: &str) -> Result<XPathValue> {
        self.eval_with(src, 0, &eval::NoBindings)
    }

    /// Evaluate `src` against a specific context node.  XPath 1.0
    /// allows the context-node to be any node in the document; the
    /// libxml2 ABI exposes this via `xmlXPathContext.node`.
    pub fn eval_at(&self, src: &str, context_node: NodeId) -> Result<XPathValue> {
        self.eval_with(src, context_node, &eval::NoBindings)
    }

    /// Evaluate `src` against `context_node`, consulting `bindings`
    /// for user-registered functions (`extensions=` in lxml),
    /// variables (`$varname`), and namespace prefix resolution.  This
    /// is the entry point the libxml2 C-ABI shim
    /// (`sup-xml-compat::xpath`) uses when its `xmlXPathContext`
    /// carries registered namespaces, functions, or variables.
    pub fn eval_with(
        &self,
        src: &str,
        context_node: NodeId,
        bindings: &dyn eval::XPathBindings,
    ) -> Result<XPathValue> {
        let expr = parse_xpath_with(src, &self.options)?;
        // Static-context check (XPath 1.0 §1): every namespace
        // prefix in the expression must be bound.  Surfaces as
        // libxml2's XPATH_UNDEF_PREFIX_ERROR / lxml's
        // XPathEvalError before any tree walk happens.
        eval::validate_prefixes(&expr, bindings)?;
        // Fresh step-budget for this top-level evaluation —
        // caps adversarial nested-predicate complexity.  Seed the
        // thread-local ceiling from this context's options
        // (default 20M; tunable via `XPathOptions::max_eval_steps`).
        eval::set_eval_budget(self.options.max_eval_steps);
        eval::reset_eval_budget();
        // Sample a stable instant for this top-level evaluation so
        // repeated fn:current-* calls within it agree (XPath 2.0 §16).
        eval::refresh_stable_now();
        let static_ctx = eval::StaticContext {
            xpath_2_0: self.options.xpath_2_0,
            libxml2_compatible: self.options.libxml2_compatible,
            // `current()` returns this node regardless of how deep into
            // steps/predicates evaluation descends.
            current_node: Some(context_node),
        };
        eval::eval_expr(
            &expr,
            &eval::EvalCtx {
                context_node, pos: 1, size: 1, bindings,
                static_ctx: &static_ctx,
            },
            &self.index,
        )
    }

    /// Find the index ID for a node identified by its arena pointer.
    /// Returns `None` if the pointer doesn't match any indexed node.
    /// Linear scan; O(n) in document size — call once per evaluation,
    /// not per expression step.
    ///
    /// Handles attribute pointers too: libxslt sets the XPath context
    /// node to an `xmlAttr*` while iterating `@*` (`<xsl:for-each
    /// select="@*">`), and the C-ABI layer hands that pointer here cast
    /// to `*const Node`.  The match is by raw address, so comparing the
    /// indexed `&Attribute` against `ptr` is sound regardless of the
    /// nominal pointer type.
    pub fn id_for_element(&self, ptr: *const sup_xml_tree::dom::Node<'_>) -> Option<NodeId> {
        use crate::xpath::context::INodeKind;
        if ptr.is_null() { return Some(0); }
        let target = ptr as *const ();
        for (i, n) in self.index.nodes.iter().enumerate() {
            let matches = match n.kind {
                INodeKind::Element(p) | INodeKind::Text(p) | INodeKind::Comment(p)
                | INodeKind::CData(p)   | INodeKind::PI(p)
                    => (p as *const _ as *const ()) == target,
                INodeKind::Attribute(a)
                    => (a as *const _ as *const ()) == target,
                _ => false,
            };
            if matches { return Some(i); }
        }
        None
    }
    pub fn eval_bool(&self, src: &str) -> Result<bool> {
        Ok(eval::value_to_bool(&self.eval(src)?, &self.index))
    }
    pub fn eval_str(&self, src: &str) -> Result<String> {
        Ok(eval::value_to_string(&self.eval(src)?, &self.index))
    }
    pub fn eval_num(&self, src: &str) -> Result<f64> {
        Ok(eval::value_to_number(&self.eval(src)?, &self.index))
    }
    pub fn eval_strings(&self, src: &str) -> Result<Vec<String>> {
        let v = self.eval(src)?;
        Ok(match v {
            XPathValue::NodeSet(ns) => ns.iter().map(|&id| self.index.string_value(id)).collect(),
            other => vec![eval::value_to_string(&other, &self.index)],
        })
    }
    pub fn eval_count(&self, src: &str) -> Result<usize> {
        let v = self.eval(src)?;
        Ok(match v { XPathValue::NodeSet(ns) => ns.len(), _ => 0 })
    }

    /// Evaluate `src` and return each matched node serialized as XML
    /// (the XPath equivalent of `xmllint --xpath`'s default output).
    ///
    /// * Element / Text / Comment / CData / PI nodes serialize to their
    ///   subtree XML form, identical to [`crate::serialize_node_to_string`]
    ///   with default options.
    /// * Attribute nodes render as `name="value"` with the value
    ///   attribute-escaped.
    /// * Namespace nodes render as `xmlns:prefix="uri"` (or `xmlns="…"`
    ///   for the default namespace).
    /// * The synthetic Document node (returned by `/`) serializes as
    ///   the whole document via [`crate::serialize_to_string`].
    ///
    /// Non-NodeSet results (string / number / boolean) collapse to a
    /// single-element vec containing their XPath 1.0 string form,
    /// matching [`Self::eval_strings`].
    pub fn eval_node_xml(&self, src: &str) -> Result<Vec<String>> {
        use crate::serializer::{serialize_node_to_string, serialize_with, SerializeOptions};
        use crate::xpath::context::INodeKind;
        let v = self.eval(src)?;
        let ns = match v {
            XPathValue::NodeSet(ns) => ns,
            other => return Ok(vec![eval::value_to_string(&other, &self.index)]),
        };
        let opts = SerializeOptions::default();
        let mut out = Vec::with_capacity(ns.len());
        for id in ns {
            let node = &self.index.nodes[id];
            let serialized = match &node.kind {
                INodeKind::Element(n) | INodeKind::Text(n)
                | INodeKind::Comment(n) | INodeKind::CData(n)
                | INodeKind::PI(n) => serialize_node_to_string(n, &opts),
                INodeKind::Attribute(a) => {
                    let mut s = String::with_capacity(a.name().len() + a.value().len() + 3);
                    s.push_str(a.name());
                    s.push_str("=\"");
                    // XML attribute-value escaping: `&`, `<`, `"`.
                    // (`'` is allowed inside `"..."` quotes.)
                    for ch in a.value().chars() {
                        match ch {
                            '&' => s.push_str("&amp;"),
                            '<' => s.push_str("&lt;"),
                            '"' => s.push_str("&quot;"),
                            c   => s.push(c),
                        }
                    }
                    s.push('"');
                    s
                }
                INodeKind::Namespace { prefix: Some(p), uri } =>
                    format!("xmlns:{}=\"{}\"", p, uri),
                INodeKind::Namespace { prefix: None, uri } =>
                    format!("xmlns=\"{}\"", uri),
                INodeKind::Document => serialize_with(self.doc, &opts),
            };
            out.push(serialized);
        }
        Ok(out)
    }
}

/// One-shot XPath evaluation against an arena document.  Builds a fresh
/// index per call — use [`XPathContext`] to amortise.
pub fn xpath_eval(doc: &ArenaDocument, src: &str) -> Result<XPathValue> {
    XPathContext::new(doc).eval(src)
}
pub fn xpath_bool(doc: &ArenaDocument, src: &str) -> Result<bool> {
    XPathContext::new(doc).eval_bool(src)
}
pub fn xpath_str(doc: &ArenaDocument, src: &str) -> Result<String> {
    XPathContext::new(doc).eval_str(src)
}
pub fn xpath_num(doc: &ArenaDocument, src: &str) -> Result<f64> {
    XPathContext::new(doc).eval_num(src)
}
pub fn xpath_strings(doc: &ArenaDocument, src: &str) -> Result<Vec<String>> {
    XPathContext::new(doc).eval_strings(src)
}
pub fn xpath_count(doc: &ArenaDocument, src: &str) -> Result<usize> {
    XPathContext::new(doc).eval_count(src)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod arena_tests {
    use super::*;
    use crate::{parse_str, ParseOptions};

    fn parse(xml: &str) -> ArenaDocument {
        parse_str(xml, &ParseOptions::default()).expect("parse")
    }

    fn parse_ns(xml: &str) -> ArenaDocument {
        let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
        parse_str(xml, &opts).expect("parse")
    }

    // ── absolute paths + child axis ──────────────────────────────────────

    #[test]
    fn absolute_path() {
        let doc = parse(r#"<catalog><book id="1"/><book id="2"/></catalog>"#);
        assert_eq!(xpath_count(&doc, "/catalog/book").unwrap(), 2);
        assert_eq!(xpath_count(&doc, "/catalog").unwrap(), 1);
    }

    #[test]
    fn wildcard_children() {
        let doc = parse("<r><a/><b/><c/></r>");
        assert_eq!(xpath_count(&doc, "/r/*").unwrap(), 3);
    }

    // ── attribute axis ───────────────────────────────────────────────────

    #[test]
    fn attribute_predicate() {
        let doc = parse(r#"<r><book id="1"/><book id="2"/><book id="1"/></r>"#);
        assert_eq!(xpath_count(&doc, "/r/book[@id='1']").unwrap(), 2);
        assert!(xpath_bool(&doc, "/r/book[@id='1']").unwrap());
    }

    #[test]
    fn current_in_nested_predicate_is_fixed_to_the_expression_context() {
        // XSLT 1.0 §12.4: current() returns the instruction's current
        // node, which stays fixed as evaluation descends into steps and
        // predicates — unlike the context node `.`.  ISO Schematron's
        // phase selection relies on this:
        //   ../phase[@id=$p]/active[@pattern=current()/@id]
        // evaluated from a <pattern> must see current()=<pattern>, not the
        // <active> the inner predicate is filtering.
        let doc = parse("<s><phase id='m'><active pat='p1'/></phase>\
                         <p id='p1'/><p id='p2'/></s>");
        let ctx = XPathContext::new(&doc);
        let node_of = |q: &str| match ctx.eval(q).unwrap() {
            XPathValue::NodeSet(ns) => ns[0],
            other => panic!("expected node-set, got {other:?}"),
        };
        let expr = "../phase[@id='m']/active[@pat=current()/@id]";
        // From p1 the active row whose @pat = current()/@id (= p1) matches.
        let hit = ctx.eval_at(expr, node_of("//p[@id='p1']")).unwrap();
        assert!(matches!(hit, XPathValue::NodeSet(ref ns) if ns.len() == 1),
            "current() in the nested predicate must resolve to p1: {hit:?}");
        // From p2 there is no matching active row.
        let miss = ctx.eval_at(expr, node_of("//p[@id='p2']")).unwrap();
        assert!(matches!(miss, XPathValue::NodeSet(ref ns) if ns.is_empty()),
            "expected no match from p2: {miss:?}");
    }

    #[test]
    fn attribute_value_extraction() {
        let doc = parse(r#"<r id="42"/>"#);
        assert_eq!(xpath_str(&doc, "/r/@id").unwrap(), "42");
    }

    // ── descendant axis ──────────────────────────────────────────────────

    #[test]
    fn descendant_or_self() {
        let doc = parse("<r><a><b><c/></b></a></r>");
        assert_eq!(xpath_count(&doc, "//c").unwrap(), 1);
        assert_eq!(xpath_count(&doc, "//*").unwrap(), 4); // r, a, b, c
    }

    // ── string functions ─────────────────────────────────────────────────

    #[test]
    fn string_value_extraction() {
        let doc = parse("<r><title>Hello</title></r>");
        assert_eq!(xpath_str(&doc, "/r/title").unwrap(), "Hello");
        assert_eq!(xpath_str(&doc, "string(/r/title)").unwrap(), "Hello");
    }

    #[test]
    fn string_concat_children() {
        let doc = parse("<r>foo<b>bar</b>baz</r>");
        assert_eq!(xpath_str(&doc, "string(/r)").unwrap(), "foobarbaz");
    }

    #[test]
    fn name_functions() {
        let doc = parse("<r><a/></r>");
        assert_eq!(xpath_str(&doc, "name(/r)").unwrap(), "r");
        assert_eq!(xpath_str(&doc, "name(/r/a)").unwrap(), "a");
    }

    // ── substring ────────────────────────────────────────────────────────
    //
    // XPath 1.0 §4.2: result is the chars at 1-based positions p where
    // round(start) <= p < round(start) + round(len).  NaN / ±Inf in
    // either argument must not panic — the spec yields an empty range
    // (or, when only +Inf appears on the length side, the rest of the
    // string).  Regression coverage for an Inf-induced usize overflow
    // discovered by fuzz_xpath_eval.

    #[test]
    fn substring_basic() {
        let doc = parse("<r/>");
        assert_eq!(xpath_str(&doc, r#"substring("12345", 2, 3)"#).unwrap(), "234");
        assert_eq!(xpath_str(&doc, r#"substring("12345", 2)"#).unwrap(), "2345");
        // Spec example: round(1.5) = 2, round(2.6) = 3, range 2..5.
        assert_eq!(xpath_str(&doc, r#"substring("12345", 1.5, 2.6)"#).unwrap(), "234");
        // Spec example: range 0..3 includes positions 1, 2.
        assert_eq!(xpath_str(&doc, r#"substring("12345", 0, 3)"#).unwrap(), "12");
    }

    #[test]
    fn substring_negative_start() {
        let doc = parse("<r/>");
        // round(-3) = -3, +5 → range -3..2, includes position 1 only.
        assert_eq!(xpath_str(&doc, r#"substring("12345", -3, 5)"#).unwrap(), "1");
        // Start past end of string → empty.
        assert_eq!(xpath_str(&doc, r#"substring("abc", 10, 5)"#).unwrap(), "");
        // Length zero → empty.
        assert_eq!(xpath_str(&doc, r#"substring("abc", 1, 0)"#).unwrap(), "");
    }

    #[test]
    fn substring_infinite_start_does_not_panic() {
        // Regression: `1 div 0` is +Inf in XPath; pre-fix this
        // overflowed when computing `(start as usize) + (len as usize)`.
        let doc = parse("<r/>");
        assert_eq!(xpath_str(&doc, r#"substring("hello", 1 div 0, 5)"#).unwrap(), "");
        assert_eq!(xpath_str(&doc, r#"substring("hello", -1 div 0, 5)"#).unwrap(), "");
    }

    #[test]
    fn substring_infinite_length() {
        let doc = parse("<r/>");
        // Spec example: range -42..+Inf includes every position → whole string.
        assert_eq!(
            xpath_str(&doc, r#"substring("12345", -42, 1 div 0)"#).unwrap(),
            "12345",
        );
        // Negative-infinity length → empty (range collapses below start).
        assert_eq!(xpath_str(&doc, r#"substring("abc", 1, -1 div 0)"#).unwrap(), "");
    }

    #[test]
    fn substring_nan_yields_empty() {
        // `0 div 0` is NaN.  Spec: result is empty.
        let doc = parse("<r/>");
        assert_eq!(xpath_str(&doc, r#"substring("hello", 0 div 0, 5)"#).unwrap(), "");
        assert_eq!(xpath_str(&doc, r#"substring("hello", 1, 0 div 0)"#).unwrap(), "");
        // Mixed: -Inf + +Inf = NaN.
        assert_eq!(
            xpath_str(&doc, r#"substring("hello", -1 div 0, 1 div 0)"#).unwrap(),
            "",
        );
    }

    // ── number functions / arithmetic ────────────────────────────────────

    #[test]
    fn count_function() {
        let doc = parse("<r><i/><i/><i/></r>");
        assert_eq!(xpath_num(&doc, "count(/r/i)").unwrap(), 3.0);
    }

    #[test]
    fn arithmetic() {
        let doc = parse("<r/>");
        assert_eq!(xpath_num(&doc, "2 + 3 * 4").unwrap(), 14.0);
        assert_eq!(xpath_num(&doc, "10 div 4").unwrap(), 2.5);
    }

    // ── predicates ───────────────────────────────────────────────────────

    #[test]
    fn position_predicate() {
        let doc = parse("<r><i>1</i><i>2</i><i>3</i></r>");
        assert_eq!(xpath_str(&doc, "/r/i[1]").unwrap(), "1");
        assert_eq!(xpath_str(&doc, "/r/i[2]").unwrap(), "2");
        assert_eq!(xpath_str(&doc, "/r/i[last()]").unwrap(), "3");
    }

    #[test]
    fn numeric_predicate_on_text() {
        let doc = parse("<r><i>5</i><i>15</i><i>25</i></r>");
        assert_eq!(xpath_count(&doc, "/r/i[. > 10]").unwrap(), 2);
    }

    // ── namespace axis ───────────────────────────────────────────────────

    /// Test-only bindings impl that registers a fixed prefix→URI map.
    /// Mirrors what lxml's `namespaces=` kwarg / libxslt's
    /// `xmlXPathRegisterNs` install on the context.
    struct TestNs(&'static [(&'static str, &'static str)]);
    impl eval::XPathBindings for TestNs {
        fn resolve_prefix(&self, prefix: &str) -> Option<String> {
            self.0.iter().find(|(p, _)| *p == prefix).map(|(_, u)| (*u).to_string())
        }
    }

    #[test]
    fn prefixed_element_addressable_by_qname() {
        let doc = parse_ns(r#"<r xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>X</dc:title></r>"#);
        let ctx = XPathContext::new(&doc);
        let bind = TestNs(&[("dc", "http://purl.org/dc/elements/1.1/")]);
        let v = ctx.eval_with("namespace-uri(/r/dc:title)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_string(&v, &ctx.index),
                   "http://purl.org/dc/elements/1.1/");
        let v = ctx.eval_with("local-name(/r/dc:title)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_string(&v, &ctx.index), "title");
    }

    /// XPath 1.0 §1 — every prefix in the expression must be bound
    /// in the static context.  Querying a prefixed name without
    /// registering the prefix must raise an error (libxml2's
    /// XPATH_UNDEF_PREFIX_ERROR; lxml's XPathEvalError).
    #[test]
    fn undefined_prefix_in_qname_errors() {
        let doc = parse_ns(r#"<r xmlns:fa="urn:x"><fa:d/></r>"#);
        let err = xpath_str(&doc, "/fa:d").unwrap_err();
        assert!(
            err.message.contains("Undefined namespace prefix"),
            "expected undefined-prefix error, got: {}", err.message,
        );
    }

    #[test]
    fn undefined_prefix_in_prefix_wildcard_errors() {
        let doc = parse_ns(r#"<r xmlns:fa="urn:x"><fa:d/></r>"#);
        let err = xpath_str(&doc, "/fa:*").unwrap_err();
        assert!(
            err.message.contains("Undefined namespace prefix"),
            "expected undefined-prefix error, got: {}", err.message,
        );
    }

    /// Undefined prefixes nested inside a predicate are also caught
    /// at static-context check time — before any tree walk happens.
    #[test]
    fn undefined_prefix_in_predicate_errors() {
        let doc = parse_ns("<r><a/></r>");
        let err = xpath_str(&doc, "/r/a[fa:flag]").unwrap_err();
        assert!(
            err.message.contains("Undefined namespace prefix"),
            "expected undefined-prefix error, got: {}", err.message,
        );
    }

    /// Every element carries an implicit `xml` namespace node per
    /// the XML Namespaces recommendation — even on an element with
    /// no `xmlns:*` declarations.
    #[test]
    fn namespace_axis_implicit_xml_prefix() {
        let doc = parse_ns("<r/>");
        assert_eq!(xpath_count(&doc, "/r/namespace::*").unwrap(), 1);
        // The implicit binding's "name" (local-name) is "xml" and
        // its string-value is the XML namespace URI.
        assert_eq!(
            xpath_str(&doc, "string(/r/namespace::*[1])").unwrap(),
            "http://www.w3.org/XML/1998/namespace",
        );
    }

    /// `xmlns:dc="…"` adds one prefixed binding; combined with the
    /// implicit `xml` that's two namespace nodes total.
    #[test]
    fn namespace_axis_one_declared_plus_xml() {
        let doc = parse_ns(
            r#"<r xmlns:dc="http://purl.org/dc/elements/1.1/"><c/></r>"#,
        );
        assert_eq!(xpath_count(&doc, "/r/namespace::*").unwrap(), 2);
        // The child inherits the dc binding, so it also has 2.
        assert_eq!(xpath_count(&doc, "/r/c/namespace::*").unwrap(), 2);
    }

    /// Named name-test on the namespace axis — `namespace::dc`
    /// matches the namespace node whose prefix is `dc`.
    #[test]
    fn namespace_axis_name_test_selects_by_prefix() {
        let doc = parse_ns(
            r#"<r xmlns:dc="http://purl.org/dc/elements/1.1/"/>"#,
        );
        assert_eq!(
            xpath_str(&doc, "string(/r/namespace::dc)").unwrap(),
            "http://purl.org/dc/elements/1.1/",
        );
        // local-name() of a namespace node is its prefix.
        assert_eq!(
            xpath_str(&doc, "local-name(/r/namespace::dc)").unwrap(),
            "dc",
        );
    }

    /// Default namespace (xmlns="…") produces a namespace node
    /// whose prefix (and therefore local-name) is empty.
    #[test]
    fn namespace_axis_default_namespace_has_empty_prefix() {
        let doc = parse_ns(r#"<r xmlns="http://example.com/ns"/>"#);
        // Strict XPath §2.3: an unprefixed name has null namespace.
        // We bind a prefix to address the default-namespaced `r`.
        let ctx = XPathContext::new(&doc);
        let bind = TestNs(&[("x", "http://example.com/ns")]);
        // 2 namespace nodes on `x:r`: the default + xml.
        let v = ctx.eval_with("count(/x:r/namespace::*)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_number(&v, &ctx.index), 2.0);
        // Default ns's local-name is the empty string.
        let v = ctx
            .eval_with("string(/x:r/namespace::*[local-name()=''])", 0, &bind)
            .unwrap();
        assert_eq!(eval::value_to_string(&v, &ctx.index), "http://example.com/ns");
    }

    /// A descendant element sees inherited bindings.
    #[test]
    fn namespace_axis_inherits_from_ancestor() {
        let doc = parse_ns(
            r#"<r xmlns:a="urn:a"><mid xmlns:b="urn:b"><leaf/></mid></r>"#,
        );
        // leaf sees a + b + xml = 3.
        assert_eq!(xpath_count(&doc, "/r/mid/leaf/namespace::*").unwrap(), 3);
        assert_eq!(
            xpath_str(&doc, "string(/r/mid/leaf/namespace::a)").unwrap(),
            "urn:a",
        );
        assert_eq!(
            xpath_str(&doc, "string(/r/mid/leaf/namespace::b)").unwrap(),
            "urn:b",
        );
    }

    /// Re-declaring a prefix on a descendant shadows the ancestor's
    /// binding: the descendant's `namespace::p` resolves to the
    /// *closer* declaration.
    #[test]
    fn namespace_axis_shadows_ancestor() {
        let doc = parse_ns(
            r#"<r xmlns:p="urn:outer"><c xmlns:p="urn:inner"/></r>"#,
        );
        assert_eq!(
            xpath_str(&doc, "string(/r/namespace::p)").unwrap(),
            "urn:outer",
        );
        assert_eq!(
            xpath_str(&doc, "string(/r/c/namespace::p)").unwrap(),
            "urn:inner",
        );
        // Each element still has exactly p + xml = 2.
        assert_eq!(xpath_count(&doc, "/r/c/namespace::*").unwrap(), 2);
    }

    /// `xmlns=""` undeclares the default namespace for this element
    /// and its descendants — no namespace node for the default.
    #[test]
    fn namespace_axis_default_undeclaration() {
        let doc = parse_ns(
            r#"<r xmlns="urn:outer"><c xmlns=""/></r>"#,
        );
        let ctx = XPathContext::new(&doc);
        let bind = TestNs(&[("x", "urn:outer")]);
        // x:r: default + xml = 2.  Inner `c` undeclares the default,
        // so it lives in no namespace and is reached unprefixed.
        let r_count = ctx.eval_with("count(/x:r/namespace::*)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_number(&r_count, &ctx.index), 2.0);
        let c_count = ctx.eval_with("count(/x:r/c/namespace::*)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_number(&c_count, &ctx.index), 1.0);
    }

    // ── EXSLT end-to-end (through eval_with + TestNs bindings) ──────────
    //
    // These prove that prefix:function dispatch flows correctly
    // through XPathBindings → exslt::dispatch, not just that the
    // family-internal dispatchers work in isolation.

    const EXSLT_NS: &[(&str, &str)] = &[
        ("math", "http://exslt.org/math"),
        ("date", "http://exslt.org/dates-and-times"),
        ("str",  "http://exslt.org/strings"),
        ("set",  "http://exslt.org/sets"),
    ];

    #[test]
    fn exslt_math_max_via_xpath() {
        let doc = parse("<r><i>3</i><i>7</i><i>1</i><i>5</i></r>");
        let ctx = XPathContext::new(&doc);
        let v = ctx.eval_with("math:max(/r/i)", 0, &TestNs(EXSLT_NS)).unwrap();
        assert_eq!(eval::value_to_number(&v, &ctx.index), 7.0);
    }

    #[test]
    fn exslt_str_padding_via_xpath() {
        let doc = parse("<r/>");
        let ctx = XPathContext::new(&doc);
        let v = ctx.eval_with("str:padding(5, '*')", 0, &TestNs(EXSLT_NS)).unwrap();
        assert_eq!(eval::value_to_string(&v, &ctx.index), "*****");
    }

    #[test]
    fn exslt_set_distinct_via_xpath() {
        let doc = parse("<r><i>x</i><i>y</i><i>x</i><i>y</i><i>z</i></r>");
        let ctx = XPathContext::new(&doc);
        let v = ctx.eval_with("count(set:distinct(/r/i))", 0, &TestNs(EXSLT_NS)).unwrap();
        // 5 nodes, 3 distinct string-values → 3.
        assert_eq!(eval::value_to_number(&v, &ctx.index), 3.0);
    }

    #[test]
    fn exslt_date_year_via_xpath() {
        let doc = parse("<r/>");
        let ctx = XPathContext::new(&doc);
        let v = ctx.eval_with(
            "date:year('2024-07-04T12:00:00Z')", 0, &TestNs(EXSLT_NS),
        ).unwrap();
        assert_eq!(eval::value_to_number(&v, &ctx.index), 2024.0);
    }

    /// EXSLT functions live under their declared namespace URI, not
    /// the prefix.  Verifying that the dispatcher matches by URI
    /// means a caller can pick any prefix for the binding.
    #[test]
    fn exslt_prefix_is_user_choice() {
        let doc = parse("<r><i>5</i><i>3</i></r>");
        let ctx = XPathContext::new(&doc);
        // Bind `MATHS` (not the conventional `math`) to the math URI.
        let bind = TestNs(&[("MATHS", "http://exslt.org/math")]);
        let v = ctx.eval_with("MATHS:min(/r/i)", 0, &bind).unwrap();
        assert_eq!(eval::value_to_number(&v, &ctx.index), 3.0);
    }

    /// Functions in a registered EXSLT namespace but with an unknown
    /// local name surface as "Unregistered XPath function".
    #[test]
    fn unknown_exslt_function_in_registered_ns_errors() {
        let doc = parse("<r/>");
        let ctx = XPathContext::new(&doc);
        let err = ctx.eval_with(
            "math:does-not-exist()", 0, &TestNs(EXSLT_NS),
        ).unwrap_err();
        assert!(
            err.message.contains("Unregistered XPath function"),
            "unexpected error: {}", err.message,
        );
    }

    // ── boolean / set operators ──────────────────────────────────────────

    #[test]
    fn union_dedups() {
        let doc = parse("<r><a/><b/></r>");
        assert_eq!(xpath_count(&doc, "/r/a | /r/b | /r/a").unwrap(), 2);
    }

    #[test]
    fn boolean_and_or() {
        let doc = parse("<r><a/><b/></r>");
        assert!(xpath_bool(&doc, "/r/a or /r/missing").unwrap());
        assert!(!xpath_bool(&doc, "/r/a and /r/missing").unwrap());
    }

    // ── context reuse ────────────────────────────────────────────────────

    #[test]
    fn context_amortises_index_build() {
        let doc = parse("<r><book/><book/><book/></r>");
        let ctx = XPathContext::new(&doc);
        assert_eq!(ctx.eval_count("/r/book").unwrap(), 3);
        assert_eq!(ctx.eval_count("//book").unwrap(), 3);
        assert!(ctx.eval_bool("/r").unwrap());
    }

    // ── XPath 3.1 higher-order functions ─────────────────────────────────

    fn eval31(src: &str) -> String {
        let doc = parse("<r/>");
        let opts = XPathOptions { xpath_2_0: true, ..XPathOptions::default() };
        let ctx = XPathContext::new_with(&doc, opts);
        // The map:/array:/math: prefixes are predeclared in an XSLT 3.0
        // static context; bind them here so the raw-XPath harness sees
        // the standard URIs.
        let bind = TestNs(&[
            ("map",   "http://www.w3.org/2005/xpath-functions/map"),
            ("array", "http://www.w3.org/2005/xpath-functions/array"),
            ("math",  "http://www.w3.org/2005/xpath-functions/math"),
        ]);
        eval::value_to_string(&ctx.eval_with(src, 0, &bind).expect("eval"), &ctx.index)
    }

    #[test]
    fn inline_function_and_dynamic_call() {
        assert_eq!(eval31("let $f := function($x) { $x * 2 } return $f(21)"), "42");
    }

    #[test]
    fn inline_function_captures_closure() {
        assert_eq!(
            eval31("let $n := 10 return (function($x) { $x + $n })(5)"),
            "15");
    }

    #[test]
    fn let_single_and_chained_bindings() {
        assert_eq!(eval31("let $x := 40 return $x + 2"), "42");
        // Later bindings see earlier ones.
        assert_eq!(eval31("let $x := 3, $y := $x * 4 return $x + $y"), "15");
    }

    #[test]
    fn let_binds_whole_sequence() {
        // Unlike `for`, `let` binds the entire sequence as one value.
        assert_eq!(eval31("let $s := (1, 2, 3, 4) return count($s)"), "4");
        assert_eq!(eval31("let $s := 1 to 5 return sum($s)"), "15");
    }

    #[test]
    fn let_nested_in_for() {
        assert_eq!(
            eval31("string-join(for $i in 1 to 3 return \
                let $sq := $i * $i return string($sq), ' ')"),
            "1 4 9");
    }

    #[test]
    fn parse_json_objects_arrays_scalars() {
        assert_eq!(eval31("parse-json('[1,2,3]')?2"), "2");
        assert_eq!(eval31("parse-json('{\"a\":10,\"b\":20}')?b"), "20");
        assert_eq!(eval31("parse-json('{\"a\":[5,6,7]}')?a?3"), "7");
        assert_eq!(eval31("parse-json('\"hi\\u0041\"')"), "hiA");
        assert_eq!(eval31("parse-json('true')"), "true");
        // null maps to the empty sequence.
        assert_eq!(eval31("count(parse-json('null'))"), "0");
        assert_eq!(eval31("parse-json('{\"x\":null}')?x => count()"), "0");
    }

    #[test]
    fn parse_json_duplicate_key_policies() {
        // Default is use-first.
        assert_eq!(eval31("parse-json('{\"k\":1,\"k\":2}')?k"), "1");
        assert_eq!(
            eval31("parse-json('{\"k\":1,\"k\":2}', map{'duplicates':'use-last'})?k"),
            "2");
    }

    #[test]
    fn xml_to_json_round_trips_vocabulary() {
        let doc = parse_ns(concat!(
            r#"<map xmlns="http://www.w3.org/2005/xpath-functions">"#,
            r#"<string key="a">hi</string>"#,
            r#"<number key="n">42</number>"#,
            r#"<boolean key="b">true</boolean>"#,
            r#"<array key="xs"><number>1</number><number>2</number></array>"#,
            r#"<null key="z"/>"#,
            r#"</map>"#));
        let opts = XPathOptions { xpath_2_0: true, ..XPathOptions::default() };
        let ctx = XPathContext::new_with(&doc, opts);
        let got = ctx.eval_str("xml-to-json(/)").unwrap();
        assert_eq!(got, r#"{"a":"hi","n":42,"b":true,"xs":[1,2],"z":null}"#);
    }

    #[test]
    fn let_rejected_under_xpath_1_0() {
        // `let` is an XPath 3.0 construct; under the default XPath 1.0
        // grammar `let $x := …` must not parse (the `:=` / `$` after a
        // bare `let` name-test is a syntax error).  Mirrors how `for`,
        // maps, arrays, and inline functions are all gated on the
        // 2.0+ flag.
        let doc = parse("<r/>");
        let ctx = XPathContext::new(&doc); // default options → XPath 1.0
        assert!(
            ctx.eval("let $x := 1 return $x").is_err(),
            "`let` must not parse under XPath 1.0",
        );
        // The same expression parses once 2.0+ syntax is enabled.
        assert_eq!(eval31("let $x := 1 return $x"), "1");
    }

    #[test]
    fn named_function_reference_via_for_each() {
        assert_eq!(
            eval31("string-join(for-each(('a','bb','ccc'), string-length#1), ',')"),
            "1,2,3");
    }

    #[test]
    fn fn_for_each_and_filter() {
        assert_eq!(
            eval31("string-join(for-each(1 to 4, function($x){ $x * $x }), ' ')"),
            "1 4 9 16");
        assert_eq!(
            eval31("string-join(filter(1 to 6, function($x){ $x mod 2 = 0 }), ' ')"),
            "2 4 6");
    }

    #[test]
    fn fn_fold_left_right() {
        assert_eq!(
            eval31("fold-left(1 to 5, 0, function($a,$b){ $a + $b })"),
            "15");
        assert_eq!(
            eval31("fold-right(('a','b','c'), '', function($x,$a){ concat($x,$a) })"),
            "abc");
    }

    #[test]
    fn map_for_each_higher_order() {
        // Sum the values of a map via map:for-each + sum().
        assert_eq!(
            eval31("sum(map:for-each(map{'a':1,'b':2,'c':3}, function($k,$v){ $v }))"),
            "6");
    }

    #[test]
    fn array_for_each_and_fold() {
        assert_eq!(
            eval31("array:fold-left([1,2,3,4], 0, function($a,$b){ $a + $b })"),
            "10");
        assert_eq!(
            eval31("array:size(array:for-each([1,2,3], function($x){ $x * $x }))"),
            "3");
    }

    #[test]
    fn fn_apply_and_arity() {
        assert_eq!(eval31("function-arity(function($a,$b){ $a }) "), "2");
        assert_eq!(eval31("apply(concat#3, ['a','b','c'])"), "abc");
    }
}
