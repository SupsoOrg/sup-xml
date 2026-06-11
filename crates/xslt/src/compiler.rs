//! Stylesheet compiler — walks a parsed XSLT document and produces
//! a [`StylesheetAst`].
//!
//! The compiler is *total*: any structural problem produces an
//! `XsltError::InvalidStylesheet`; unsupported XSLT instructions
//! compile to `Instr::Unsupported { name }` rather than failing
//! compilation, so a stylesheet with one corner-case instruction
//! still compiles and runs (the unsupported instruction errors
//! only if execution actually reaches it).
//!
//! XPath expressions in attributes are pre-parsed to
//! [`sup_xml_core::xpath::Expr`] up-front — saves the re-parse
//! per-evaluation cost, and lets compile-time catch malformed XPath.

use sup_xml_core::xpath::{parse_xpath_with, Expr, XPathOptions};
use sup_xml_tree::dom::{Document, Node, Attribute, NodeKind};

use crate::ast::*;
use crate::error::XsltError;
use crate::loader::Loader;
use crate::whitespace::is_xslt_whitespace_only;
use crate::XSLT_NS;

// ── XSLT 2.0 mode plumbing ────────────────────────────────────────
//
// Set once at the top of `compile()` from the stylesheet's `version`
// attribute; read by every `parse_xpath` call inside compile to pick
// the right XPath grammar.  Thread-local because compile runs
// synchronously and never crosses threads — and that lets us avoid
// threading a `CompileCtx` through every `compile_*` free function.
thread_local! {
    static XSLT_2_0_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Independent toggle for XPath 2.0 grammar.  Usually moves in
    /// lockstep with [`XSLT_2_0_MODE`], but the simplified-stylesheet
    /// path enables XSLT 2.0 instructions while keeping XPath 1.0
    /// (XPath 2.0 reserves keywords like `eq` / `ne` / `to` / `is`
    /// that legitimate 1.0 stylesheets use as element names).
    static XPATH_2_0_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// True when the surrounding stylesheet's declared `version=`
    /// exceeds the processor's supported version (we support 2.0).
    /// XSLT 2.0 §3.5 says forwards-compat mode silently accepts
    /// unknown elements, unknown attributes, and similar surface
    /// extensions — so per-element XTSE0090 / XTSE0010 validators
    /// must defer to runtime rather than refuse the stylesheet.
    static FORWARDS_COMPAT_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Evaluated `<xsl:param static="yes">` values (XSLT 3.0 §3.5),
    /// keyed by expanded-name.  Available to `use-when` and shadow
    /// attributes at compile time.  Populated by the static-param
    /// pre-scan in `compile`; cleared when compilation finishes.
    static STATIC_PARAMS: std::cell::RefCell<
        std::collections::HashMap<String, sup_xml_core::xpath::eval::Value>>
        = std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    /// Package library for `xsl:use-package` resolution: package name →
    /// (source text, base URI).  Populated by
    /// [`compile_with_packages`]; cleared when that returns.
    static PACKAGE_SOURCES: std::cell::RefCell<
        std::collections::HashMap<String, (String, Option<String>)>>
        = std::cell::RefCell::new(std::collections::HashMap::new());
    /// Static base URI for the module currently being compiled —
    /// consulted by `evaluate_use_when_at` so `fn:static-base-uri()`
    /// inside a use-when answers with the right path.
    static MODULE_BASE_URI: std::cell::RefCell<Option<String>>
        = const { std::cell::RefCell::new(None) };
}

fn get_package_source(name: &str) -> Option<(String, Option<String>)> {
    PACKAGE_SOURCES.with(|m| m.borrow().get(name).cloned())
}

/// Compile `text` with a package library available for
/// `xsl:use-package` resolution (keyed by package name → (source,
/// base)).  Used by the conformance runner, which sources secondary
/// packages from the test catalog's `<package>` declarations.
pub fn compile_with_packages(
    text:     &str,
    loader:   &dyn Loader,
    base:     Option<&str>,
    packages: std::collections::HashMap<String, (String, Option<String>)>,
) -> Result<StylesheetAst, XsltError> {
    PACKAGE_SOURCES.with(|m| *m.borrow_mut() = packages);
    let mut counter = TOP_LEVEL_IMPORT_PRECEDENCE;
    let r = compile_with_imports(text, loader, base, StylesheetAst::default(), &mut counter);
    PACKAGE_SOURCES.with(|m| m.borrow_mut().clear());
    r
}

/// XSLT 2.0 §3.6 / XTSE0265 — every module in a stylesheet package
/// must declare the same `input-type-annotations` setting; mixing
/// `strip` in one module and `preserve` in another is a static
/// error.  An `unspecified` value (or no attribute) is compatible
/// with anything.  Called from the post-merge `finalize` so every
/// compile entry point shares the same check.
pub(crate) fn validate_input_type_annotations(
    ast: &StylesheetAst,
) -> Result<(), XsltError> {
    let mut has_strip    = false;
    let mut has_preserve = false;
    for v in &ast.input_type_annotations {
        match v.as_str() {
            "strip"    => has_strip = true,
            "preserve" => has_preserve = true,
            _ => {}
        }
    }
    if has_strip && has_preserve {
        return Err(XsltError::InvalidStylesheet(
            "stylesheet declares input-type-annotations='strip' in one \
             module and 'preserve' in another (XTSE0265)".into()));
    }
    Ok(())
}

fn set_static_param(key: String, v: sup_xml_core::xpath::eval::Value) {
    STATIC_PARAMS.with(|m| { m.borrow_mut().insert(key, v); });
}
fn get_static_param(name: &str) -> Option<sup_xml_core::xpath::eval::Value> {
    STATIC_PARAMS.with(|m| m.borrow().get(name).cloned())
}
fn clear_static_params() {
    STATIC_PARAMS.with(|m| m.borrow_mut().clear());
}

/// True when the surrounding `compile()` is running on a stylesheet
/// whose declared version exceeds 2.0 — forwards-compatible mode
/// per XSLT 2.0 §3.5.  Validators that would otherwise raise
/// XTSE0090 / XTSE0010 for unknown attributes / elements skip the
/// check in this mode.
pub(crate) fn in_forwards_compat_mode() -> bool {
    FORWARDS_COMPAT_MODE.with(|c| c.get())
}

/// True when the surrounding `compile()` was invoked on a stylesheet
/// that declared `version="2.0"` (or higher).  Visible to other
/// modules so the XPath function table can register / hide 2.0-only
/// functions appropriately.
pub(crate) fn is_xslt_2_0_compile() -> bool {
    XSLT_2_0_MODE.with(|c| c.get())
}

pub(crate) fn is_xpath_2_0_compile() -> bool {
    XPATH_2_0_MODE.with(|c| c.get())
}

/// RAII guard that flips the thread-local for the duration of one
/// `compile()` call and restores the previous value when dropped.
struct XsltModeGuard { prev_xslt: bool, prev_xpath: bool, prev_fwd: bool }
impl XsltModeGuard {
    fn enter_full(xslt_2_0: bool, xpath_2_0: bool, fwd: bool) -> Self {
        let prev_xslt  = XSLT_2_0_MODE.with(|c| c.replace(xslt_2_0));
        let prev_xpath = XPATH_2_0_MODE.with(|c| c.replace(xpath_2_0));
        let prev_fwd   = FORWARDS_COMPAT_MODE.with(|c| c.replace(fwd));
        Self { prev_xslt, prev_xpath, prev_fwd }
    }
}
impl Drop for XsltModeGuard {
    fn drop(&mut self) {
        let (px, py, pf) = (self.prev_xslt, self.prev_xpath, self.prev_fwd);
        XSLT_2_0_MODE.with(|c| c.set(px));
        XPATH_2_0_MODE.with(|c| c.set(py));
        FORWARDS_COMPAT_MODE.with(|c| c.set(pf));
    }
}

/// Variant that also resolves any `$prefix:local` variable
/// references in the parsed expression to Clark form
/// (`${uri}local`) using `node`'s in-scope namespaces.  Required
/// when the XPath is embedded in a template whose namespace
/// bindings haven't been hoisted to the stylesheet root — without
/// this, `$Q:pvar1` (with `xmlns:Q="…"` on the surrounding
/// template) wouldn't resolve at apply time because the runtime
/// only consults the stylesheet-root context.
/// Skip past whitespace and XPath 2.0 comments `(:...:)` (which may
/// nest) at the head of `s` so callers that inspect the first real
/// character of a pattern don't confuse a leading comment for a
/// grouping paren.
fn strip_leading_xpath_comments_and_space(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut pos = 0;
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
                // Unterminated comment — let the XPath parser surface
                // the real error; for the leading-paren check, treat
                // the whole input as "after the comment".
                return "";
            }
            continue;
        }
        break;
    }
    &s[pos..]
}

fn parse_xpath_at(node: &Node, src: &str) -> sup_xml_core::error::Result<Expr> {
    let mut expr = parse_xpath(src)?;
    resolve_xpath_variable_prefixes(&mut expr, node);
    apply_xpath_default_namespace(&mut expr, node);
    apply_static_base_uri(&mut expr, node);
    apply_default_collation(&mut expr, node);
    // Wrap the whole expression so XPath value comparisons (`eq`,
    // `ne`, `lt`, …) see the static default-collation at runtime.
    // Operator forms can't take a collation argument, so we have to
    // thread the URI via a runtime mechanism instead.
    if let Some(uri) = effective_default_collation(node) {
        if uri != "http://www.w3.org/2005/xpath-functions/collation/codepoint" {
            expr = Expr::WithDefaultCollation(uri, Box::new(expr));
        }
    }
    // XSLT 2.0 §3.8 — a `[xsl:]version="1.0"` scope evaluates XPath in
    // 1.0 backwards-compatibility mode (XPath 2.0 §B.1).  Wrap so the
    // runtime applies the 1.0 conversion rules (arithmetic → double,
    // range bounds take the first item) to this expression.
    if ancestor_forces_backwards_compat(node) {
        expr = Expr::BackwardsCompat(Box::new(expr));
    }
    Ok(expr)
}

/// XSLT 2.0 §3.6.1 / §16.3 — bake the in-scope `default-collation`
/// URI into each string-function call that accepts a trailing
/// collation argument but doesn't supply one.  Without this rewrite
/// the runtime would silently fall through to codepoint comparison
/// because compare(a, b) / contains(a, b) / etc never see the
/// surrounding xsl element's default-collation declaration.
fn apply_default_collation(expr: &mut Expr, node: &Node) {
    let Some(uri) = effective_default_collation(node) else { return; };
    if uri == "http://www.w3.org/2005/xpath-functions/collation/codepoint" {
        return;
    }
    fn walk(e: &mut Expr, uri: &str) {
        use sup_xml_core::xpath::ast::LocationPath;
        if let Expr::FunctionCall(name, args) = e {
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            let need_pad = match local {
                "compare"          if args.len() == 2 => true,
                "contains"         if args.len() == 2 => true,
                "starts-with"      if args.len() == 2 => true,
                "ends-with"        if args.len() == 2 => true,
                "substring-before" if args.len() == 2 => true,
                "substring-after"  if args.len() == 2 => true,
                "index-of"         if args.len() == 2 => true,
                "distinct-values"  if args.len() == 1 => true,
                "deep-equal"       if args.len() == 2 => true,
                _ => false,
            };
            if need_pad {
                args.push(Expr::Literal(uri.to_string()));
            }
        }
        match e {
            Expr::FunctionCall(_, args)
            | Expr::Sequence(args)              => for a in args { walk(a, uri); }
            Expr::Or(l, r) | Expr::And(l, r)
            | Expr::Eq(l, r) | Expr::Ne(l, r)
            | Expr::Lt(l, r) | Expr::Gt(l, r)
            | Expr::Le(l, r) | Expr::Ge(l, r)
            | Expr::ValueEq(l, r) | Expr::ValueNe(l, r)
            | Expr::ValueLt(l, r) | Expr::ValueGt(l, r)
            | Expr::ValueLe(l, r) | Expr::ValueGe(l, r)
            | Expr::Add(l, r) | Expr::Sub(l, r)
            | Expr::Mul(l, r) | Expr::Div(l, r)
            | Expr::Mod(l, r) | Expr::IDiv(l, r)
            | Expr::Union(l, r) | Expr::Intersect(l, r)
            | Expr::Except(l, r) | Expr::Range(l, r)
            | Expr::SimpleMap(l, r) | Expr::NodeBefore(l, r)
            | Expr::NodeAfter(l, r) | Expr::NodeIs(l, r) => { walk(l, uri); walk(r, uri); }
            Expr::Neg(x) | Expr::InstanceOf(x, _)
            | Expr::CastAs(x, _) | Expr::CastableAs(x, _)
            | Expr::TreatAs(x, _) => walk(x, uri),
            Expr::IfThenElse { cond, then_branch, else_branch } => {
                walk(cond, uri); walk(then_branch, uri); walk(else_branch, uri);
            }
            Expr::For { bindings, body } => {
                for (_, e) in bindings { walk(e, uri); }
                walk(body, uri);
            }
            Expr::Quantified { bindings, test, .. } => {
                for (_, e) in bindings { walk(e, uri); }
                walk(test, uri);
            }
            Expr::FilterPath { primary, predicates, steps } => {
                walk(primary, uri);
                for p in predicates { walk(p, uri); }
                for s in steps {
                    if let Some(f) = s.filter.as_mut() { walk(f, uri); }
                    for p in &mut s.predicates { walk(p, uri); }
                }
            }
            Expr::Path(LocationPath::Absolute(steps))
            | Expr::Path(LocationPath::Relative(steps)) => {
                for s in steps {
                    if let Some(f) = s.filter.as_mut() { walk(f, uri); }
                    for p in &mut s.predicates { walk(p, uri); }
                }
            }
            _ => {}
        }
    }
    walk(expr, &uri);
}

/// Walk an `xml:base`-aware ancestor chain from `node` upwards,
/// resolving each declared `xml:base` against the next outer one
/// per RFC 3986 (XPath 2.0 §3.1.5 / XML Base §3).  Returns the
/// effective static base URI for an XPath expression compiled at
/// `node`'s location, or `None` when no ancestor declared one.
fn effective_xml_base(node: &Node) -> Option<String> {
    // Collect the chain root-down so we can fold-resolve from the
    // outermost declaration toward the leaf.
    let mut bases: Vec<String> = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.is_element() {
            for a in n.attributes() {
                if a.local_name() == "base"
                   && a.namespace.get().and_then(|n| n.prefix()) == Some("xml") {
                    bases.push(a.value().to_string());
                    break;
                }
            }
        }
        cur = n.parent.get();
    }
    if bases.is_empty() { return None; }
    bases.reverse();
    let mut base = bases[0].clone();
    for b in &bases[1..] {
        base = sup_xml_core::xpath::eval::resolve_uri_against(&base, b);
    }
    Some(base)
}

/// Rewrite each `fn:static-base-uri()` and 1-arg `fn:resolve-uri(.)`
/// call in `expr` to use the static base URI computed from `node`'s
/// `xml:base` ancestor chain.  XPath 2.0 §15.5.7 specifies that the
/// 1-arg resolve-uri uses the static base; baking it in at compile
/// time lets each call site resolve against its own xml:base scope
/// without threading per-instruction static contexts through the
/// runtime.
fn apply_static_base_uri(expr: &mut Expr, node: &Node) {
    let Some(base) = effective_xml_base(node) else { return; };
    rewrite_base_uri_calls(expr, &base);
}

fn rewrite_base_uri_calls(expr: &mut Expr, base: &str) {
    use sup_xml_core::xpath::ast::LocationPath;
    match expr {
        Expr::FunctionCall(name, args) => {
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            // fn:static-base-uri() with no arguments — drop the call
            // entirely, replacing it with the literal base URI.
            if (name == "static-base-uri" || local == "static-base-uri" || name == "fn:static-base-uri")
                && args.is_empty()
            {
                *expr = Expr::Literal(base.to_string());
                return;
            }
            // fn:resolve-uri($x) — append the static base URI so the
            // 1-arg form becomes the 2-arg form with the call site's
            // base baked in.
            if (name == "resolve-uri" || local == "resolve-uri" || name == "fn:resolve-uri")
                && args.len() == 1
            {
                args.push(Expr::Literal(base.to_string()));
                // Fall through to recurse into the (newly appended)
                // arg in case it itself uses static-base-uri (rare).
            }
            for a in args { rewrite_base_uri_calls(a, base); }
        }
        Expr::Or(a, b) | Expr::And(a, b)
        | Expr::Eq(a, b) | Expr::Ne(a, b)
        | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
        | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
        | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
        | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
        | Expr::Add(a, b) | Expr::Sub(a, b)
        | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
        | Expr::Union(a, b)
        | Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b)
        | Expr::Range(a, b) | Expr::SimpleMap(a, b)
        | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => {
            rewrite_base_uri_calls(a, base);
            rewrite_base_uri_calls(b, base);
        }
        Expr::Neg(a)
        | Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
        | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => rewrite_base_uri_calls(a, base),
        Expr::Sequence(args) => {
            for a in args { rewrite_base_uri_calls(a, base); }
        }
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            rewrite_base_uri_calls(cond, base);
            rewrite_base_uri_calls(then_branch, base);
            rewrite_base_uri_calls(else_branch, base);
        }
        Expr::For { bindings, body } | Expr::Let { bindings, body } | Expr::Quantified { bindings, test: body, .. } => {
            for (_, e) in bindings { rewrite_base_uri_calls(e, base); }
            rewrite_base_uri_calls(body, base);
        }
        Expr::FilterPath { primary, predicates, steps } => {
            rewrite_base_uri_calls(primary, base);
            for p in predicates { rewrite_base_uri_calls(p, base); }
            for s in steps { for p in &mut s.predicates { rewrite_base_uri_calls(p, base); } }
        }
        Expr::Path(p) => match p {
            LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                for s in steps { for p in &mut s.predicates { rewrite_base_uri_calls(p, base); } }
            }
        },
        Expr::TryCatch { body, catches } => {
            rewrite_base_uri_calls(body, base);
            for c in catches { rewrite_base_uri_calls(&mut c.body, base); }
        }
        Expr::WithDefaultCollation(_, inner) => rewrite_base_uri_calls(inner, base),
        Expr::BackwardsCompat(inner) => rewrite_base_uri_calls(inner, base),
        Expr::MapConstructor(es) => for (k, v) in es {
            rewrite_base_uri_calls(k, base); rewrite_base_uri_calls(v, base);
        },
        Expr::ArrayConstructor { members, .. } =>
            for m in members { rewrite_base_uri_calls(m, base); },
        Expr::Lookup(b, key) => {
            rewrite_base_uri_calls(b, base);
            if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key {
                rewrite_base_uri_calls(e, base);
            }
        }
        Expr::UnaryLookup(key) =>
            if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key {
                rewrite_base_uri_calls(e, base);
            },
        Expr::InlineFunction { body, .. } => rewrite_base_uri_calls(body, base),
        Expr::DynamicCall { func, args } => {
            rewrite_base_uri_calls(func, base);
            for a in args { rewrite_base_uri_calls(a, base); }
        }
        Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => {}
        Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) | Expr::Variable(_) => {}
    }
}

/// Resolve the effective `xpath-default-namespace` for an XPath
/// expression compiled at `node`: walk the ancestor chain looking
/// Resolve the in-scope `[xsl:]default-collation` for `node` by
/// walking its ancestor chain.  The attribute lists one or more
/// collation URIs (whitespace-separated); the first recognised one
/// is the effective default per XSLT 2.0 §3.6.1.  Returns `None`
/// when no ancestor declares one or when none of the declared URIs
/// are implemented.
fn effective_default_collation(node: &Node) -> Option<String> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.is_element() {
            let v = if is_xslt_element(n) {
                read_attribute(n, "default-collation")
            } else {
                read_xsl_attribute(n, "default-collation")
            };
            if let Some(raw) = v {
                for tok in raw.split_whitespace() {
                    if is_recognised_collation(tok) {
                        return Some(tok.to_string());
                    }
                }
                return None;
            }
        }
        cur = n.parent.get();
    }
    None
}

/// for the first XSLT element with the attribute set (or any LRE
/// with `xsl:xpath-default-namespace`), and return the URI.  An
/// empty value (explicitly cleared by a child element) shadows any
/// outer declaration, so we record the first one encountered.
fn xpath_default_namespace_for(node: &Node) -> Option<String> {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.is_element() {
            let v = if is_xslt_element(n) {
                read_attribute(n, "xpath-default-namespace")
            } else {
                read_xsl_attribute(n, "xpath-default-namespace")
            };
            if let Some(uri) = v {
                let trimmed = uri.trim();
                return if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };
            }
        }
        cur = n.parent.get();
    }
    None
}

/// Walk a parsed XPath and rewrite each unprefixed element-axis
/// `NodeTest::LocalName` into `NodeTest::DefaultNamespaceName` when
/// `node`'s effective `xpath-default-namespace` is non-empty.  XSLT
/// 2.0 §5.1.1: this only applies to unprefixed element names — the
/// attribute and namespace axes keep the null-URI semantics, and
/// names that already carry an explicit prefix never change.
fn apply_xpath_default_namespace(expr: &mut Expr, node: &Node) {
    let Some(uri) = xpath_default_namespace_for(node) else { return; };
    use sup_xml_core::xpath::ast::{Axis, LocationPath, NodeTest, Step};
    fn rewrite_step(s: &mut Step, uri: &str) {
        let on_element_axis = matches!(s.axis,
            Axis::Child | Axis::Descendant | Axis::DescendantOrSelf
            | Axis::Self_ | Axis::Parent | Axis::Ancestor
            | Axis::AncestorOrSelf | Axis::FollowingSibling
            | Axis::PrecedingSibling | Axis::Following | Axis::Preceding);
        if on_element_axis {
            if let NodeTest::LocalName(local) = &s.node_test {
                s.node_test = NodeTest::DefaultNamespaceName {
                    uri: uri.to_string(),
                    local: local.clone(),
                };
            }
        }
        for p in &mut s.predicates { rewrite(p, uri); }
    }
    fn rewrite(e: &mut Expr, uri: &str) {
        match e {
            Expr::Or(a, b) | Expr::And(a, b)
            | Expr::Eq(a, b) | Expr::Ne(a, b)
            | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
            | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
            | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
            | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
            | Expr::Add(a, b) | Expr::Sub(a, b)
            | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
            | Expr::Union(a, b)
            | Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b)
            | Expr::Range(a, b) | Expr::SimpleMap(a, b)
            | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => {
                rewrite(a, uri); rewrite(b, uri);
            }
            Expr::Neg(a)
            | Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
            | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => rewrite(a, uri),
            Expr::FunctionCall(_, args) | Expr::Sequence(args) => {
                for a in args { rewrite(a, uri); }
            }
            Expr::IfThenElse { cond, then_branch, else_branch } => {
                rewrite(cond, uri); rewrite(then_branch, uri); rewrite(else_branch, uri);
            }
            Expr::For { bindings, body } | Expr::Let { bindings, body } | Expr::Quantified { bindings, test: body, .. } => {
                for (_, e) in bindings { rewrite(e, uri); }
                rewrite(body, uri);
            }
            Expr::FilterPath { primary, predicates, steps } => {
                rewrite(primary, uri);
                for p in predicates { rewrite(p, uri); }
                for s in steps { rewrite_step(s, uri); }
            }
            Expr::Path(p) => match p {
                LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                    for s in steps { rewrite_step(s, uri); }
                }
            },
            Expr::TryCatch { body, catches } => {
                rewrite(body, uri);
                for c in catches { rewrite(&mut c.body, uri); }
            }
            Expr::WithDefaultCollation(_, inner) => rewrite(inner, uri),
            Expr::BackwardsCompat(inner) => rewrite(inner, uri),
            Expr::MapConstructor(es) => for (k, v) in es { rewrite(k, uri); rewrite(v, uri); },
            Expr::ArrayConstructor { members, .. } => for m in members { rewrite(m, uri); },
            Expr::Lookup(b, key) => {
                rewrite(b, uri);
                if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key { rewrite(e, uri); }
            }
            Expr::UnaryLookup(key) =>
                if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key { rewrite(e, uri); },
            Expr::InlineFunction { body, .. } => rewrite(body, uri),
            Expr::DynamicCall { func, args } => {
                rewrite(func, uri);
                for a in args { rewrite(a, uri); }
            }
            Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => {}
            Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) | Expr::Variable(_) => {}
        }
    }
    rewrite(expr, &uri);
}

/// Walk an Expr tree, replacing each `Expr::Variable("prefix:local")`
/// reference with `Expr::Variable("{uri}local")` (Clark form),
/// resolving `prefix` through `node`'s ancestor xmlns declarations.
/// Unresolved prefixes pass through unchanged — the runtime's
/// `XsltBindings.variable` lookup will report the missing prefix.
fn resolve_xpath_variable_prefixes(expr: &mut Expr, node: &Node) {
    use sup_xml_core::xpath::ast::{Step, LocationPath};
    fn lookup(node: &Node, prefix: &str) -> Option<String> {
        let mut cur = Some(node);
        while let Some(n) = cur {
            for (p, uri) in n.ns_declarations() {
                if p == Some(prefix) { return Some(uri.to_string()); }
            }
            cur = n.parent.get();
        }
        None
    }
    fn walk(e: &mut Expr, node: &Node) {
        match e {
            Expr::Variable(name) => {
                if let Some((p, local)) = name.split_once(':') {
                    if let Some(uri) = lookup(node, p) {
                        *name = format!("{{{uri}}}{local}");
                    }
                }
            }
            Expr::Or(a, b) | Expr::And(a, b)
            | Expr::Eq(a, b) | Expr::Ne(a, b)
            | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
            | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
            | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
            | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
            | Expr::Add(a, b) | Expr::Sub(a, b)
            | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
            | Expr::Union(a, b)
            | Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b)
            | Expr::Range(a, b) | Expr::SimpleMap(a, b) | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => {
                walk(a, node); walk(b, node);
            }
            Expr::Neg(a)
            | Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
            | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => walk(a, node),
            Expr::FunctionCall(_, args)
            | Expr::Sequence(args) => {
                for a in args { walk(a, node); }
            }
            Expr::IfThenElse { cond, then_branch, else_branch } => {
                walk(cond, node); walk(then_branch, node); walk(else_branch, node);
            }
            Expr::For { bindings, body } | Expr::Let { bindings, body } | Expr::Quantified { bindings, test: body, .. } => {
                for (_, e) in bindings { walk(e, node); }
                walk(body, node);
            }
            Expr::FilterPath { primary, predicates, steps } => {
                walk(primary, node);
                for p in predicates { walk(p, node); }
                for s in steps { walk_step(s, node); }
            }
            Expr::Path(p) => match p {
                LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                    for s in steps { walk_step(s, node); }
                }
            },
            Expr::TryCatch { body, catches } => {
                walk(body, node);
                for c in catches { walk(&mut c.body, node); }
            }
            Expr::WithDefaultCollation(_, inner) => walk(inner, node),
            Expr::BackwardsCompat(inner) => walk(inner, node),
            Expr::MapConstructor(es) => for (k, v) in es { walk(k, node); walk(v, node); },
            Expr::ArrayConstructor { members, .. } => for m in members { walk(m, node); },
            Expr::Lookup(b, key) => {
                walk(b, node);
                if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key { walk(e, node); }
            }
            Expr::UnaryLookup(key) =>
                if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key { walk(e, node); },
            Expr::InlineFunction { body, .. } => walk(body, node),
            Expr::DynamicCall { func, args } => {
                walk(func, node);
                for a in args { walk(a, node); }
            }
            Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => {}
            Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => {}
        }
    }
    fn walk_step(s: &mut Step, node: &Node) {
        for p in &mut s.predicates { walk(p, node); }
    }
    walk(expr, node);
}

/// Wrapper around [`sup_xml_core::xpath::parse_xpath_with`] that
/// picks the right grammar (1.0 vs 2.0) from the active compile mode.
/// Every `parse_xpath(s)` call in this module routes through here.
fn parse_xpath(src: &str) -> sup_xml_core::error::Result<Expr> {
    let mut opts = XPathOptions::default();
    opts.xpath_2_0 = is_xpath_2_0_compile();
    parse_xpath_with(src, &opts)
}

/// `version="2.0"` (or higher) on `<xsl:stylesheet>` enables XSLT 2.0
/// features.  Anything below 2.0 — including the empty string and
/// 1.x values — stays in pure 1.0 mode.
pub(crate) fn version_enables_2_0(version: &str) -> bool {
    let v = version.trim();
    // Accept `2.0`, `2.1`, `3.0`, … — anything whose major version
    // parses as ≥ 2.  Doesn't strip-and-compare exact strings so we
    // tolerate future minor bumps.
    let major = v.split('.').next().and_then(|s| s.parse::<u32>().ok());
    matches!(major, Some(n) if n >= 2)
}

/// True iff `s` is a valid `xs:decimal` lexical form per XSD §3.3.3:
/// optional sign, then either a non-empty integer part, an optional
/// `.` plus a non-empty fractional part, or both.  No exponent,
/// whitespace, or alphabetics.
fn is_xs_decimal_lexical(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() { return false; }
    let body = t.strip_prefix(|c| c == '+' || c == '-').unwrap_or(t);
    let (whole, frac) = match body.split_once('.') {
        Some(pair) => pair,
        None       => (body, ""),
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() && frac.is_empty() { return false; }
    (whole.is_empty() || digits(whole)) && (frac.is_empty() || digits(frac))
}

/// True iff `version` (a stylesheet `version=` attribute value) is
/// strictly greater than `threshold` (e.g. `"2.0"`).  Used to gate
/// XSLT 2.0 §3.5 forwards-compatible processing — only when the
/// declared version exceeds what the processor implements do
/// unknown XSLT elements / functions get accepted silently.
fn version_is_greater_than(version: &str, threshold: &str) -> bool {
    fn parse(s: &str) -> Option<(u32, u32)> {
        let mut parts = s.trim().split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        Some((major, minor))
    }
    match (parse(version), parse(threshold)) {
        (Some(v), Some(t)) => v > t,
        _ => false,
    }
}

/// True iff any ancestor of `node` carries a `version` attribute (on
/// an XSLT-namespace element) or `xsl:version` attribute (on a
/// literal-result-element) that exceeds the processor's supported
/// XSLT version — XSLT 2.0 §3.5 lets such an attribute open a
/// forwards-compatible scope mid-tree, separate from the stylesheet-
/// root version that the global thread-local tracks.
fn ancestor_enables_forwards_compat(node: &Node) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.is_element() {
            let v = if is_xslt_element(n) {
                read_attribute(n, "version")
            } else {
                read_xsl_attribute(n, "version")
            };
            if let Some(v) = v {
                if version_is_greater_than(v, "2.0") { return true; }
            }
        }
        cur = n.parent.get();
    }
    false
}

/// Effective XSLT version at `node` is < 2.0 — backwards-compatible
/// processing (XSLT 2.0 §3.8).  True when the nearest ancestor-or-self
/// carrying a `version` / `xsl:version` attribute declares 1.x; that
/// attribute opens a 1.0-compatibility scope mid-tree (e.g.
/// `<out xsl:version="1.0">`), where `xsl:value-of` of a sequence
/// takes only the first item rather than space-joining.
fn ancestor_forces_backwards_compat(node: &Node) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.is_element() {
            let v = if is_xslt_element(n) {
                read_attribute(n, "version")
            } else {
                read_xsl_attribute(n, "version")
            };
            // The nearest version-bearing ancestor wins.
            if let Some(v) = v {
                return version_is_greater_than("2.0", v);
            }
        }
        cur = n.parent.get();
    }
    false
}

/// XSLT 3.0 §5.4.2 — is text-value-template expansion (`{…}` in text
/// nodes) in effect at `node`?  Controlled by the nearest in-scope
/// `[xsl:]expand-text` attribute; defaults to off.  `node` is a text
/// node, so the search starts at its parent element.
fn expand_text_in_scope(node: &Node) -> bool {
    let mut cur = node.parent.get();
    while let Some(n) = cur {
        if n.is_element() {
            let v = if is_xslt_element(n) {
                read_attribute(n, "expand-text")
            } else {
                read_xsl_attribute(n, "expand-text")
            };
            if let Some(v) = v {
                return matches!(v.trim(), "yes" | "true" | "1");
            }
        }
        cur = n.parent.get();
    }
    false
}

/// Public entry point — compile a parsed stylesheet document into
/// a [`StylesheetAst`].
pub fn compile(doc: &Document) -> Result<StylesheetAst, XsltError> {
    let root = doc.root();
    if !root.is_element() {
        return Err(XsltError::InvalidStylesheet(
            "stylesheet document has no root element".into(),
        ));
    }
    // XSLT 1.0 §2.3 simplified stylesheet: a non-XSLT root with an
    // `xsl:version` attribute is shorthand for an xsl:stylesheet
    // containing a single `xsl:template match="/"` whose body is the
    // root element.  We synthesise that AST directly.
    if !is_xslt_element(root) {
        if has_xsl_version_attr(root) {
            return compile_simplified(root);
        }
        return Err(XsltError::InvalidStylesheet(format!(
            "root element is not in the XSLT namespace (got '{}')",
            root.name(),
        )));
    }
    let local = root.local_name();
    // XSLT 3.0 §3.5 — `xsl:package` is a stylesheet-equivalent root.
    // We treat it as a self-contained stylesheet (its declarations are
    // compiled like any top-level declarations); cross-package linking
    // via xsl:use-package is not yet implemented.
    if local != "stylesheet" && local != "transform" && local != "package" {
        return Err(XsltError::InvalidStylesheet(format!(
            "root XSLT element must be xsl:stylesheet, xsl:transform or \
             xsl:package, got xsl:{local}"
        )));
    }

    let mut ast = StylesheetAst::default();
    // XSLT 1.0 §2.2 / XSLT 2.0 §3.6 — `version` is a required
    // attribute on `xsl:stylesheet` / `xsl:transform` (XTSE0010).
    ast.version = match read_attribute(root, "version") {
        Some(v) => v.to_string(),
        None    => return Err(XsltError::InvalidStylesheet(format!(
            "xsl:{local} requires a version= attribute (XTSE0010)"
        ))),
    };
    // XSLT 2.0 §3.6 — the version= value must be an xs:decimal.
    // Saxon-style scientific notation (`2.0e3`) or trailing
    // alphabetics are XTSE0110.
    if !is_xs_decimal_lexical(&ast.version) {
        return Err(XsltError::InvalidStylesheet(format!(
            "xsl:{local} version='{}' is not a valid xs:decimal (XTSE0110)",
            ast.version,
        )));
    }
    // XSLT 2.0 §3.6 — only a closed set of attributes is permitted
    // on `xsl:stylesheet` / `xsl:transform`.  Unprefixed attributes
    // outside the set are XTSE0090; foreign-namespace attributes
    // are allowed (extension data).  In forwards-compat mode the
    // mode guard installed below will short-circuit the check.
    const ALLOWED_ROOT_ATTRS: &[&str] = &[
        "version", "id", "xpath-default-namespace",
        "default-validation", "default-collation",
        "exclude-result-prefixes", "extension-element-prefixes",
        "use-when", "default-mode", "expand-text",
        "input-type-annotations",
    ];
    let in_forwards_compat = version_is_greater_than(&ast.version, "2.0");
    if !in_forwards_compat {
        for attr in root.attributes() {
            let name = attr.name();
            if attr.namespace.get().is_some() || name.starts_with("xmlns") || name.contains(':') { continue; }
            if !ALLOWED_ROOT_ATTRS.contains(&name) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:{local} has unrecognised attribute '{name}' (XTSE0090)"
                )));
            }
        }
        // XSLT 2.0 §3.6 / XTSE0125 — when default-collation= is
        // set on xsl:stylesheet, the URI must be one the
        // processor recognises (XTSE0125).  We currently
        // implement only the codepoint collation.
        if let Some(c) = read_attribute(root, "default-collation") {
            // XSLT 2.0 §3.6.1 — the attribute is a whitespace-
            // separated list of URIs; the first one the processor
            // recognises becomes the effective default.  XTSE0125
            // fires only if NONE are recognised.
            let any_known = c.split_whitespace()
                .any(is_recognised_collation);
            if !any_known {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:{local} default-collation='{c}' is not recognised (XTSE0125)"
                )));
            }
        }
        // XSLT 2.0 §11.1.2 / §7.1.1 — every prefix listed in
        // [xsl:]exclude-result-prefixes and
        // [xsl:]extension-element-prefixes must resolve through the
        // in-scope xmlns bindings (XTSE0808 / XTSE1430).  An
        // unrecognised prefix is a static error even when the
        // stylesheet would otherwise compile cleanly.  `#all` /
        // `#default` are the spec-reserved tokens.
        for attr_name in &["exclude-result-prefixes", "extension-element-prefixes"] {
            if let Some(raw) = read_attribute(root, attr_name) {
                for tok in raw.split_whitespace() {
                    if matches!(tok, "#all" | "#default") { continue; }
                    let mut found = false;
                    for (p, _) in root.ns_declarations() {
                        if p == Some(tok) { found = true; break; }
                    }
                    if !found {
                        let code = if *attr_name == "exclude-result-prefixes" { "XTSE0808" } else { "XTSE1430" };
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:{local} {attr_name}='{raw}' references undeclared \
                             prefix '{tok}' ({code})"
                        )));
                    }
                }
            }
        }
    }

    // `xml:base` on the stylesheet root supplies the static base URI
    // for XPath expressions whose evaluation depends on it
    // (XPath 2.0 §C.1) — primarily `fn:resolve-uri($rel)` /
    // `fn:static-base-uri()`.  Stored verbatim; lexical resolution
    // happens at the call site.  Match on the local name with an
    // explicit XML-namespace href check because the `c-abi` build
    // splits the prefix off, leaving `attr.name()` as just `"base"`.
    for attr in root.attributes() {
        let ns_href = attr.namespace.get().map(|n| n.href()).unwrap_or("");
        let local = attr.name().rsplit_once(':').map(|(_, l)| l).unwrap_or(attr.name());
        if local == "base" && ns_href == "http://www.w3.org/XML/1998/namespace" {
            ast.xml_base = Some(attr.value().to_string());
            break;
        }
    }
    // XSLT 2.0 §3.6 — record this module's input-type-annotations
    // value (if any).  The cross-module consistency check runs after
    // include / import merging in `compile_with_packages`.
    if let Some(v) = read_attribute(root, "input-type-annotations") {
        let v = v.trim();
        if !v.is_empty() {
            ast.input_type_annotations.push(v.to_string());
        }
    }

    // XSLT instruction set: 2.0 (xsl:function / xsl:sequence /
    // analyze-string / for-each-group …) is gated by the stylesheet
    // declaring version="2.0" or higher.
    //
    // XPath grammar: an XSLT 2.0 processor always accepts XPath 2.0
    // syntax even when the stylesheet declared version="1.0" — XSLT
    // 2.0 §3.5 puts the processor into backwards-compatible
    // *behaviour* mode where some semantics revert to 1.0 (the
    // value-of "first item only" rule, untyped → number coercion at
    // call boundaries), but the surface grammar still includes 2.0
    // constructs like `1 to 5`, `for $x in …`, `eq` / `ne`,
    // sequence parens, kind tests, etc.  Otherwise the W3C suite's
    // backwards-compat cases (1.0 stylesheet, 2.0-grammar XPath)
    // can't even compile.
    let xslt_2_0_on = version_enables_2_0(&ast.version);
    let in_fwd_compat = version_is_greater_than(&ast.version, "2.0");
    let _xslt_mode = XsltModeGuard::enter_full(xslt_2_0_on, true, in_fwd_compat);

    // Capture every xmlns declaration on the stylesheet root so
    // the runtime can resolve prefixes used inside XPath
    // expressions (match=, select=, test=, …).  Without this,
    // stylesheets that bind `iso:` / `xs:` / `sch:` on the root
    // and reference them inside match patterns get silently empty
    // matches at apply time.
    for (prefix, uri) in root.ns_declarations() {
        match prefix {
            Some(p) => { ast.namespaces.insert(p.to_string(), uri.to_string()); }
            // Store the default namespace under the empty-string key so
            // runtime lookups (`xsl:element`'s name expansion, the
            // XSLT "in-scope default namespace" rule) can find it
            // without a separate field.
            None    => { ast.namespaces.insert(String::new(), uri.to_string()); }
        }
    }
    // Also collect xmlns declarations from inner elements so XPath
    // expressions inside templates that bind a prefix locally
    // (e.g. `<xsl:template ... xmlns:fn="…">`) can resolve it.
    // Root-scoped declarations win over inner ones — we use
    // `entry().or_insert(...)` so the root binding sticks.  This is
    // a moderate approximation of XSLT 2.0's per-element in-scope
    // namespace rule; precise per-instruction resolution would need
    // tracking namespaces alongside each compiled XPath expression.
    collect_inner_ns(root, &mut ast.namespaces);

    // Source position counter for include-aware conflict resolution
    // (XSLT 1.0 §5.5).  Counts only element children so non-element
    // siblings (text, comments) don't shift the numbering.
    // XSLT 3.0 §3.5 — static parameters are evaluated first so they
    // are in scope for `use-when` and shadow attributes during the
    // main compilation pass.
    clear_static_params();
    for child in root.children() {
        if !child.is_element() || !is_xslt_element(child)
            || child.local_name() != "param" { continue; }
        let is_static = read_attribute(child, "static")
            .map(|v| matches!(v, "yes" | "true" | "1")).unwrap_or(false);
        if !is_static { continue; }
        let name = required_qname_attr(child, "name", "xsl:param")?;
        let value = match read_attribute(child, "select") {
            Some(sel) => {
                let e = parse_xpath_at(child, sel).map_err(XsltError::from)?;
                eval_static_expr_ast(&e, Some(child))?
            }
            None => sup_xml_core::xpath::eval::Value::String(String::new()),
        };
        set_static_param(qname_key(&name), value);
    }

    let mut pos: u32 = 0;
    for child in root.children() {
        // XSLT 2.0 §3.8 — xsl:stylesheet may only contain XSLT element
        // children, namespace nodes, and comments / PIs.  Non-whitespace
        // text content at top level is XTSE0120.
        if !child.is_element() {
            if is_text_like(child) && !child.content().trim().is_empty()
                && !in_forwards_compat_mode()
            {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:stylesheet may not contain text node children (XTSE0120)".into()
                ));
            }
            continue;
        }
        compile_top_level(child, &mut ast, pos)?;
        pos += 1;
    }

    // Harvest static-URI `document()` arguments now that the AST is
    // built — the runtime pre-loads these once per transformation
    // instead of resolving them inside an XPath evaluation step.
    ast.documents_to_load = crate::walk::collect_static_document_uris(&ast);

    // XSLT 1.0 §11.4 / 2.0 §9.5 / XTSE0630 — duplicate global
    // variable / param declarations at the same import precedence
    // are a static error.  Single-module compile means everything
    // here shares precedence; flag the first duplicate.
    {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for v in &ast.global_variables {
            let k = qname_key(&v.name);
            if !seen.insert(k.clone()) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate global xsl:variable '{k}' (XTSE0630)"
                )));
            }
        }
        for p in &ast.global_params {
            let k = qname_key(&p.name);
            if !seen.insert(k.clone()) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate global xsl:param '{k}' or collision with \
                     a same-named xsl:variable (XTSE0630)"
                )));
            }
        }
    }
    // XSLT 1.0 §11.4 / 2.0 §9.5 / XTDE0640 — global variable
    // declarations may not have cyclic dependencies.  Build the
    // ref graph from each global xsl:variable / xsl:param's select
    // expression and run a simple DFS cycle check.
    detect_global_variable_cycle(&ast)?;

    Ok(ast)
}

fn detect_global_variable_cycle(ast: &StylesheetAst) -> Result<(), XsltError> {
    use std::collections::{HashMap, HashSet};
    // Map: variable name → list of variable names referenced by its
    // defining expression.  Keys are Clark form per [`qname_key`].
    // Params count as variables for cycle purposes.
    let mut deps: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_names: HashSet<String> = HashSet::new();
    for v in &ast.global_variables { all_names.insert(qname_key(&v.name)); }
    for p in &ast.global_params    { all_names.insert(qname_key(&p.name)); }
    let collect_refs = |sel: &Option<sup_xml_core::xpath::ast::Expr>|
        -> Vec<String>
    {
        let Some(e) = sel else { return Vec::new(); };
        let mut refs = Vec::new();
        collect_variable_refs(e, &mut refs);
        refs.into_iter()
            .filter(|r| all_names.contains(r))
            .collect()
    };
    for v in &ast.global_variables {
        deps.insert(qname_key(&v.name), collect_refs(&v.select));
    }
    for p in &ast.global_params {
        deps.insert(qname_key(&p.name), collect_refs(&p.select));
    }
    // DFS with a recursion stack to detect a back-edge.
    fn dfs(
        name:    &str,
        deps:    &HashMap<String, Vec<String>>,
        seen:    &mut HashSet<String>,
        stack:   &mut HashSet<String>,
    ) -> Option<String> {
        if stack.contains(name) { return Some(name.to_string()); }
        if seen.contains(name)  { return None; }
        seen.insert(name.to_string());
        stack.insert(name.to_string());
        if let Some(refs) = deps.get(name) {
            for r in refs {
                if let Some(cyc) = dfs(r, deps, seen, stack) { return Some(cyc); }
            }
        }
        stack.remove(name);
        None
    }
    let mut seen = HashSet::new();
    for k in deps.keys() {
        let mut stack = HashSet::new();
        if let Some(cyc) = dfs(k, &deps, &mut seen, &mut stack) {
            return Err(XsltError::InvalidStylesheet(format!(
                "circular reference among global variables/params \
                 involving '{cyc}' (XTDE0640)"
            )));
        }
    }
    Ok(())
}

// ── top-level dispatch ───────────────────────────────────────────

/// XSLT 2.0 §3.10.2 — evaluate a `use-when` static expression.
///
/// The static evaluation context is intentionally minimal: no source
/// document, no variables, no user-defined functions; only the small
/// set of XPath functions that are well-defined without runtime data
/// (`system-property`, `element-available`, `function-available`,
/// `true`, `false`, `not`, plus literal comparisons).  Anything more
/// involved gracefully returns `true` so we don't accidentally drop
/// content the test would have exercised.
/// Evaluate a `use-when` expression at the host element so the
/// XPath's prefixes resolve through the static xmlns context
/// XPath's prefixes resolve through the static xmlns context
/// at the call site (XSLT 2.0 §3.10.2).  Without a node, only
/// the well-known xsl/xs/fn prefixes are bound.
fn evaluate_use_when_at(
    expr_text: &str, host: Option<&Node>,
) -> Result<bool, XsltError> {
    use sup_xml_core::xpath::eval::{eval_expr, EvalCtx, StaticContext, Value, XPathBindings};
    let mut expr = match sup_xml_core::xpath::parse_xpath_with(
        expr_text,
        &sup_xml_core::xpath::XPathOptions {
            xpath_2_0: true, libxml2_compatible: false,
            ..sup_xml_core::xpath::XPathOptions::default()
        },
    ) {
        Ok(e) => e,
        // XSLT 2.0 §3.10.2 — a use-when expression that doesn't even
        // parse is XPST0003.  We previously treated a parse failure as
        // "use the element" to be lenient, but the W3C suite expects
        // the static error to surface.
        Err(e) => return Err(XsltError::InvalidStylesheet(format!(
            "use-when='{expr_text}' failed to parse (XPST0003): {e}"
        ))),
    };
    // XSLT 2.0 §3.10.2 — fn:static-base-uri() inside a use-when
    // resolves to the *module's* base URI.  Substitute it now so the
    // dynamic-context-less evaluator never has to ask.
    if let Some(base) = MODULE_BASE_URI.with(|b| b.borrow().clone()) {
        rewrite_base_uri_calls(&mut expr, &base);
    }
    // XSLT 2.0 §3.10.2 — context-item / -position / -size / focus
    // access is a dynamic error inside a use-when.  Detect the
    // common forms statically so the error fires at compile rather
    // than silently defaulting to true.
    if use_when_uses_focus(&expr) {
        return Err(XsltError::InvalidStylesheet(format!(
            "use-when='{expr_text}' accesses the context item / focus, \
             which is undefined in the static context (XPDY0002)"
        )));
    }
    let mut ns_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(n) = host {
        let mut cur = Some(n);
        while let Some(n) = cur {
            for (p, uri) in n.ns_declarations() {
                if let Some(p) = p {
                    ns_map.entry(p.to_string()).or_insert_with(|| uri.to_string());
                }
            }
            cur = n.parent.get();
        }
    }
    let bindings = UseWhenBindings { extra_ns: ns_map };
    let idx = StaticEmptyIndex;
    let static_ctx = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        xpath_3_0: false,
        libxml2_compatible: false, current_node: None,
    };
    let ctx = EvalCtx {
        context_node: 0, pos: 1, size: 1,
        bindings: &bindings, static_ctx: &static_ctx,
    };
    match eval_expr(&expr, &ctx, &idx) {
        Ok(Value::Boolean(b)) => Ok(b),
        Ok(Value::Number(n))  => Ok(n.as_f64() != 0.0 && !n.as_f64().is_nan()),
        Ok(Value::String(s))  => Ok(!s.is_empty()),
        Ok(Value::NodeSet(ns)) => Ok(!ns.is_empty()),
        Ok(_) => Ok(true),
        Err(e) => Err(XsltError::InvalidStylesheet(format!(
            "use-when='{expr_text}': {}", e.message,
        ))),
    }
}

/// Evaluate a compiled XPath expression in the static context (no
/// source document, static parameters in scope) — used by static
/// parameters (XSLT 3.0 §3.5) and shadow attributes (§3.9).
fn eval_static_expr_ast(expr: &Expr, host: Option<&Node>) -> Result<sup_xml_core::xpath::eval::Value, XsltError> {
    use sup_xml_core::xpath::eval::{eval_expr, EvalCtx, StaticContext, XPathBindings};
    let mut ns_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(n) = host {
        let mut cur = Some(n);
        while let Some(n) = cur {
            for (p, uri) in n.ns_declarations() {
                if let Some(p) = p {
                    ns_map.entry(p.to_string()).or_insert_with(|| uri.to_string());
                }
            }
            cur = n.parent.get();
        }
    }
    let bindings = UseWhenBindings { extra_ns: ns_map };
    let idx = StaticEmptyIndex;
    let static_ctx = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        xpath_3_0: false,
        libxml2_compatible: false, current_node: None,
    };
    let ctx = EvalCtx {
        context_node: 0, pos: 1, size: 1,
        bindings: &bindings, static_ctx: &static_ctx,
    };
    eval_expr(expr, &ctx, &idx).map_err(|e| XsltError::InvalidStylesheet(format!(
        "static expression: {}", e.message)))
}

/// Resolve a shadow attribute (XSLT 3.0 §3.9): `_name="{avt}"` supplies
/// the value of attribute `name` via a value template evaluated in the
/// static context.  Returns the resolved string, or `None` when no
/// shadow attribute `_name` is present.
fn resolve_shadow_attr(node: &Node, name: &str) -> Result<Option<String>, XsltError> {
    use sup_xml_core::xpath::eval::value_to_string;
    let shadow = format!("_{name}");
    let Some(raw) = read_attribute(node, &shadow) else { return Ok(None) };
    let template = avt(node, raw)?;
    let mut out = String::new();
    for part in &template.parts {
        match part {
            crate::ast::AvtPart::Literal(s) => out.push_str(s),
            crate::ast::AvtPart::Expr(e) => {
                let v = eval_static_expr_ast(e, Some(node))?;
                out.push_str(&value_to_string(&v, &StaticEmptyIndex));
            }
        }
    }
    Ok(Some(out))
}

/// Read attribute `name`, honouring a `_name` shadow attribute when the
/// plain form is absent (XSLT 3.0 §3.9).  Returns an owned value since
/// the shadow form is computed.
fn read_attr_with_shadow(node: &Node, name: &str) -> Result<Option<String>, XsltError> {
    if let Some(v) = read_attribute(node, name) {
        return Ok(Some(v.to_string()));
    }
    resolve_shadow_attr(node, name)
}

/// True if `expr` references the dynamic-context focus (context item,
/// position, size) in a form that's well-defined only when evaluated
/// against a source document.  XSLT 2.0 §3.10.2: such access from a
/// use-when expression is XPDY0002.
fn use_when_uses_focus(expr: &Expr) -> bool {
    use sup_xml_core::xpath::ast::{LocationPath, Step};
    fn step_uses_focus(s: &Step) -> bool {
        // Any axis step is rooted at the context item unless it sits
        // inside a FilterPath whose primary supplies a different
        // starting set — the FilterPath case is handled above.
        if !s.predicates.is_empty() {
            for p in &s.predicates { if walk(p) { return true; } }
        }
        true
    }
    fn walk(e: &Expr) -> bool {
        match e {
            Expr::Path(LocationPath::Relative(steps)) => steps.iter().any(step_uses_focus),
            Expr::Path(LocationPath::Absolute(_))    => false,
            Expr::FilterPath { primary, predicates, steps } => {
                if walk(primary) { return true; }
                for p in predicates { if walk(p) { return true; } }
                for s in steps {
                    for p in &s.predicates { if walk(p) { return true; } }
                }
                false
            }
            Expr::FunctionCall(name, args) => {
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
                // XSLT 2.0 §3.10.2 — these functions all depend on
                // the dynamic context (focus, document graph, runtime
                // settings) and are forbidden inside a use-when.
                // XSLT 2.0 §3.10.2 — these depend on the dynamic
                // focus or runtime context.  doc() / unparsed-text() /
                // doc-available() / similar may evaluate but the spec
                // is explicit that they always answer the empty
                // sequence (or false) so the engine's use-when
                // evaluator handles them out-of-band rather than
                // raising here.
                if matches!(local,
                    "position" | "last" | "current" | "current-group"
                    | "current-grouping-key" | "regex-group"
                    | "generate-id" | "id" | "idref"
                    | "key" | "format-number" | "format-date"
                    | "format-time" | "format-dateTime"
                    | "base-uri" | "in-scope-prefixes"
                    | "namespace-uri-for-prefix"
                    // XSLT 2.0 §3.10.2 — these load external resources
                    // (or invoke the runtime loader) that aren't
                    // available during static evaluation; the W3C
                    // suite treats them as errors rather than
                    // silently returning empty.
                    | "doc" | "document" | "unparsed-text"
                    | "unparsed-text-available"
                ) {
                    return true;
                }
                args.iter().any(walk)
            }
            Expr::Or(a, b) | Expr::And(a, b)
            | Expr::Eq(a, b) | Expr::Ne(a, b)
            | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
            | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
            | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
            | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
            | Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b)
            | Expr::Div(a, b) | Expr::Mod(a, b)
            | Expr::Union(a, b) | Expr::Range(a, b)
            | Expr::SimpleMap(a, b)
            | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b)
                => walk(a) || walk(b),
            Expr::Neg(a) => walk(a),
            Expr::IfThenElse { cond, then_branch, else_branch } =>
                walk(cond) || walk(then_branch) || walk(else_branch),
            Expr::For { bindings, body } => {
                bindings.iter().any(|(_, e)| walk(e)) || walk(body)
            }
            Expr::Quantified { bindings, test, .. } => {
                bindings.iter().any(|(_, e)| walk(e)) || walk(test)
            }
            Expr::Sequence(items) => items.iter().any(walk),
            Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b) => walk(a) || walk(b),
            Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
            | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => walk(a),
            Expr::TryCatch { body, catches } => {
                if walk(body) { return true; }
                catches.iter().any(|c| walk(&c.body))
            }
            _ => false,
        }
    }
    walk(expr)
}

/// Stub [`DocIndexLike`] for `use-when` static evaluation — no
/// nodes, no document.  Calls into accessors that need a node
/// return empty strings; the only XPath calls that make sense in a
/// static context (`system-property`, `function-available`,
/// numeric / boolean / string ops) never reach the index methods
/// in the first place.
struct StaticEmptyIndex;
impl sup_xml_core::xpath::DocIndexLike for StaticEmptyIndex {
    fn parent(&self, _: sup_xml_core::xpath::NodeId) -> Option<sup_xml_core::xpath::NodeId> { None }
    fn children(&self, _: sup_xml_core::xpath::NodeId) -> &[sup_xml_core::xpath::NodeId] { &[] }
    fn attr_range(&self, _: sup_xml_core::xpath::NodeId) -> std::ops::Range<sup_xml_core::xpath::NodeId> { 0..0 }
    fn kind(&self, _: sup_xml_core::xpath::NodeId) -> sup_xml_core::xpath::XPathNodeKind {
        sup_xml_core::xpath::XPathNodeKind::Document
    }
    fn pi_target(&self, _: sup_xml_core::xpath::NodeId) -> &str { "" }
    fn string_value(&self, _: sup_xml_core::xpath::NodeId) -> String { String::new() }
    fn node_name(&self, _: sup_xml_core::xpath::NodeId) -> &str { "" }
    fn local_name(&self, _: sup_xml_core::xpath::NodeId) -> &str { "" }
    fn namespace_uri(&self, _: sup_xml_core::xpath::NodeId) -> &str { "" }
}

/// Restricted [`XPathBindings`] for `use-when` evaluation.  Resolves
/// the `xsl` prefix to the XSLT namespace so calls like
/// `system-property('xsl:version')` work; everything else falls
/// through to the no-binding default.
struct UseWhenBindings {
    /// Per-call additional xmlns bindings — collected by
    /// `evaluate_use_when_at` from the host element's in-scope
    /// xmlns declarations so a use-when expression can reference
    /// any prefix declared on or above its host.
    extra_ns: std::collections::HashMap<String, String>,
}
impl sup_xml_core::xpath::eval::XPathBindings for UseWhenBindings {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        if let Some(u) = self.extra_ns.get(prefix) { return Some(u.clone()); }
        match prefix {
            "xsl" => Some("http://www.w3.org/1999/XSL/Transform".into()),
            "xs"  => Some("http://www.w3.org/2001/XMLSchema".into()),
            "fn"  => Some("http://www.w3.org/2005/xpath-functions".into()),
            _ => None,
        }
    }
    fn variable(&self, name: &str) -> Option<sup_xml_core::xpath::eval::Value> {
        // `use-when` and shadow attributes may reference static
        // parameters (XSLT 3.0 §3.5).  The lexical reference is
        // unprefixed in the common case; try it directly, then the
        // local part of a prefixed reference.
        get_static_param(name)
            .or_else(|| name.rsplit(':').next().and_then(get_static_param))
    }
    fn foreign_string_value(
        &self, _: sup_xml_core::xpath::eval::ForeignNodePtr,
    ) -> String { String::new() }
    fn call_function(
        &self, _ns_uri: &str, name: &str,
        args: Vec<sup_xml_core::xpath::eval::Value>,
    ) -> Option<std::result::Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError>> {
        // XSLT 2.0 §3.10.2 — the static `use-when` context exposes
        // `system-property`, `element-available`, `function-available`,
        // and `type-available`.  We give them just enough to answer
        // sensible boolean/string results without touching the
        // runtime XSLT engine.
        use sup_xml_core::xpath::eval::{Value, value_to_string};
        let dummy = StaticEmptyIndex;
        match name {
            "system-property" if args.len() == 1 => {
                let raw = value_to_string(&args[0], &dummy);
                let (prefix, local) = match raw.split_once(':') {
                    Some((p, l)) => (Some(p), l),
                    None         => (None, raw.as_str()),
                };
                // Any prefix that resolves to the XSLT URI counts —
                // stylesheets commonly bind it as `xsl` or `xslt`,
                // but the W3C suite also uses `t`, `xslt2`, etc.
                let xsl = match prefix {
                    Some(p) => self.resolve_prefix(p).as_deref()
                        == Some("http://www.w3.org/1999/XSL/Transform"),
                    None => false,
                };
                if !xsl { return Some(Ok(Value::String(String::new()))); }
                let s = match local {
                    "version"                            => "2.0",
                    "vendor"                             => "sup-xml",
                    "vendor-url"                         => "https://github.com/super_source/sup_xml",
                    "product-name"                       => "sup-xml",
                    "product-version"                    => env!("CARGO_PKG_VERSION"),
                    "is-schema-aware"                    => "no",
                    "supports-serialization"             => "yes",
                    "supports-backwards-compatibility"   => "yes",
                    "supports-namespace-axis"            => "yes",
                    "supports-streaming"                 => "no",
                    "supports-dynamic-evaluation"        => "no",
                    "supports-higher-order-functions"    => "no",
                    "xpath-version"                      => "2.0",
                    "xsd-version"                        => "1.0",
                    _ => "",
                };
                Some(Ok(Value::String(s.to_string())))
            }
            // XSLT 2.0 §3.10.2 — `function-available` and
            // `element-available` are part of the static `use-when`
            // context.  Without an XSLT engine binding we answer
            // best-effort: built-in XSLT/XPath function and
            // instruction names are recognised; everything else
            // (extension functions / instructions, unregistered
            // namespaces) answers `false()`, which is exactly the
            // signal `use-when` patterns need to gate forwards-
            // compatibility fall-backs (`use-when="not(function-
            // available('saxon:assign'))"` etc).
            // XSLT 2.0 §3.10.2 — fn:doc-available always answers
            // `false` during static use-when evaluation; the runtime
            // loader isn't available here.  fn:doc is handled by the
            // focus-rejection path above (the spec deems it an error).
            "doc-available" => Some(Ok(Value::Boolean(false))),
            "function-available" if (1..=2).contains(&args.len()) => {
                let name = value_to_string(&args[0], &dummy);
                // XSLT 2.0 §16.5 / XTDE1400 — the supplied name must
                // be a valid lexical QName even when called from a
                // static use-when context.
                if !crate::functions::is_lexical_qname(&name) {
                    return Some(Err(sup_xml_core::xpath::eval::xpath_err(
                        format!("function-available(): '{name}' is not a valid \
                                 QName (XTDE1400)")
                    )));
                }
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name);
                let prefix = name.rsplit_once(':').map(|(p, _)| p);
                // XSLT 2.0 §16.5 / XTDE1400 — a prefix in the
                // argument must resolve to an in-scope namespace.
                // The static use-when context only sees the
                // host-element's xmlns bindings plus the well-known
                // `xs:` / `fn:` / `xsl:` reservations.
                if let Some(p) = prefix {
                    let known = matches!(p, "xs" | "fn" | "xsl" | "xslt" | "xsd")
                        || self.extra_ns.contains_key(p);
                    if !known {
                        return Some(Err(sup_xml_core::xpath::eval::xpath_err(
                            format!("function-available(): prefix '{p}' is not \
                                     bound in scope (XTDE1400)")
                        )));
                    }
                }
                // Unprefixed names: check built-in XPath/XSLT list.
                let unprefixed_builtin = prefix.is_none()
                    && crate::functions::is_builtin_function(local);
                // `fn:NAME` / `xs:NAME` — XPath functions namespace
                // or XSD constructor.  We recognise the XPath ones
                // by name and any XSD type with a static atomic kind.
                let xs_constructor = matches!(prefix, Some("xs"))
                    && sup_xml_core::xpath::eval::atomic_kind_static(local).is_some();
                let fn_builtin = matches!(prefix, Some("fn"))
                    && crate::functions::is_builtin_function(local);
                Some(Ok(Value::Boolean(
                    unprefixed_builtin || xs_constructor || fn_builtin
                )))
            }
            "element-available" if args.len() == 1 => {
                let name = value_to_string(&args[0], &dummy);
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name);
                let prefix = name.rsplit_once(':').map(|(p, _)| p);
                // The argument is in the XSLT namespace if its
                // prefix resolves to the XSLT URI (or there's no
                // prefix at all).  Compare against the URI rather
                // than literal `xsl:` so author-chosen aliases like
                // `xmlns:t="http://www.w3.org/1999/XSL/Transform"`
                // are recognised too.
                let xslt_uri = "http://www.w3.org/1999/XSL/Transform";
                let in_xslt = match prefix {
                    None    => true,
                    Some(p) => self.extra_ns.get(p)
                        .map(|u| u == xslt_uri)
                        .unwrap_or(false),
                };
                Some(Ok(Value::Boolean(
                    in_xslt && crate::functions::is_builtin_xslt_instruction(local)
                )))
            }
            // `type-available` — the static use-when context's
            // accessor for whether a type is known to the processor.
            // Basic 2.0 processor knows xs: built-in types only.
            "type-available" if args.len() == 1 => {
                let name = value_to_string(&args[0], &dummy);
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name);
                let prefix = name.rsplit_once(':').map(|(p, _)| p);
                let xs_type = matches!(prefix, Some("xs") | Some("xsd"))
                    && sup_xml_core::xpath::eval::atomic_kind_static(local).is_some();
                Some(Ok(Value::Boolean(xs_type)))
            }
            _ => None,
        }
    }
}

fn compile_top_level(node: &Node, ast: &mut StylesheetAst, pos: u32) -> Result<(), XsltError> {
    // XSLT 2.0 §3.10.2 / §3.6 — when forwards-compatible processing
    // is in effect and the top-level element is an XSLT-namespace
    // name we don't recognise, the element is treated as absent
    // outright.  Any errors in its attributes (including a use-when
    // that references an unknown function) are suppressed because
    // the element wouldn't have contributed anything regardless.
    let forwards_compat = version_is_greater_than(ast.version.as_str(), "2.0");
    let is_unknown_xslt = is_xslt_element(node)
        && !KNOWN_TOP_LEVEL_XSLT_ELEMENTS.contains(&node.local_name());
    if forwards_compat && is_unknown_xslt {
        return Ok(());
    }
    // XSLT 2.0 §3.10.2 `use-when` — static conditional compilation.
    // Evaluate the attribute as an XPath in a restricted static
    // context; if the boolean result is false, the element (and its
    // subtree) is treated as absent.
    if let Some(uw) = read_attribute(node, "use-when") {
        if !evaluate_use_when_at(uw, Some(node))? {
            return Ok(());
        }
    }
    if !is_xslt_element(node) {
        // XSLT 2.0 §3.8 — a top-level child of xsl:stylesheet that
        // isn't an XSLT element must be in a non-null namespace.
        // An unprefixed element here (e.g. `<porridge/>`) is
        // XTSE0130.  Foreign-namespace extension data is permitted
        // (and silently ignored by the engine).
        let ns_uri = node.namespace.get().map(|ns| ns.href()).unwrap_or("");
        if ns_uri.is_empty() && !in_forwards_compat_mode() {
            return Err(XsltError::InvalidStylesheet(format!(
                "top-level element <{}> has no namespace URI (XTSE0130)",
                node.name(),
            )));
        }
        return Ok(());
    }
    match node.local_name() {
        "template"        => {
            let mut t = compile_template(node)?;
            t.source_path = vec![pos];
            ast.templates.push(t);
        }
        "variable"        => ast.global_variables.push(compile_variable(node)?),
        "param"           => {
            let p = compile_param(node)?;
            // XSLT 2.0 §9 — top-level (stylesheet) parameters can't
            // carry tunnel="yes"; tunnel parameters only make sense
            // inside template / function bodies.  XTSE0020.
            if p.tunnel {
                return Err(XsltError::InvalidStylesheet(
                    "top-level xsl:param cannot specify tunnel='yes' (XTSE0020)".into()
                ));
            }
            ast.global_params.push(p);
        }
        "key"             => {
            let k = compile_key(node)?;
            // XSLT 2.0 §16.3 / XTSE1220 — same-name xsl:key
            // declarations must share an effective collation.
            // Compare against earlier declarations of the same
            // name; the codepoint URI and an absent collation are
            // equivalent (both denote the default).
            let new_coll = k.collation.as_deref()
                .filter(|s| !s.is_empty()
                    && *s != "http://www.w3.org/2005/xpath-functions/collation/codepoint");
            for prior in ast.keys.iter()
                .filter(|p| qname_key(&p.name) == qname_key(&k.name))
            {
                let prior_coll = prior.collation.as_deref()
                    .filter(|s| !s.is_empty()
                        && *s != "http://www.w3.org/2005/xpath-functions/collation/codepoint");
                if prior_coll != new_coll {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:key '{}' declared with conflicting effective \
                         collations (XTSE1220)", qname_key(&k.name)
                    )));
                }
            }
            ast.keys.push(k);
        }
        "attribute-set"   => ast.attribute_sets.push(compile_attribute_set(node)?),
        "mode"            => ast.modes.push(compile_mode(node)?),
        "accumulator"     => ast.accumulators.push(compile_accumulator(node)?),
        // XSLT 2.0 §3.13 — record the imported schema for deferred
        // loading; the loader isn't available here, so resolution happens
        // once the module tree is assembled.  Used by `castable`/`cast`/
        // `instance of` for user-defined types (schema-aware processing).
        "import-schema"   => {
            let namespace = read_attribute(node, "namespace").map(|s| s.to_string());
            if let Some(loc) = read_attribute(node, "schema-location") {
                ast.schema_imports.push((namespace, loc.to_string()));
            }
        }
        "use-package"     => ast.use_packages.push(compile_use_package(node)?),
        "output"          => ast.outputs.push(compile_output(node)?),
        "strip-space"     => collect_whitespace_rules(node, true,  &mut ast.whitespace_rules)?,
        "preserve-space"  => collect_whitespace_rules(node, false, &mut ast.whitespace_rules)?,
        "include"  => {
            validate_xslt_only_attributes(node, "xsl:include", &["href"])?;
            validate_must_be_empty(node, "xsl:include")?;
            let href = read_attribute(node, "href").ok_or_else(||
                XsltError::InvalidStylesheet(
                    "xsl:include requires an href= attribute (XTSE0010)".into()))?;
            ast.includes.push(href.to_string());
            ast.include_positions.push(pos);
        }
        "import"   => {
            validate_xslt_only_attributes(node, "xsl:import", &["href"])?;
            validate_must_be_empty(node, "xsl:import")?;
            let href = read_attribute(node, "href").ok_or_else(||
                XsltError::InvalidStylesheet(
                    "xsl:import requires an href= attribute (XTSE0010)".into()))?;
            ast.imports.push(href.to_string());
        }
        "namespace-alias" => {
            if let Some(pair) = compile_namespace_alias(node)? {
                // XSLT 2.0 §7.1.1 / XTSE0810 — within a single module,
                // two xsl:namespace-alias declarations with the same
                // source URI must agree on the target URI.  (Across
                // modules with differing import precedence the higher
                // precedence wins; that's handled in
                // [`compile_with_imports`].)
                let (style_uri, result_uri, _) = &pair;
                for (existing_style, existing_result, _) in &ast.namespace_aliases {
                    if existing_style == style_uri && existing_result != result_uri {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:namespace-alias source URI '{style_uri}' is \
                             aliased to both '{existing_result}' and \
                             '{result_uri}' at the same import precedence \
                             (XTSE0810)"
                        )));
                    }
                }
                ast.namespace_aliases.push(pair);
            }
        }
        "decimal-format" => compile_decimal_format(node, ast)?,
        // XSLT 2.0 `<xsl:function>` — only compiled when the active
        // mode allows it.  In 1.0 mode this falls through to the
        // "unknown top-level" arm and is silently ignored.
        "function" => {
            let f = compile_function(node)?;
            // XSLT 2.0 §10.3 / XTSE0770 — two stylesheet functions
            // may not share the same expanded name and arity.
            let arity = f.params.len();
            let key = qname_key(&f.name);
            if ast.functions.iter().any(|g|
                qname_key(&g.name) == key && g.params.len() == arity)
            {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate xsl:function '{key}#{arity}' (XTSE0770)"
                )));
            }
            ast.functions.push(f);
        }
        "character-map" => {
            ast.character_maps.push(compile_character_map(node)?);
        }
        // Known XSLT instructions only make sense inside templates
        // (sequence constructors).  Surface them at top level as
        // XTSE0010 — the spec's "wrong context" rule, exercised by
        // tests like error-0010c (apply-imports), error-0010au
        // (xsl:text), error-0010al (xsl:value-of), etc.
        name if INSTRUCTION_NAMES_TOP_LEVEL_FORBIDDEN.contains(&name) => {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:{name} is not allowed at top level (XTSE0010)"
            )));
        }
        // `xsl:stylesheet` / `xsl:transform` may only appear as the
        // root of a stylesheet, never as a nested top-level element
        // — unless the surrounding stylesheet is in forwards-
        // compatible mode (its declared version exceeds ours), in
        // which case we silently accept and ignore.
        "stylesheet" | "transform" => {
            let v = ast.version.as_str();
            if !version_is_greater_than(v, "2.0") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:{} may only appear as the stylesheet root \
                     (XTSE0010)", node.local_name()
                )));
            }
        }
        // Top-level XSLT element that's "known" (spec-defined in
        // 1.0, 2.0, or 3.0) but not active in the current mode —
        // silently accept and ignore.  Example: `xsl:function` in a
        // `version="1.0"` stylesheet (recognised, but only compiled
        // when 2.0 mode is on).
        other if KNOWN_TOP_LEVEL_XSLT_ELEMENTS.contains(&other) => {}
        // Unknown XSLT-namespace top-level element.  XSLT 2.0 §3.5 /
        // §6 say this is XTSE0010 when the stylesheet's declared
        // version is at or below the processor's supported version
        // (forwards-compatible processing kicks in only for *higher*
        // versions, where unknown 3.0+ elements are permissible).
        other => {
            let v = ast.version.as_str();
            let in_forwards_compat = version_is_greater_than(v, "2.0");
            if !in_forwards_compat {
                return Err(XsltError::InvalidStylesheet(format!(
                    "unknown XSLT top-level element xsl:{other} \
                     (XTSE0010 — declared version {v} is not in \
                     forwards-compatible mode)",
                )));
            }
        }
    }
    Ok(())
}

/// XSLT instruction names that may only appear inside a template
/// body — finding any of these at top level is XTSE0010.  This
/// list deliberately excludes elements that *do* appear at top
/// level (xsl:template, xsl:param, xsl:variable, xsl:key,
/// xsl:output, xsl:strip-space, xsl:preserve-space, xsl:include,
/// xsl:import, xsl:namespace-alias, xsl:decimal-format,
/// xsl:attribute-set, xsl:function, xsl:character-map).
const INSTRUCTION_NAMES_TOP_LEVEL_FORBIDDEN: &[&str] = &[
    "apply-imports", "apply-templates", "call-template", "choose",
    "if", "for-each", "value-of", "text", "copy", "copy-of",
    "element", "attribute", "comment", "processing-instruction",
    "number", "message", "fallback", "next-match",
    "analyze-string", "perform-sort", "document", "result-document",
    "sequence", "for-each-group", "namespace", "map", "map-entry",
];

/// XSLT-namespace top-level elements the spec defines (1.0 + 2.0 +
/// 3.0).  An XSLT element in this set used at top level is
/// recognised, even if the active compile mode doesn't compile it
/// (e.g. `xsl:function` in a 1.0 stylesheet — recognised, silently
/// ignored).  Names outside this set are XTSE0010 unless the
/// stylesheet is in forwards-compatible mode.
const KNOWN_TOP_LEVEL_XSLT_ELEMENTS: &[&str] = &[
    // XSLT 1.0
    "template", "variable", "param", "key", "attribute-set",
    "output", "strip-space", "preserve-space",
    "include", "import", "namespace-alias", "decimal-format",
    "stylesheet", "transform",
    // XSLT 2.0 / 3.0 additions.
    "function", "character-map", "import-schema",
    "result-document", "use-package", "package", "accumulator",
    "mode", "global-context-item", "merge-source", "merge-key",
    "merge-action", "context-item",
];

/// Compile an `<xsl:function name="my:foo">` declaration into a
/// [`UserFunction`].  Children are split into leading `<xsl:param>`
/// declarations (XSLT 2.0 §10.3 requires them first) and the body
/// sequence constructor.
fn compile_function(node: &Node) -> Result<UserFunction, XsltError> {
    // XSLT 2.0 §10.3 — closed attribute set.  XTSE0090 / XTSE0020 on
    // unrecognised attributes or invalid override= values.
    validate_xslt_only_attributes(node, "xsl:function",
        &["name", "as", "override", "visibility", "streamability", "cache", "new-each-time"])?;
    if let Some(v) = read_attribute(node, "override") {
        if !matches!(v.trim(), "yes" | "no") {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:function override='{v}' must be 'yes' or 'no' (XTSE0020)"
            )));
        }
    }
    let name = read_attribute(node, "name").ok_or_else(||
        XsltError::InvalidStylesheet("xsl:function requires a name= attribute".into())
    )?;
    let qname = parse_qname_on(node, name)?;
    if qname.uri.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:function name must be in a namespace (prefixed)".into()));
    }
    reject_reserved_name(&qname, "xsl:function")?;
    let mut params = Vec::new();
    let mut body   = Vec::new();
    let mut seen_non_param = false;
    for child in node.children() {
        if !child.is_element() { continue; }
        if is_xslt_element(&child) && child.local_name() == "param" && !seen_non_param {
            let p = compile_param(&child)?;
            // XSLT 2.0 §10.3 — function parameters cannot be tunnel
            // parameters (XTSE0020 in the W3C suite's bucket).
            if p.tunnel {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:function parameter cannot specify tunnel='yes' (XTSE0020)".into()
                ));
            }
            // XSLT 2.0 §10.3 / XTSE0760 — xsl:function parameters
            // may not declare a default value; they must be empty
            // and carry no select= attribute (callers must always
            // supply every argument).
            if p.select.is_some() || !p.body.is_empty() {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:function parameter '{}' cannot have a default \
                     value (XTSE0760)", qname_key(&p.name),
                )));
            }
            // XSLT 2.0 §10.3 — xsl:function parameters cannot carry
            // the required= attribute at all (every argument must be
            // supplied; the attribute is meaningless here).  XSLT 3.0
            // generalised required= to templates / functions and
            // lifted this restriction.
            if read_attribute(&child, "required").is_some() {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:function parameter '{}' cannot specify \
                     required= (XTSE0020)", qname_key(&p.name),
                )));
            }
            let key = qname_key(&p.name);
            if params.iter().any(|q: &Param| qname_key(&q.name) == key) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate xsl:param '{key}' on xsl:function (XTSE0580)"
                )));
            }
            params.push(p);
            continue;
        }
        seen_non_param = true;
        compile_instr_into(&child, &mut body)?;
    }
    // Try to lower the body into a single XPath expression so the
    // pure-XPath dispatch path can handle it.  Bodies that don't
    // reduce (literal elements, complex constructs) keep their
    // instruction-list form and surface the "complex body" error at
    // call time.  See [`desugar_body_to_xpath`] for the supported
    // patterns.
    // XSLT 2.0 §10.3 / XTTE0945 — xsl:function bodies have no
    // context item, so instructions that depend on the focus
    // (xsl:copy, xsl:apply-templates with no select, …) are
    // statically detectable type errors.  Walk the body looking
    // for the obvious ones; flag with the spec error.
    reject_function_body_focus_use(&body)?;
    if let Some(expr) = desugar_body_to_xpath(&body) {
        body = vec![Instr::Sequence { select: expr }];
    }
    let as_type = read_attribute(node, "as").map(str::to_string);
    let visibility = read_attribute(node, "visibility").map(str::to_string);
    Ok(UserFunction { name: qname, params, body, as_type, visibility })
}

/// Walk `body` for instructions that reference the dynamic focus —
/// xsl:copy (which copies the context item), xsl:apply-templates with
/// no select=, etc.  Returns XTTE0945 when one is found inside an
/// xsl:function body, where the focus is undefined per XSLT 2.0 §10.3.
/// Append every free `$variable` reference encountered while
/// walking `e` to `out` (Clark-form keys: prefixed names already
/// resolved at `parse_xpath_at` time become `{uri}local`).  Used by
/// global-variable cycle detection (XTDE0640).
fn collect_variable_refs(e: &sup_xml_core::xpath::ast::Expr, out: &mut Vec<String>) {
    use sup_xml_core::xpath::ast::{Expr, LocationPath};
    match e {
        Expr::Variable(name) => out.push(name.clone()),
        Expr::Or(l, r) | Expr::And(l, r)
        | Expr::Eq(l, r) | Expr::Ne(l, r)
        | Expr::Lt(l, r) | Expr::Gt(l, r) | Expr::Le(l, r) | Expr::Ge(l, r)
        | Expr::ValueEq(l, r) | Expr::ValueNe(l, r)
        | Expr::ValueLt(l, r) | Expr::ValueGt(l, r)
        | Expr::ValueLe(l, r) | Expr::ValueGe(l, r)
        | Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r)
        | Expr::Div(l, r) | Expr::Mod(l, r)
        | Expr::Union(l, r) | Expr::IDiv(l, r)
        | Expr::Intersect(l, r) | Expr::Except(l, r)
        | Expr::Range(l, r) | Expr::SimpleMap(l, r)
        | Expr::NodeBefore(l, r) | Expr::NodeAfter(l, r) | Expr::NodeIs(l, r) => {
            collect_variable_refs(l, out);
            collect_variable_refs(r, out);
        }
        Expr::Neg(x) | Expr::InstanceOf(x, _)
        | Expr::CastAs(x, _) | Expr::CastableAs(x, _)
        | Expr::TreatAs(x, _) => collect_variable_refs(x, out),
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            collect_variable_refs(cond, out);
            collect_variable_refs(then_branch, out);
            collect_variable_refs(else_branch, out);
        }
        Expr::For { bindings, body } | Expr::Let { bindings, body } | Expr::Quantified { bindings, test: body, .. } => {
            for (_, e) in bindings { collect_variable_refs(e, out); }
            collect_variable_refs(body, out);
        }
        Expr::FilterPath { primary, predicates, steps } => {
            collect_variable_refs(primary, out);
            for p in predicates { collect_variable_refs(p, out); }
            for s in steps {
                if let Some(f) = &s.filter { collect_variable_refs(f, out); }
                for p in &s.predicates { collect_variable_refs(p, out); }
            }
        }
        Expr::Path(p) => match p {
            LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                for s in steps {
                    if let Some(f) = &s.filter { collect_variable_refs(f, out); }
                    for p in &s.predicates { collect_variable_refs(p, out); }
                }
            }
        }
        Expr::FunctionCall(_, args) | Expr::Sequence(args) =>
            for a in args { collect_variable_refs(a, out); }
        Expr::TryCatch { body, catches } => {
            collect_variable_refs(body, out);
            for c in catches { collect_variable_refs(&c.body, out); }
        }
        Expr::WithDefaultCollation(_, inner) => collect_variable_refs(inner, out),
        Expr::BackwardsCompat(inner) => collect_variable_refs(inner, out),
        Expr::MapConstructor(es) => for (k, v) in es {
            collect_variable_refs(k, out); collect_variable_refs(v, out);
        },
        Expr::ArrayConstructor { members, .. } =>
            for m in members { collect_variable_refs(m, out); },
        Expr::Lookup(b, key) => {
            collect_variable_refs(b, out);
            if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key {
                collect_variable_refs(e, out);
            }
        }
        Expr::UnaryLookup(key) =>
            if let sup_xml_core::xpath::ast::LookupKey::Expr(e) = key {
                collect_variable_refs(e, out);
            },
        // An inline function's parameters are locally bound; references
        // to them inside the body are not dependencies of the enclosing
        // scope, so they are excluded from the collected set.
        Expr::InlineFunction { params, body, .. } => {
            let mut inner = Vec::new();
            collect_variable_refs(body, &mut inner);
            for v in inner {
                if !params.contains(&v) { out.push(v); }
            }
        }
        Expr::DynamicCall { func, args } => {
            collect_variable_refs(func, out);
            for a in args { collect_variable_refs(a, out); }
        }
        Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => {}
        Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => {}
    }
}

/// True iff `e` reads the OUTER focus — the context item the
/// expression was evaluated against, NOT a re-focused predicate
/// (`$var[.]`), simple-map (`seq ! .`), or `for $x in seq` body.
/// Used to gate XPDY0002 in xsl:function bodies: such bodies have
/// no outer focus, so any outer-focus reference is an error.  A
/// predicate's own `.` is fine — it refers to the iterated item,
/// not the function's missing context.
fn expr_uses_outer_focus(e: &sup_xml_core::xpath::ast::Expr) -> bool {
    use sup_xml_core::xpath::ast::{Expr, LocationPath};
    match e {
        // Relative path is rooted at the outer context item.
        Expr::Path(LocationPath::Relative(_)) => true,
        // Absolute path has no outer-context dependency.
        Expr::Path(LocationPath::Absolute(_)) => false,
        // FilterPath: primary is outer context.  Predicates and
        // steps re-focus, so don't recurse.
        Expr::FilterPath { primary, .. } => expr_uses_outer_focus(primary),
        // For / Quantified / SimpleMap re-focus their body to the
        // iteration item; only the input sequences are outer.
        Expr::For { bindings, .. }
        | Expr::Quantified { bindings, .. } => {
            bindings.iter().any(|(_, x)| expr_uses_outer_focus(x))
        }
        // `let` does not re-focus: both the bound expressions and the
        // body are evaluated against the outer focus.
        Expr::Let { bindings, body } =>
            bindings.iter().any(|(_, x)| expr_uses_outer_focus(x))
                || expr_uses_outer_focus(body),
        Expr::SimpleMap(l, _) => expr_uses_outer_focus(l),
        Expr::Or(l, r) | Expr::And(l, r)
        | Expr::Eq(l, r) | Expr::Ne(l, r)
        | Expr::Lt(l, r) | Expr::Gt(l, r) | Expr::Le(l, r) | Expr::Ge(l, r)
        | Expr::ValueEq(l, r) | Expr::ValueNe(l, r)
        | Expr::ValueLt(l, r) | Expr::ValueGt(l, r)
        | Expr::ValueLe(l, r) | Expr::ValueGe(l, r)
        | Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r)
        | Expr::Div(l, r) | Expr::Mod(l, r)
        | Expr::Union(l, r) | Expr::IDiv(l, r)
        | Expr::Intersect(l, r) | Expr::Except(l, r)
        | Expr::Range(l, r)
        | Expr::NodeBefore(l, r) | Expr::NodeAfter(l, r) | Expr::NodeIs(l, r) =>
            expr_uses_outer_focus(l) || expr_uses_outer_focus(r),
        Expr::Neg(x) | Expr::InstanceOf(x, _)
        | Expr::CastAs(x, _) | Expr::CastableAs(x, _)
        | Expr::TreatAs(x, _) => expr_uses_outer_focus(x),
        Expr::IfThenElse { cond, then_branch, else_branch } =>
            expr_uses_outer_focus(cond)
                || expr_uses_outer_focus(then_branch)
                || expr_uses_outer_focus(else_branch),
        Expr::FunctionCall(name, args) => {
            // Focus-sensitive 0-arg calls (position(), last(),
            // current(), name(), local-name(), namespace-uri(),
            // string(), normalize-space(), number(), node-name(),
            // base-uri()) read the outer focus.
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            let focus_no_arg = matches!(local,
                "position" | "last" | "current"
                | "name" | "local-name" | "namespace-uri"
                | "string" | "normalize-space" | "number"
                | "node-name" | "base-uri" | "string-length"
                | "has-children" | "root" | "data");
            if focus_no_arg && args.is_empty() { return true; }
            // `key(name, val)` (2-arg) implicitly uses the context
            // node to find the document — XTDE1270 in a context-
            // less call site.  Same applies to `id(refs)` (1-arg)
            // and `idref(refs)` (1-arg).
            let needs_ctx_doc = matches!(local, "key" | "id" | "idref");
            if needs_ctx_doc && (args.len() == 1 || args.len() == 2) { return true; }
            args.iter().any(expr_uses_outer_focus)
        }
        Expr::Sequence(items) => items.iter().any(expr_uses_outer_focus),
        Expr::TryCatch { body, catches } =>
            expr_uses_outer_focus(body)
                || catches.iter().any(|c| expr_uses_outer_focus(&c.body)),
        Expr::WithDefaultCollation(_, inner) => expr_uses_outer_focus(inner),
        Expr::BackwardsCompat(inner) => expr_uses_outer_focus(inner),
        Expr::MapConstructor(es) =>
            es.iter().any(|(k, v)| expr_uses_outer_focus(k) || expr_uses_outer_focus(v)),
        Expr::ArrayConstructor { members, .. } =>
            members.iter().any(expr_uses_outer_focus),
        Expr::Lookup(b, key) => expr_uses_outer_focus(b)
            || matches!(key, sup_xml_core::xpath::ast::LookupKey::Expr(e) if expr_uses_outer_focus(e)),
        // The context-item primary and a unary lookup both read the
        // context item — outer focus.
        Expr::ContextItem | Expr::UnaryLookup(_) => true,
        // An inline function establishes its own focus when called; its
        // body never reads the focus of the surrounding expression.
        Expr::InlineFunction { .. } => false,
        Expr::DynamicCall { func, args } =>
            expr_uses_outer_focus(func) || args.iter().any(expr_uses_outer_focus),
        Expr::NamedFunctionRef { .. } | Expr::Placeholder => false,
        Expr::Variable(_) | Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => false,
    }
}

fn reject_function_body_focus_use(body: &[Instr]) -> Result<(), XsltError> {
    use crate::ast::Instr;
    let reject_focus_expr = |e: &sup_xml_core::xpath::ast::Expr, who: &str|
        -> Result<(), XsltError>
    {
        if expr_uses_outer_focus(e) {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} inside xsl:function body references the context item, \
                 which is undefined for stylesheet functions (XPDY0002)"
            )));
        }
        Ok(())
    };
    for instr in body {
        match instr {
            Instr::Copy { .. } => return Err(XsltError::InvalidStylesheet(
                "xsl:copy inside xsl:function body is XTTE0945 — the body \
                 has no context item to copy".into()
            )),
            // Outer-context expressions on instructions that don't
            // change the focus — `.` / `position()` / a relative
            // path here implicitly reads the (undefined) function
            // context item.
            Instr::Sequence { select } =>
                reject_focus_expr(select, "xsl:sequence select=")?,
            Instr::ValueOf { select, .. } =>
                reject_focus_expr(select, "xsl:value-of select=")?,
            Instr::CopyOf { select, .. } =>
                reject_focus_expr(select, "xsl:copy-of select=")?,
            // Recurse into block-style instructions that DON'T set
            // a new focus.  xsl:for-each / xsl:for-each-group /
            // xsl:apply-templates / xsl:analyze-string DO change
            // the focus, so xsl:copy inside them is valid even
            // from a function body — leave their bodies alone.
            Instr::If { test, body } => {
                reject_focus_expr(test, "xsl:if test=")?;
                reject_function_body_focus_use(body)?;
            }
            Instr::Choose { whens, otherwise } => {
                for (test, b) in whens {
                    reject_focus_expr(test, "xsl:when test=")?;
                    reject_function_body_focus_use(b)?;
                }
                if let Some(b) = otherwise { reject_function_body_focus_use(b)?; }
            }
            Instr::Variable(v) => {
                if let Some(sel) = &v.select {
                    reject_focus_expr(sel, "xsl:variable select=")?;
                }
                reject_function_body_focus_use(&v.body)?;
            }
            Instr::Document { body }
            | Instr::Fallback { body }
            | Instr::Message { body, .. } => reject_function_body_focus_use(body)?,
            _ => {}
        }
    }
    Ok(())
}

/// Lower an `xsl:function` body's instruction list into a single
/// XPath 2.0 expression where possible.  Returns `None` when the
/// body contains constructs that don't have a clean XPath equivalent
/// (literal element constructors, `xsl:copy`, etc.); the caller
/// keeps the instruction form in that case.
///
/// Supported lowerings:
///
/// * `<xsl:sequence select="E"/>` → `E`
/// * `<xsl:value-of select="E"/>` → `string(E)` (the result-tree
///   text-node materialisation collapses to the string-value)
/// * `<xsl:if test="T">B</xsl:if>` → `if (T) then DESUGAR(B) else ()`
/// * `<xsl:choose>...</xsl:choose>` → chained `if-then-else`
/// * Multiple top-level instructions → XPath 2.0 sequence
///   constructor `(E1, E2, …)`
///
/// Bodies containing `xsl:variable` are left as instruction lists
/// because XPath 2.0 has no `let` expression to lower to — the
/// instruction interpreter scopes the binding without iterating.
fn desugar_body_to_xpath(body: &[Instr]) -> Option<sup_xml_core::xpath::ast::Expr> {
    use sup_xml_core::xpath::ast::Expr;
    if body.is_empty() {
        return Some(Expr::Sequence(Vec::new()));
    }
    // Walk left-to-right, threading any `Variable` bindings into a
    // for-expression that scopes the remainder of the body.
    desugar_tail(body)
}

fn desugar_tail(tail: &[Instr]) -> Option<sup_xml_core::xpath::ast::Expr> {
    use sup_xml_core::xpath::ast::Expr;
    if tail.is_empty() {
        return Some(Expr::Sequence(Vec::new()));
    }
    if tail.len() == 1 {
        return desugar_one(&tail[0]);
    }
    // xsl:variable is a let-binding (single value), not an
    // iteration.  XPath 2.0 has no `let` expression we can lower
    // to, so bail out and let the instruction-list interpreter
    // (`eval_function_body`) evaluate the body — it layers the
    // binding into the scope chain without iterating.
    if matches!(&tail[0], Instr::Variable(_)) {
        return None;
    }
    // Otherwise the body is a comma-sequence of N items.
    let mut items: Vec<Expr> = Vec::with_capacity(tail.len());
    for ins in tail {
        items.push(desugar_one(ins)?);
    }
    Some(Expr::Sequence(items))
}

fn desugar_one(ins: &Instr) -> Option<sup_xml_core::xpath::ast::Expr> {
    use sup_xml_core::xpath::ast::Expr;
    match ins {
        Instr::Sequence { select } => Some(select.clone()),
        Instr::ValueOf { select, .. } => Some(Expr::FunctionCall(
            "string".into(), vec![select.clone()])),
        Instr::CopyOf { select, .. } => Some(select.clone()),
        Instr::If { test, body } => {
            let then_e = desugar_tail(body)?;
            Some(Expr::IfThenElse {
                cond:        Box::new(test.clone()),
                then_branch: Box::new(then_e),
                else_branch: Box::new(Expr::Sequence(Vec::new())),
            })
        }
        Instr::Choose { whens, otherwise } => {
            // Build right-to-left: start with otherwise (or empty),
            // wrap each when as if-then-else.
            let mut acc: Expr = match otherwise {
                Some(o) => desugar_tail(o)?,
                None    => Expr::Sequence(Vec::new()),
            };
            for (test, body) in whens.iter().rev() {
                let then_e = desugar_tail(body)?;
                acc = Expr::IfThenElse {
                    cond:        Box::new(test.clone()),
                    then_branch: Box::new(then_e),
                    else_branch: Box::new(acc),
                };
            }
            Some(acc)
        }
        Instr::LiteralText { text, dose: _ } => {
            // Whitespace-only literals that came from stylesheet
            // formatting (multi-char whitespace between sibling
            // instructions) don't contribute to a function's value;
            // drop them.  Empty / non-whitespace literals (including
            // the empty `<xsl:text/>` form that XSLT 2.0 §10.3 says
            // contributes a text-node item) become an explicit
            // string literal so `count()` sees the right cardinality.
            if !text.is_empty() && text.chars().all(char::is_whitespace) {
                Some(Expr::Sequence(Vec::new()))
            } else {
                Some(Expr::Literal(text.clone()))
            }
        }
        Instr::Variable(v) if v.body.is_empty() && v.select.is_none() => {
            // Useless declaration with no init; treat as empty.
            Some(Expr::Sequence(Vec::new()))
        }
        // Anything else (literal elements, xsl:copy, xsl:apply-templates,
        // xsl:for-each, etc.) doesn't desugar cleanly.
        _ => None,
    }
}

// ── templates ────────────────────────────────────────────────────

/// Default import precedence for templates compiled from the
/// top-level stylesheet (vs. ones brought in via xsl:import).
/// The xsl:import resolver overrides this with a lower value for
/// imported templates.
const TOP_LEVEL_IMPORT_PRECEDENCE: i32 = 0;

// Visited stylesheet identities (canonicalised base URI) on the
// active xsl:include / xsl:import recursion path.  Re-entering a
// URI mid-walk is XSLT §3.10 error XTSE0210 / XTSE0180 — we raise
// a clean compile error instead of recursing forever and blowing
// the stack.
thread_local! {
    static IMPORT_CHAIN: std::cell::RefCell<std::collections::HashSet<String>>
        = std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Recursive compile entry point — handles `xsl:import` /
/// `xsl:include` by calling `loader` for each referenced href,
/// parsing the result, and merging its templates into `acc` with
/// the right precedence stamp.
///
/// `precedence_counter` is a shared counter the recursion uses to
/// hand out monotonically-decreasing precedence values — the
/// outermost stylesheet's templates use 0, the first import gets
/// -1, the next gets -2 across the whole import tree (depth-first
/// walk in reverse-document-order so the LAST import has the
/// highest precedence among imports, per XSLT 1.0 §2.6.2).
///
/// The base URI threads through so relative hrefs in nested
/// imports resolve correctly.
pub fn compile_with_imports(
    text:                &str,
    loader:              &dyn Loader,
    base:                Option<&str>,
    acc:                 StylesheetAst,
    precedence_counter:  &mut i32,
) -> Result<StylesheetAst, XsltError> {
    // Decimal-format conflicts (XTSE1290) can only be judged once every
    // module across the whole import/include/use-package tree has merged,
    // since a higher-precedence declaration may resolve a lower-precedence
    // clash — so the check runs here, after the recursive build.
    #[cfg_attr(not(feature = "xsd"), allow(unused_mut))]
    let mut ast = compile_with_imports_inner(text, loader, base, acc, precedence_counter)?;
    finalize_decimal_format_conflicts(&ast)?;
    // Schema-aware: resolve `xsl:import-schema` against the loader and
    // compile each imported schema.  Lenient — a schema that can't be
    // loaded or compiled is skipped (the stylesheet still runs untyped),
    // matching the many stylesheets that import a schema they don't
    // actually depend on.
    #[cfg(feature = "xsd")]
    {
        // Bridge the XSLT `Loader` to the XSD `SchemaResolver` so a
        // schema's own `<xs:import>` / `<xs:include>` directives resolve
        // through the same loader (relative to the host base).  Using
        // `compile_with` rather than `compile_str` is what lets
        // multi-file schemas (e.g. xs:notation schemas that import
        // others) compile — `compile_str` rejects import/include.
        struct LoaderResolver<'a> { loader: &'a dyn Loader, base: Option<&'a str> }
        impl sup_xml_core::xsd::SchemaResolver for LoaderResolver<'_> {
            fn resolve(&self, location: &str, _ns: Option<&str>)
                -> std::result::Result<Option<Vec<u8>>, std::io::Error>
            {
                // Unresolvable references degrade to "not found" (None)
                // rather than failing the whole compile — matches the
                // lenient handling of the top-level import below.
                Ok(self.loader.load(location, self.base).ok().map(String::into_bytes))
            }
        }
        let imports = ast.schema_imports.clone();
        for (_ns, location) in &imports {
            if let Ok(text) = loader.load(location, base) {
                let resolver = LoaderResolver { loader, base };
                if let Ok(schema) = sup_xml_core::xsd::Schema::compile_with(&text, resolver) {
                    ast.schemas.push(std::sync::Arc::new(schema));
                }
            }
        }
    }
    Ok(ast)
}

fn compile_with_imports_inner(
    text:                &str,
    loader:              &dyn Loader,
    base:                Option<&str>,
    mut acc:             StylesheetAst,
    precedence_counter:  &mut i32,
) -> Result<StylesheetAst, XsltError> {
    // Publish the module's static base URI so `evaluate_use_when_at`
    // can resolve `fn:static-base-uri()` inside use-when expressions.
    // Restore the prior value on return so nested modules see their
    // own base.
    let prev_base = MODULE_BASE_URI.with(|b|
        std::mem::replace(&mut *b.borrow_mut(), base.map(str::to_string)));
    struct BaseGuard(Option<String>);
    impl Drop for BaseGuard {
        fn drop(&mut self) {
            MODULE_BASE_URI.with(|b| *b.borrow_mut() = self.0.take());
        }
    }
    let _base_guard = BaseGuard(prev_base);
    // Detect cycles before they recurse: if `base` is already on the
    // active chain, the stylesheet directly or indirectly includes
    // itself.  Spec calls this a static error; we raise it here so
    // the offending stylesheet doesn't loop the compiler.
    let chain_key = base.unwrap_or("").to_string();
    let inserted = IMPORT_CHAIN.with(|c|
        if !chain_key.is_empty() && c.borrow().contains(&chain_key) {
            false
        } else {
            c.borrow_mut().insert(chain_key.clone());
            true
        });
    if !inserted {
        return Err(XsltError::InvalidStylesheet(format!(
            "stylesheet '{chain_key}' includes itself (directly or via a chain)"
        )));
    }
    struct ChainGuard(String);
    impl Drop for ChainGuard {
        fn drop(&mut self) {
            IMPORT_CHAIN.with(|c| { c.borrow_mut().remove(&self.0); });
        }
    }
    let _guard = ChainGuard(chain_key);
    // Stylesheets often carry a `<!DOCTYPE foo SYSTEM "x.dtd">` whose
    // external subset defines entity references (`&aelig;`, `&copy;`,
    // …) used in literal result content.  Enable external-DTD loading
    // through a sandboxed [`FilesystemResolver`] scoped to the
    // directory of the stylesheet being parsed; SYSTEM literals
    // outside that directory are refused.
    let resolver: Option<std::sync::Arc<dyn sup_xml_core::EntityResolver>> = base
        .and_then(|b| std::path::Path::new(b).parent().map(std::path::Path::to_path_buf))
        .map(|dir| {
            let r: std::sync::Arc<dyn sup_xml_core::EntityResolver> =
                std::sync::Arc::new(sup_xml_core::FilesystemResolver::new(vec![dir]));
            r
        });
    let opts = sup_xml_core::ParseOptions {
        namespace_aware:   true,
        load_external_dtd: resolver.is_some(),
        external_resolver: resolver,
        base_url:          base.map(str::to_string),
        ..Default::default()
    };
    let doc = sup_xml_core::parse_str(text, &opts).map_err(XsltError::from)?;
    // XSLT 2.0 §3.10.2 — `use-when` on the principal element of an
    // included / imported stylesheet (xsl:stylesheet, xsl:transform,
    // or a simplified-stylesheet root) elides the entire module if
    // the expression evaluates to false.
    let root = doc.root();
    if root.is_element() {
        // XSLT 2.0 §3.6 — the version= attribute on an xsl:stylesheet
        // / xsl:transform root must be a valid xs:decimal even when
        // use-when on the same element would elide the module.  The
        // W3C suite (use-when-0227) treats this as a precondition the
        // use-when= short-circuit doesn't override.
        if is_xslt_element(root)
            && matches!(root.local_name(), "stylesheet" | "transform" | "package")
        {
            if let Some(v) = read_attribute(root, "version") {
                if !is_xs_decimal_lexical(v) {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:{} version='{v}' is not a valid xs:decimal (XTSE0110)",
                        root.local_name()
                    )));
                }
            }
        }
        if let Some(uw) = read_attribute(root, "use-when") {
            if !evaluate_use_when_at(uw, Some(root))? {
                return Ok(acc);
            }
        }
    }
    let local = compile(&doc)?;
    // Capture the using package's own references before the merge
    // below moves local's component vectors — needed for the
    // xsl:use-package visibility check (XSLT 3.0 §3.5.2).
    let pkg_refs = (!local.use_packages.is_empty()).then(|| capture_pkg_refs(&local));

    // The locally-compiled stylesheet's precedence is whatever the
    // caller pre-stamped on `acc.templates` length-wise — but
    // simpler: we use `*precedence_counter` as this stylesheet's
    // precedence, then decrement for each import.
    let this_precedence = *precedence_counter;

    // Stamp local templates with this stylesheet's precedence.
    let mut local_templates = local.templates;
    for t in &mut local_templates {
        t.import_precedence = this_precedence;
    }
    let mut local_attribute_sets = local.attribute_sets;
    for s in &mut local_attribute_sets {
        s.import_precedence = this_precedence;
    }
    // Stamp local strip/preserve-space rules.  XSLT 1.0 §3.4's
    // tiebreaker for whitespace handling promotes the import
    // precedence above pattern specificity, so the runtime needs
    // the precedence on each rule.
    let mut local_whitespace_rules = local.whitespace_rules;
    for r in &mut local_whitespace_rules {
        match r {
            WhitespaceRule::Strip(_, p) | WhitespaceRule::Preserve(_, p) => {
                *p = this_precedence;
            }
        }
    }

    // Merge top-level decls.  The outer `acc` holds the so-far
    // accumulated state; we append local's decls.  Namespace map:
    // OUTER wins on conflict (the outer stylesheet's bindings
    // override imports, matching XSLT 1.0 §3.2's "in-scope
    // namespaces" rules at the consuming stylesheet's level).
    for (p, u) in local.namespaces {
        acc.namespaces.entry(p).or_insert(u);
    }
    acc.templates.extend(local_templates);
    acc.global_variables.extend(local.global_variables);
    acc.global_params.extend(local.global_params);
    acc.keys.extend(local.keys);
    acc.attribute_sets.extend(local_attribute_sets);
    acc.modes.extend(local.modes);
    acc.accumulators.extend(local.accumulators);
    acc.schema_imports.extend(local.schema_imports);
    // Decimal-formats merge per-attribute by import precedence (XSLT 2.0
    // §16.4.2): this module is merged at `this_precedence`; its imports
    // recurse below at a lower precedence and cannot override an
    // attribute this module set.
    for (k, df) in &local.decimal_formats {
        let mask = local.decimal_format_explicit.get(k).copied().unwrap_or(0);
        let conflict = local.decimal_format_conflicts.get(k).copied().unwrap_or(0);
        merge_decimal_format_into(&mut acc, k, df, mask, conflict, this_precedence);
    }
    acc.whitespace_rules.extend(local_whitespace_rules);
    acc.outputs.extend(local.outputs);
    acc.input_type_annotations.extend(local.input_type_annotations);
    acc.includes.extend(local.includes.iter().cloned());
    acc.imports.extend(local.imports.iter().cloned());
    acc.namespace_aliases.extend(local.namespace_aliases);
    acc.documents_to_load.extend(local.documents_to_load);
    acc.functions.extend(local.functions);
    acc.character_maps.extend(local.character_maps);
    if acc.version.is_empty() { acc.version = local.version; }
    // First stylesheet on the chain (the principal one) wins for
    // `xml:base` — imports/includes have their own base URIs that
    // would apply to expressions inside *their* compiled stylesheet
    // only; the runtime currently uses a single static base URI for
    // the whole transformation.
    if acc.xml_base.is_none() { acc.xml_base = local.xml_base; }

    // Resolve xsl:include first — same precedence as the
    // including stylesheet (the include's contents are
    // logically inlined, XSLT 1.0 §2.6.1).  After each include is
    // expanded, prepend the include's source position to every
    // newly-added template's `source_path` so the include's
    // contents sort *at* the include directive's position for
    // §5.5 source-order conflict resolution.
    for (href, &inc_pos) in local.includes.iter().zip(local.include_positions.iter()) {
        let inc_text = loader.load(href, base)?;
        let inc_base = loader.resolve(href, base).ok();
        // Reuse this stylesheet's precedence — don't decrement.
        let prev = *precedence_counter;
        *precedence_counter = this_precedence;
        let before = acc.templates.len();
        acc = compile_with_imports_inner(&inc_text, loader, inc_base.as_deref(), acc, precedence_counter)?;
        for t in &mut acc.templates[before..] {
            let mut p = vec![inc_pos];
            p.append(&mut t.source_path);
            t.source_path = p;
        }
        *precedence_counter = prev;
    }

    // Resolve xsl:import in REVERSE document order so the LAST
    // xsl:import in the source ends up with the HIGHEST import
    // precedence among siblings (each gets a fresh, lower
    // counter value, but we walk last-to-first so the last one
    // gets the highest of the import-counter values).
    // Imports get a separate precedence per the spec, so we don't
    // need to align their `source_path` with the importing file —
    // import precedence is the dominant tiebreaker anyway.
    for href in local.imports.iter().rev() {
        let imp_text = loader.load(href, base)?;
        let imp_base = loader.resolve(href, base).ok();
        *precedence_counter -= 1;
        acc = compile_with_imports_inner(&imp_text, loader, imp_base.as_deref(), acc, precedence_counter)?;
    }

    // Resolve xsl:use-package (XSLT 3.0 §3.5.1).  An xsl:override's
    // declarations merge at the using package's precedence so they win
    // over the used package's originals; the used package itself merges
    // at a lower precedence (like an import).
    for up in &local.use_packages {
        merge_package_components(&mut acc, &up.overrides, this_precedence);
        match get_package_source(&up.name) {
            Some((src, pkg_base)) => {
                let before_fns = acc.functions.len();
                let before_vars = acc.global_variables.len();
                *precedence_counter -= 1;
                acc = compile_with_imports_inner(
                    &src, loader, pkg_base.as_deref(), acc, precedence_counter)?;
                // XSLT 3.0 §3.5.2 — the using package may not reference
                // the used package's private components.
                if let Some(refs) = &pkg_refs {
                    let used_fns = acc.functions[before_fns..].to_vec();
                    let used_vars = acc.global_variables[before_vars..].to_vec();
                    check_used_package_privates(refs, &used_fns, &used_vars)?;
                }
            }
            None => return Err(XsltError::InvalidStylesheet(format!(
                "xsl:use-package: no package named '{}' is available (XTSE3000)",
                up.name))),
        }
    }

    Ok(acc)
}

/// True when a component's `visibility=` makes it usable by a package
/// that uses the declaring package (XSLT 3.0 §3.5.2).  The default
/// (None) is `private`.
fn visible_to_user(vis: &Option<String>) -> bool {
    matches!(vis.as_deref(), Some("public") | Some("final") | Some("abstract"))
}

/// Expand a lexical `prefix:local` reference to `{uri}local` using the
/// in-scope namespaces, matching [`qname_key`]'s form.  An unprefixed
/// name keeps its local form.
fn expand_lexical(name: &str, ns: &std::collections::HashMap<String, String>) -> String {
    match name.split_once(':') {
        Some((p, l)) => match ns.get(p) {
            Some(uri) if !uri.is_empty() => format!("{{{uri}}}{l}"),
            _ => name.to_string(),
        },
        None => name.to_string(),
    }
}

/// Best-effort collector of the function-call and variable-reference
/// names appearing in `e`.  Covers the common XPath shapes; unhandled
/// variants simply contribute nothing (so the visibility check can
/// only under-report, never raise a spurious error).
fn collect_expr_refs(e: &Expr, fns: &mut Vec<String>, vars: &mut Vec<String>) {
    use sup_xml_core::xpath::ast::Expr::*;
    use sup_xml_core::xpath::ast::LocationPath;
    let two = |a: &Expr, b: &Expr, f: &mut Vec<String>, v: &mut Vec<String>| {
        collect_expr_refs(a, f, v); collect_expr_refs(b, f, v);
    };
    match e {
        FunctionCall(name, args) => {
            fns.push(name.clone());
            for a in args { collect_expr_refs(a, fns, vars); }
        }
        Variable(n) => vars.push(n.clone()),
        Or(l, r) | And(l, r) | Eq(l, r) | Ne(l, r) | Lt(l, r) | Gt(l, r)
        | Le(l, r) | Ge(l, r) | ValueEq(l, r) | ValueNe(l, r) | ValueLt(l, r)
        | ValueGt(l, r) | ValueLe(l, r) | ValueGe(l, r) | Add(l, r) | Sub(l, r)
        | Mul(l, r) | Div(l, r) | Mod(l, r) | Union(l, r) | IDiv(l, r)
        | Intersect(l, r) | Except(l, r) | Range(l, r) | SimpleMap(l, r)
        | NodeBefore(l, r) | NodeAfter(l, r) | NodeIs(l, r) => two(l, r, fns, vars),
        Neg(x) | InstanceOf(x, _) | CastAs(x, _) | CastableAs(x, _)
        | TreatAs(x, _) | WithDefaultCollation(_, x) | BackwardsCompat(x) =>
            collect_expr_refs(x, fns, vars),
        IfThenElse { cond, then_branch, else_branch } => {
            collect_expr_refs(cond, fns, vars);
            collect_expr_refs(then_branch, fns, vars);
            collect_expr_refs(else_branch, fns, vars);
        }
        For { bindings, body } | Let { bindings, body }
        | Quantified { bindings, test: body, .. } => {
            for (_, ex) in bindings { collect_expr_refs(ex, fns, vars); }
            collect_expr_refs(body, fns, vars);
        }
        Sequence(items) => for x in items { collect_expr_refs(x, fns, vars); },
        FilterPath { primary, predicates, steps } => {
            collect_expr_refs(primary, fns, vars);
            for p in predicates { collect_expr_refs(p, fns, vars); }
            for s in steps { for p in &s.predicates { collect_expr_refs(p, fns, vars); } }
        }
        Path(LocationPath::Absolute(steps)) | Path(LocationPath::Relative(steps)) => {
            for s in steps { for p in &s.predicates { collect_expr_refs(p, fns, vars); } }
        }
        _ => {}
    }
}

fn collect_body_refs(body: &[Instr], fns: &mut Vec<String>, vars: &mut Vec<String>) {
    crate::walk::walk_body(body, &mut |e: &Expr| collect_expr_refs(e, fns, vars));
}

/// The using package's own references and declared names, captured
/// before the merge consumes its component vectors, for the
/// xsl:use-package visibility check.
struct PkgRefs {
    ref_fns:  Vec<String>,
    ref_vars: Vec<String>,
    own_fns:  std::collections::HashSet<String>,
    own_vars: std::collections::HashSet<String>,
    ns:       std::collections::HashMap<String, String>,
}

/// Capture the references made by — and the names declared by — the
/// using package's own code (plus its xsl:override bodies).
fn capture_pkg_refs(local: &StylesheetAst) -> PkgRefs {
    use std::collections::HashSet;
    let mut ref_fns = Vec::new();
    let mut ref_vars = Vec::new();
    for t in &local.templates { collect_body_refs(&t.body, &mut ref_fns, &mut ref_vars); }
    for f in &local.functions { collect_body_refs(&f.body, &mut ref_fns, &mut ref_vars); }
    for v in &local.global_variables {
        if let Some(s) = &v.select { collect_expr_refs(s, &mut ref_fns, &mut ref_vars); }
        collect_body_refs(&v.body, &mut ref_fns, &mut ref_vars);
    }
    let mut own_fns: HashSet<String> = local.functions.iter().map(|f| qname_key(&f.name)).collect();
    let mut own_vars: HashSet<String> = local.global_variables.iter().map(|v| qname_key(&v.name))
        .chain(local.global_params.iter().map(|p| qname_key(&p.name))).collect();
    // The xsl:override bodies count as the using package's own code.
    for up in &local.use_packages {
        for t in &up.overrides.templates { collect_body_refs(&t.body, &mut ref_fns, &mut ref_vars); }
        for f in &up.overrides.functions {
            collect_body_refs(&f.body, &mut ref_fns, &mut ref_vars);
            own_fns.insert(qname_key(&f.name));
        }
        for v in &up.overrides.global_variables {
            if let Some(s) = &v.select { collect_expr_refs(s, &mut ref_fns, &mut ref_vars); }
            own_vars.insert(qname_key(&v.name));
        }
    }
    PkgRefs { ref_fns, ref_vars, own_fns, own_vars, ns: local.namespaces.clone() }
}

/// Enforce that the using package does not reference a used package's
/// private components (XSLT 3.0 §3.5.2): a reference to a function /
/// variable that is private in the used package — and neither public
/// there nor declared by the using package — is a static error
/// (XPST0017 for functions, XPST0008 for variables).
fn check_used_package_privates(
    refs: &PkgRefs,
    used_functions: &[UserFunction],
    used_variables: &[Variable],
) -> Result<(), XsltError> {
    use std::collections::HashSet;
    let priv_fns: HashSet<String> = used_functions.iter()
        .filter(|f| !visible_to_user(&f.visibility)).map(|f| qname_key(&f.name)).collect();
    let pub_fns: HashSet<String> = used_functions.iter()
        .filter(|f| visible_to_user(&f.visibility)).map(|f| qname_key(&f.name)).collect();
    let priv_vars: HashSet<String> = used_variables.iter()
        .filter(|v| !visible_to_user(&v.visibility)).map(|v| qname_key(&v.name)).collect();
    let pub_vars: HashSet<String> = used_variables.iter()
        .filter(|v| visible_to_user(&v.visibility)).map(|v| qname_key(&v.name)).collect();
    for r in &refs.ref_fns {
        let key = expand_lexical(r, &refs.ns);
        if priv_fns.contains(&key) && !pub_fns.contains(&key) && !refs.own_fns.contains(&key) {
            return Err(XsltError::InvalidStylesheet(format!(
                "reference to private function '{r}' of a used package (XPST0017)")));
        }
    }
    for r in &refs.ref_vars {
        let key = expand_lexical(r, &refs.ns);
        if priv_vars.contains(&key) && !pub_vars.contains(&key) && !refs.own_vars.contains(&key) {
            return Err(XsltError::InvalidStylesheet(format!(
                "reference to private variable '${r}' of a used package (XPST0008)")));
        }
    }
    Ok(())
}

/// Merge a sub-stylesheet's components into `acc` at import precedence
/// `prec` — used for `xsl:override` declarations (XSLT 3.0 §3.5.1),
/// which must outrank the used package's originals.
fn merge_package_components(acc: &mut StylesheetAst, sub: &StylesheetAst, prec: i32) {
    for t in &sub.templates {
        let mut t = t.clone();
        t.import_precedence = prec;
        acc.templates.push(t);
    }
    for s in &sub.attribute_sets {
        let mut s = s.clone();
        s.import_precedence = prec;
        acc.attribute_sets.push(s);
    }
    acc.global_variables.extend(sub.global_variables.iter().cloned());
    acc.global_params.extend(sub.global_params.iter().cloned());
    acc.functions.extend(sub.functions.iter().cloned());
    acc.accumulators.extend(sub.accumulators.iter().cloned());
    acc.modes.extend(sub.modes.iter().cloned());
    acc.keys.extend(sub.keys.iter().cloned());
    for (k, df) in &sub.decimal_formats {
        let mask = sub.decimal_format_explicit.get(k).copied().unwrap_or(0);
        let conflict = sub.decimal_format_conflicts.get(k).copied().unwrap_or(0);
        merge_decimal_format_into(acc, k, df, mask, conflict, prec);
    }
    acc.character_maps.extend(sub.character_maps.iter().cloned());
}

fn compile_template(node: &Node) -> Result<Template, XsltError> {
    // XSLT 2.0 §6.7 — `match`, `name`, `priority`, `mode`, `as`.
    // Generic §3.6 attributes (use-when / version / xpath-default-
    // namespace / default-collation / extension-element-prefixes /
    // exclude-result-prefixes / expand-text) are added by
    // `validate_xslt_only_attributes`.  XSLT 3.0 adds `visibility`.
    validate_xslt_only_attributes(node, "xsl:template",
        &["match", "name", "priority", "mode", "as", "visibility"])?;
    let match_pattern = match read_attribute(node, "match") {
        Some(s) => {
            // XSLT 2.0 §5.5.2 / XTSE0340 — parenthesised expressions
            // are not allowed at the top level of a pattern in 2.0
            // (relaxed in 3.0).  The XPath parser collapses parens,
            // so we look at the source string itself.  Skip past any
            // leading XPath 2.0 comments `(: ... :)` — those aren't
            // grouping parens, just whitespace.
            let leading = strip_leading_xpath_comments_and_space(s.as_ref());
            if leading.starts_with('(') {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:template match='{s}' uses a parenthesised \
                     top-level expression, which is not allowed in a \
                     pattern (XTSE0340)"
                )));
            }
            let e = parse_xpath_at(node, s).map_err(XsltError::from)?;
            reject_pattern_grouping_calls(&e, "xsl:template match=")?;
            reject_invalid_pattern_axes(&e, "xsl:template match=")?;
            reject_invalid_pattern_key_calls(&e, "xsl:template match=")?;
            ensure_pattern_shape(&e, "xsl:template match=")?;
            Some(e)
        }
        None    => None,
    };
    let name = read_attribute(node, "name")
        .map(|s| parse_qname_on(node, s))
        .transpose()?;
    if let Some(n) = &name { reject_reserved_name(n, "xsl:template")?; }
    // XSLT 2.0 §6 — `mode=` is a whitespace-separated list of
    // tokens: `#default`, `#all`, or a QName.  XSLT 1.0 only ever
    // saw a single QName.  Collect them all into `modes` plus the
    // `#all` flag; keep `mode` (single Option<QName>) for callers
    // (and tests) that only need the first / primary entry.
    let mut modes: Vec<QName> = Vec::new();
    let mut modes_match_all = false;
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(raw) = read_attribute(node, "mode") {
        let mut tok_count = 0usize;
        for tok in raw.split_whitespace() {
            tok_count += 1;
            // XSLT 2.0 §6 / XTSE0550 — `#all` may not appear alongside
            // any other token; duplicate tokens are an error.
            let key = tok.to_string();
            if !seen_keys.insert(key.clone()) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:template mode='{raw}' lists '{tok}' more than once (XTSE0550)"
                )));
            }
            match tok {
                "#all"     => modes_match_all = true,
                "#default" => modes.push(QName {
                    prefix: None, local: String::new(), uri: String::new(),
                }),
                "#current" => return Err(XsltError::InvalidStylesheet(
                    "xsl:template mode='#current' is not valid on the \
                     template declaration (XTSE0550)".into()
                )),
                qn => modes.push(parse_qname_on(node, qn)?),
            }
        }
        if tok_count == 0 {
            return Err(XsltError::InvalidStylesheet(
                "xsl:template mode= attribute must list at least one token (XTSE0550)".into()
            ));
        }
        if modes_match_all && tok_count > 1 {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:template mode='{raw}' — '#all' must appear alone (XTSE0550)"
            )));
        }
    }
    // The primary `mode` slot stays compatible with single-mode
    // callers: it's `Some(first-listed-mode)` for the legacy code
    // paths, or `None` when the list is empty.  `#all` templates
    // expose `None` here too — selection consults `modes_match_all`.
    let mode = modes.first().cloned().and_then(|m| {
        if m.local.is_empty() && m.uri.is_empty() { None } else { Some(m) }
    });
    let priority = match read_attribute(node, "priority") {
        Some(s) => {
            // XSLT 2.0 §6 — `priority=` is constrained to xs:decimal
            // (XTSE0530); non-decimal lexicals like "highest" or
            // scientific notation are static errors.
            if !is_xs_decimal_lexical(s) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:template priority='{s}' is not a valid xs:decimal \
                     (XTSE0530)"
                )));
            }
            Some(s.trim().parse::<f64>().map_err(|_| XsltError::InvalidStylesheet(
                format!("xsl:template priority must be a number, got '{s}'")))?)
        }
        None => None,
    };

    if match_pattern.is_none() && name.is_none() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:template must have either match= or name= (XTSE0500)".into(),
        ));
    }
    // XSLT 2.0 §6 — an xsl:template with no match= must not carry
    // mode= or priority= attributes (XTSE0500).
    if match_pattern.is_none() {
        if mode.is_some() {
            return Err(XsltError::InvalidStylesheet(
                "xsl:template with no match= cannot carry mode= (XTSE0500)".into(),
            ));
        }
        if priority.is_some() {
            return Err(XsltError::InvalidStylesheet(
                "xsl:template with no match= cannot carry priority= (XTSE0500)".into(),
            ));
        }
    }

    // Body opens with `xsl:param` declarations (if any), then any
    // mix of instructions.  Split at first non-param child.
    let mut params = Vec::new();
    let mut body   = Vec::new();
    let mut seen_non_param = false;
    for child in node.children() {
        if !child.is_element() && !is_significant_text(child) { continue; }
        if !seen_non_param
            && child.is_element() && is_xslt_element(child)
            && child.local_name() == "param"
        {
            let p = compile_param(child)?;
            // XSLT 2.0 §10.1 / XTSE0580 — two xsl:param children of
            // the same template / function may not share a name.
            let key = qname_key(&p.name);
            if params.iter().any(|q: &Param| qname_key(&q.name) == key) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate xsl:param '{key}' on xsl:template (XTSE0580)"
                )));
            }
            params.push(p);
            continue;
        }
        seen_non_param = true;
        compile_instr_into(child, &mut body)?;
    }

    let as_type = read_attribute(node, "as").map(str::to_string);
    Ok(Template {
        match_pattern, name, mode, modes, modes_match_all, priority,
        import_precedence: TOP_LEVEL_IMPORT_PRECEDENCE,
        source_path: Vec::new(),
        params, body, as_type,
    })
}

// ── variables / params ───────────────────────────────────────────

fn compile_variable(node: &Node) -> Result<Variable, XsltError> {
    // `as` and `static` are XSLT 2.0 / 3.0 additions; the validator
    // is mode-aware via the `FORWARDS_COMPAT_MODE` guard above.
    validate_xslt_only_attributes(node, "xsl:variable",
        &["name", "select", "as", "static", "visibility"])?;
    let name = required_qname_attr(node, "name", "xsl:variable")?;
    reject_reserved_name(&name, "xsl:variable")?;
    let (select, body) = split_select_and_body(node)?;
    reject_select_with_body(node, &select, &body, "xsl:variable")?;
    let as_type = read_attribute(node, "as").map(str::to_string);
    // XPath 2.0 §3.1.5 — the body-form temporary tree's document
    // node carries the resolved xml:base from the variable's
    // declaration site.  Capture it once at compile time so the
    // runtime can stamp the RTF root without re-walking the
    // ancestor chain on every variable binding.
    let base_uri = effective_xml_base(node);
    let visibility = read_attribute(node, "visibility").map(str::to_string);
    Ok(Variable { name, select, body, as_type, base_uri, visibility })
}

fn compile_param(node: &Node) -> Result<Param, XsltError> {
    validate_xslt_only_attributes(node, "xsl:param",
        &["name", "select", "as", "required", "tunnel", "static"])?;
    let name = required_qname_attr(node, "name", "xsl:param")?;
    reject_reserved_name(&name, "xsl:param")?;
    let (select, body) = split_select_and_body(node)?;
    reject_select_with_body(node, &select, &body, "xsl:param")?;
    let tunnel = if is_xslt_2_0_compile() {
        match read_attribute(node, "tunnel") {
            Some(v) => parse_yesno_strict(v, "xsl:param", "tunnel")?,
            None    => false,
        }
    } else { false };
    let as_type = read_attribute(node, "as").map(str::to_string);
    let required = if is_xslt_2_0_compile() {
        match read_attribute(node, "required") {
            Some(v) => parse_yesno_strict(v, "xsl:param", "required")?,
            None    => false,
        }
    } else { false };
    // XSLT 2.0 §9.5 / XTSE0010 — a required parameter has no
    // meaningful default, so neither select= nor a sequence-
    // constructor body may be supplied.
    if required && (select.is_some() || !body.is_empty()) {
        return Err(XsltError::InvalidStylesheet(format!(
            "xsl:param '{}' is required and cannot specify a default \
             value (XTSE0010)", qname_key(&name),
        )));
    }
    Ok(Param { name, select, body, tunnel, as_type, required })
}

/// XSLT 2.0 §9.2 / XTSE0620 — a variable-binding element
/// (xsl:variable, xsl:param, xsl:with-param) may not specify both
/// a `select=` attribute and a non-empty sequence constructor.
fn reject_select_with_body(
    _node: &Node, select: &Option<Expr>, body: &[Instr], who: &str,
) -> Result<(), XsltError> {
    if in_forwards_compat_mode() { return Ok(()); }
    let has_body = body.iter().any(|i| !matches!(i,
        Instr::LiteralText { text, .. } if text.trim().is_empty()));
    if select.is_some() && has_body {
        return Err(XsltError::InvalidStylesheet(format!(
            "{who} cannot have both a select= attribute and a non-empty \
             body (XTSE0620)"
        )));
    }
    Ok(())
}

// xsl:variable / xsl:param can carry either a select= XPath OR a
// body sequence-constructor (mutually exclusive per the spec, but
// we don't enforce it strictly — body wins if both present,
// matching libxslt).
fn split_select_and_body(node: &Node)
    -> Result<(Option<Expr>, Vec<Instr>), XsltError>
{
    let select = match read_attribute(node, "select") {
        Some(s) => Some(parse_xpath_at(node, s).map_err(XsltError::from)?),
        None    => None,
    };
    let mut body = Vec::new();
    for child in node.children() {
        if !child.is_element() && !is_significant_text(child) { continue; }
        compile_instr_into(child, &mut body)?;
    }
    Ok((select, body))
}

// An instruction whose content is either a `select=` XPath OR a
// contained sequence constructor, but not both (XTSE3280 for the
// on-empty / on-non-empty family).  A `select=` is shorthand for a
// body of a single `xsl:sequence`.
fn select_or_body(node: &Node, who: &str) -> Result<Vec<Instr>, XsltError> {
    let (select, body) = split_select_and_body(node)?;
    reject_select_with_body(node, &select, &body, who)?;
    Ok(match select {
        Some(select) => vec![Instr::Sequence { select }],
        None => body,
    })
}

// ── keys, attribute-sets, outputs ────────────────────────────────

fn compile_key(node: &Node) -> Result<Key, XsltError> {
    // XSLT 2.0 §16.3 adds `collation` to the 1.0 `name`/`match`/`use`.
    validate_xslt_only_attributes(node, "xsl:key",
        &["name", "match", "use", "collation"])?;
    let name = required_qname_attr(node, "name", "xsl:key")?;
    reject_reserved_name(&name, "xsl:key")?;
    // XSLT 2.0 §16.3 / XTSE1210 — when the optional collation= URI
    // isn't one the processor recognises, that's a static error.
    // We currently implement only the codepoint collation; any
    // other URI is rejected (unless we're in forwards-compat mode).
    let collation = if let Some(c) = read_attribute(node, "collation") {
        if !in_forwards_compat_mode() && !is_recognised_collation(c) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:key collation='{c}' is not recognised by the processor (XTSE1210)"
            )));
        }
        Some(c.trim().to_string())
    } else {
        // XSLT 2.0 §16.3 — absent collation= falls back to the
        // in-scope default-collation, which itself defaults to
        // codepoint.  We only carry through a non-codepoint
        // URI here; codepoint stays implicit.
        effective_default_collation(node)
    };
    let m = require_attr(node, "match", "xsl:key")?;
    let matcher = parse_xpath_at(node, m).map_err(XsltError::from)?;
    reject_invalid_pattern_axes(&matcher, "xsl:key match=")?;
    ensure_pattern_shape(&matcher, "xsl:key match=")?;
    // XSLT 2.0 §16.3 — the key value is given EITHER by the `use=`
    // attribute OR by a contained sequence constructor, but not both
    // and not neither (XTSE1205).
    let has_content = node.children().any(|c| match c.kind {
        NodeKind::Element => true,
        NodeKind::Text | NodeKind::CData => !is_xslt_whitespace_only(c.content()),
        _ => false,
    });
    match (read_attribute(node, "use"), has_content) {
        (Some(_), true) => Err(XsltError::InvalidStylesheet(
            "xsl:key must not have both a use= attribute and a \
             sequence constructor (XTSE1205)".into())),
        (None, false) => Err(XsltError::InvalidStylesheet(
            "xsl:key requires either a use= attribute or a sequence \
             constructor (XTSE1205)".into())),
        (Some(u), false) => Ok(Key {
            name,
            matcher,
            use_: parse_xpath_at(node, u).map_err(XsltError::from)?,
            body: Vec::new(),
            collation,
        }),
        (None, true) => Ok(Key {
            name,
            matcher,
            use_: Expr::Sequence(Vec::new()),
            body: compile_body(node)?,
            collation,
        }),
    }
}

/// Compile an `<xsl:mode>` declaration (XSLT 3.0 §6.6).  Only the
/// `on-no-match` action affects evaluation today; the streamability /
/// typing / multiple-match properties are accepted and validated but
/// otherwise inert.
fn compile_mode(node: &Node) -> Result<ModeDecl, XsltError> {
    validate_xslt_only_attributes(node, "xsl:mode", &[
        "name", "streamable", "on-no-match", "on-multiple-match",
        "warning-on-no-match", "warning-on-multiple-match", "typed", "visibility",
    ])?;
    validate_must_be_empty(node, "xsl:mode")?;
    let name = match read_attribute(node, "name") {
        None | Some("#default") | Some("#unnamed") => None,
        Some(qn) => Some(parse_qname_on(node, qn)?),
    };
    let on_no_match = match read_attribute(node, "on-no-match") {
        None | Some("text-only-copy") => OnNoMatch::TextOnlyCopy,
        Some("deep-copy")    => OnNoMatch::DeepCopy,
        Some("shallow-copy") => OnNoMatch::ShallowCopy,
        Some("deep-skip")    => OnNoMatch::DeepSkip,
        Some("shallow-skip") => OnNoMatch::ShallowSkip,
        Some("fail")         => OnNoMatch::Fail,
        Some(other) => return Err(XsltError::InvalidStylesheet(format!(
            "xsl:mode on-no-match='{other}' is not a recognised value (XTSE0020)"))),
    };
    Ok(ModeDecl { name, on_no_match })
}

/// Compile an `<xsl:use-package>` declaration (XSLT 3.0 §3.5.1).  The
/// referenced package is resolved by name after the local stylesheet;
/// here we capture the name/version and compile any `<xsl:override>`
/// children's declarations into a sub-stylesheet so they can be merged
/// at the using package's precedence.
fn compile_use_package(node: &Node) -> Result<UsePackage, XsltError> {
    let name = require_attr(node, "name", "xsl:use-package")?.to_string();
    let version = read_attribute(node, "package-version").map(str::to_string);
    let mut overrides = StylesheetAst::default();
    let mut pos: u32 = 0;
    for child in node.children() {
        if !child.is_element() || !is_xslt_element(child) { continue; }
        match child.local_name() {
            "override" => {
                // Each child of xsl:override is a component declaration
                // that replaces the corresponding used-package one.
                for decl in child.children() {
                    if !decl.is_element() { continue; }
                    compile_top_level(decl, &mut overrides, pos)?;
                    pos += 1;
                }
            }
            // xsl:accept adjusts re-export visibility; accepted as a
            // no-op (we don't enforce visibility on a single level of
            // use-package).
            "accept" => {}
            _ => {}
        }
    }
    Ok(UsePackage { name, version, overrides: Box::new(overrides) })
}

/// Compile an `<xsl:accumulator>` declaration (XSLT 3.0 §18).
fn compile_accumulator(node: &Node) -> Result<AccumulatorDecl, XsltError> {
    validate_xslt_only_attributes(node, "xsl:accumulator",
        &["name", "initial-value", "as", "streamable"])?;
    let name = match read_attr_with_shadow(node, "name")? {
        Some(s) => parse_qname_on(node, &s)?,
        None => return Err(XsltError::InvalidStylesheet(
            "xsl:accumulator requires a name= attribute (XTSE0010)".into())),
    };
    let initial_value = parse_xpath_at(node,
        require_attr(node, "initial-value", "xsl:accumulator")?)
        .map_err(XsltError::from)?;
    let mut rules = Vec::new();
    for child in node.children() {
        if !child.is_element() {
            if is_significant_text(child) {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:accumulator cannot contain text content (XTSE0010)".into()));
            }
            continue;
        }
        if !is_xslt_element(child) || child.local_name() != "accumulator-rule" {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:accumulator may only contain xsl:accumulator-rule, found <{}> \
                 (XTSE0010)", child.name())));
        }
        rules.push(compile_accumulator_rule(child)?);
    }
    Ok(AccumulatorDecl { name, initial_value, rules })
}

fn compile_accumulator_rule(node: &Node) -> Result<AccumulatorRule, XsltError> {
    validate_xslt_only_attributes(node, "xsl:accumulator-rule",
        &["match", "phase", "select"])?;
    let match_pattern = {
        let e = parse_xpath_at(node, require_attr(node, "match", "xsl:accumulator-rule")?)
            .map_err(XsltError::from)?;
        reject_invalid_pattern_axes(&e, "xsl:accumulator-rule match=")?;
        e
    };
    let phase = match read_attribute(node, "phase") {
        None | Some("start") => AccumulatorPhase::Start,
        Some("end")          => AccumulatorPhase::End,
        Some(other) => return Err(XsltError::InvalidStylesheet(format!(
            "xsl:accumulator-rule phase='{other}' must be 'start' or 'end' (XTSE0020)"))),
    };
    let select = read_attribute(node, "select")
        .map(|s| parse_xpath_at(node, s)).transpose().map_err(XsltError::from)?;
    let mut body = Vec::new();
    if select.is_none() {
        for child in node.children() {
            if !child.is_element() && !is_significant_text(child) { continue; }
            compile_instr_into(child, &mut body)?;
        }
    }
    Ok(AccumulatorRule { match_pattern, phase, select, body })
}

fn compile_attribute_set(node: &Node) -> Result<AttributeSet, XsltError> {
    validate_xslt_only_attributes(node, "xsl:attribute-set",
        &["name", "use-attribute-sets"])?;
    let name = required_qname_attr(node, "name", "xsl:attribute-set")?;
    reject_reserved_name(&name, "xsl:attribute-set")?;
    let use_attribute_sets = parse_qname_list(
        node, read_attribute(node, "use-attribute-sets").unwrap_or(""),
    )?;
    let mut attributes = Vec::new();
    for child in node.children() {
        // Non-whitespace text inside xsl:attribute-set is XTSE0010.
        if matches!(child.kind, NodeKind::Text | NodeKind::CData)
            && !is_xslt_whitespace_only(child.content())
        {
            return Err(XsltError::InvalidStylesheet(
                "xsl:attribute-set cannot contain non-whitespace text \
                 content (XTSE0010 — only xsl:attribute allowed)".into(),
            ));
        }
        if !child.is_element() { continue; }
        if !is_xslt_element(child) || child.local_name() != "attribute" {
            return Err(XsltError::InvalidStylesheet(
                "xsl:attribute-set children must be xsl:attribute".into(),
            ));
        }
        let mut buf = Vec::with_capacity(1);
        compile_instr_into(child, &mut buf)?;
        attributes.extend(buf);
    }
    Ok(AttributeSet {
        name, use_attribute_sets, attributes,
        import_precedence: TOP_LEVEL_IMPORT_PRECEDENCE,
    })
}

fn compile_output(node: &Node) -> Result<OutputSpec, XsltError> {
    let mut out = OutputSpec::default();
    // XSLT 2.0 §20 — each boolean / enumerated attribute on
    // xsl:output has a closed value set; non-conforming values are
    // XTSE0020 statically.  In forwards-compat mode the check is
    // skipped so future-version stylesheets retain freedom.
    let require_yesno = |n: &Node, attr: &str| -> Result<(), XsltError> {
        if in_forwards_compat_mode() { return Ok(()); }
        if let Some(v) = read_attribute(n, attr) {
            if !matches!(v.trim(), "yes" | "no") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:output {attr}='{v}' must be 'yes' or 'no' (XTSE0020)"
                )));
            }
        }
        Ok(())
    };
    require_yesno(node, "indent")?;
    require_yesno(node, "omit-xml-declaration")?;
    require_yesno(node, "byte-order-mark")?;
    require_yesno(node, "escape-uri-attributes")?;
    require_yesno(node, "include-content-type")?;
    require_yesno(node, "undeclare-prefixes")?;
    if !in_forwards_compat_mode() {
        if let Some(v) = read_attribute(node, "standalone") {
            if !matches!(v.trim(), "yes" | "no" | "omit") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:output standalone='{v}' must be 'yes', 'no', or 'omit' (XTSE0020)"
                )));
            }
        }
        if let Some(v) = read_attribute(node, "normalization-form") {
            if !matches!(v.trim(),
                "NFC" | "NFD" | "NFKC" | "NFKD" | "fully-normalized" | "none")
                && !v.trim().starts_with("nmtoken")
            {
                // Spec also permits an NMTOKEN extension value; we
                // accept anything non-empty for those to avoid
                // false positives.
                if v.trim().is_empty() {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:output normalization-form='{v}' is invalid (XTSE0020)"
                    )));
                }
            }
        }
    }
    out.method                 = read_attribute(node, "method").map(str::to_string);
    // XSLT 2.0 §20 / XTSE1570 — an unprefixed method= value must be
    // one of the four built-ins; a prefixed value names an extension
    // method whose prefix must be in scope, and the QName itself
    // must be valid (no double colon, etc.).
    if let Some(m) = &out.method {
        match m.split_once(':') {
            None => if !matches!(m.as_str(),
                "xml" | "html" | "xhtml" | "text") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:output method='{m}' is not one of \
                     xml / html / xhtml / text (XTSE1570)"
                )));
            },
            Some(_) => {
                // Reuse the standard QName parser so a malformed name
                // (`your::xml`) or an unbound prefix surfaces here.
                parse_qname_on(node, m).map_err(|_| XsltError::InvalidStylesheet(
                    format!("xsl:output method='{m}' is not a valid EQName (XTSE1570)")
                ))?;
            }
        }
    }
    out.encoding               = read_attribute(node, "encoding").map(str::to_string);
    out.indent                 = read_attribute(node, "indent").map(parse_yesno);
    out.omit_xml_declaration   = read_attribute(node, "omit-xml-declaration").map(parse_yesno);
    out.standalone             = read_attribute(node, "standalone").map(parse_yesno);
    out.media_type             = read_attribute(node, "media-type").map(str::to_string);
    out.doctype_public         = read_attribute(node, "doctype-public").map(str::to_string);
    out.doctype_system         = read_attribute(node, "doctype-system").map(str::to_string);
    out.version                = read_attribute(node, "version").map(str::to_string);
    if let Some(s) = read_attribute(node, "cdata-section-elements") {
        out.cdata_section_elements = parse_qname_list(node, s)?;
    }
    if let Some(s) = read_attribute(node, "use-character-maps") {
        out.use_character_maps = parse_qname_list(node, s)?;
    }
    Ok(out)
}

/// Compile an `<xsl:character-map name="map" use-character-maps="m1 m2">`
/// declaration into a [`CharacterMap`].  Children must be
/// `<xsl:output-character character="c" string="…"/>` elements;
/// anything else is rejected with a clear diagnostic.
fn compile_character_map(node: &Node) -> Result<CharacterMap, XsltError> {
    let name = require_attr(node, "name", "xsl:character-map")?;
    let qname = parse_qname_on(node, name)?;
    let use_character_maps = match read_attribute(node, "use-character-maps") {
        Some(s) => parse_qname_list(node, s)?,
        None    => Vec::new(),
    };
    let mut mappings: Vec<(char, String)> = Vec::new();
    for child in node.children() {
        if !child.is_element() { continue; }
        if !is_xslt_element(&child) || child.local_name() != "output-character" {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:character-map: unexpected child <{}>; only <xsl:output-character> is allowed",
                child.name()
            )));
        }
        let ch_str = require_attr(&child, "character", "xsl:output-character")?;
        let mut chs = ch_str.chars();
        let ch = chs.next().ok_or_else(|| XsltError::InvalidStylesheet(
            "xsl:output-character 'character' attribute is empty".into()))?;
        if chs.next().is_some() {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:output-character 'character' must be a single character (got {ch_str:?})"
            )));
        }
        let replacement = read_attribute(&child, "string").unwrap_or("").to_string();
        mappings.push((ch, replacement));
    }
    Ok(CharacterMap { name: qname, use_character_maps, mappings })
}

/// Compile a single `xsl:namespace-alias` declaration into
/// `(stylesheet-URI, result-URI)`.  The two `*-prefix` attributes
/// reference prefixes declared on the stylesheet — we resolve each
/// to its URI via ancestor xmlns lookups.  Returns None when
/// either side is "#default" but no default namespace is in
/// scope (the alias is a no-op in that case).
/// Per-attribute copy + inequality operations for a `DecimalFormat`,
/// indexed by the same bit positions as `decimal_format_explicit`.
type DfCopy = fn(&mut crate::format_number::DecimalFormat, &crate::format_number::DecimalFormat);
type DfNeq  = fn(&crate::format_number::DecimalFormat, &crate::format_number::DecimalFormat) -> bool;
const DF_FIELDS: [(u16, DfCopy, DfNeq); 10] = [
    (1 << 0, |d, s| d.decimal_separator  = s.decimal_separator,  |a, b| a.decimal_separator  != b.decimal_separator),
    (1 << 1, |d, s| d.grouping_separator = s.grouping_separator, |a, b| a.grouping_separator != b.grouping_separator),
    (1 << 2, |d, s| d.infinity           = s.infinity.clone(),   |a, b| a.infinity           != b.infinity),
    (1 << 3, |d, s| d.minus_sign         = s.minus_sign,         |a, b| a.minus_sign         != b.minus_sign),
    (1 << 4, |d, s| d.nan                = s.nan.clone(),        |a, b| a.nan                != b.nan),
    (1 << 5, |d, s| d.percent            = s.percent,            |a, b| a.percent            != b.percent),
    (1 << 6, |d, s| d.per_mille          = s.per_mille,          |a, b| a.per_mille          != b.per_mille),
    (1 << 7, |d, s| d.zero_digit         = s.zero_digit,         |a, b| a.zero_digit         != b.zero_digit),
    (1 << 8, |d, s| d.digit              = s.digit,              |a, b| a.digit              != b.digit),
    (1 << 9, |d, s| d.pattern_separator  = s.pattern_separator,  |a, b| a.pattern_separator  != b.pattern_separator),
];

/// Merge one module's `decimal-format` declaration (already merged
/// within its own module, with explicit-attribute `mask` and same-module
/// `src_conflict` bits) into `acc` at import precedence `prec`.
///
/// Modules are visited highest-precedence-first, so per attribute: a
/// higher precedence overwrites and resolves any clash; an equal
/// precedence with a different value records a conflict; a lower
/// precedence is ignored (XSLT 2.0 §16.4.2).
fn merge_decimal_format_into(
    acc: &mut StylesheetAst, key: &str,
    df: &crate::format_number::DecimalFormat, mask: u16, src_conflict: u16, prec: i32,
) {
    let entry = acc.decimal_formats.entry(key.to_string())
        .or_insert_with(crate::format_number::DecimalFormat::default);
    let precs = acc.decimal_format_attr_prec.entry(key.to_string())
        .or_insert([i32::MIN; 10]);
    let exp = acc.decimal_format_explicit.entry(key.to_string()).or_insert(0);
    let conf = acc.decimal_format_conflicts.entry(key.to_string()).or_insert(0);
    for (idx, (bit, copy, neq)) in DF_FIELDS.iter().enumerate() {
        if mask & bit == 0 { continue; }
        let inc_conflicted = src_conflict & bit != 0;
        if prec > precs[idx] {
            copy(entry, df);
            precs[idx] = prec;
            *exp |= bit;
            if inc_conflicted { *conf |= bit; } else { *conf &= !bit; }
        } else if prec == precs[idx] && (neq(entry, df) || inc_conflicted) {
            *conf |= bit;
        }
    }
}

/// Final XTSE1290 gate: after every module has merged, any decimal-format
/// attribute still carrying a conflict bit was set to differing values at
/// the highest import precedence with no higher declaration to break the
/// tie.
fn finalize_decimal_format_conflicts(ast: &StylesheetAst) -> Result<(), XsltError> {
    for (key, &conflict) in &ast.decimal_format_conflicts {
        if conflict != 0 {
            let name = if key.is_empty() { "(default)".to_string() } else { key.clone() };
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:decimal-format {name} has conflicting declarations of \
                 equal import precedence (XTSE1290)"
            )));
        }
    }
    Ok(())
}

/// Parse a single `<xsl:decimal-format>` declaration into the
/// stylesheet's `decimal_formats` table.  Absent attributes fall
/// back to the XSLT 1.0 §12.3 defaults (which match
/// [`crate::format_number::DecimalFormat::default`]).
///
/// The unnamed default decimal-format is keyed at `""`; named
/// ones at their raw `name` attribute text.  `format-number()`'s
/// third argument is compared by the same string form.
fn compile_decimal_format(node: &Node, ast: &mut StylesheetAst) -> Result<(), XsltError> {
    use crate::format_number::DecimalFormat;
    // XSLT 2.0 §16.4 — the closed attribute set; `exponent-separator`
    // was introduced in XSLT 3.0 and is XTSE0090 in a 2.0 host.
    validate_xslt_only_attributes(node, "xsl:decimal-format",
        &["name", "decimal-separator", "grouping-separator", "infinity",
          "minus-sign", "NaN", "percent", "per-mille",
          "zero-digit", "digit", "pattern-separator"])?;
    validate_must_be_empty(node, "xsl:decimal-format")?;
    // Track which separator characters were explicitly set by the
    // user.  XSLT 2.0 §16.4.2 says XTSE1300 conflicts are evaluated
    // on the EFFECTIVE format after merging same-named declarations
    // — a single declaration that only sets `decimal-separator=","`
    // shouldn't fail validation against the default
    // `grouping-separator=","`, because another declaration with
    // the same name may override grouping.
    let mut df = DecimalFormat::default();
    let mut set_decimal_sep   = None::<char>;
    let mut set_grouping_sep  = None::<char>;
    let mut set_minus         = None::<char>;
    let mut set_percent       = None::<char>;
    let mut set_per_mille     = None::<char>;
    let mut set_zero_digit    = None::<char>;
    let mut set_digit         = None::<char>;
    let mut set_pattern_sep   = None::<char>;
    if let Some(v) = read_attribute(node, "decimal-separator")
        { let c = first_char(v, "decimal-separator")?; df.decimal_separator = c;
          set_decimal_sep = Some(c); }
    if let Some(v) = read_attribute(node, "grouping-separator")
        { let c = first_char(v, "grouping-separator")?; df.grouping_separator = c;
          set_grouping_sep = Some(c); }
    if let Some(v) = read_attribute(node, "infinity")           { df.infinity           = v.to_string(); }
    if let Some(v) = read_attribute(node, "minus-sign")
        { let c = first_char(v, "minus-sign")?; df.minus_sign = c; set_minus = Some(c); }
    if let Some(v) = read_attribute(node, "NaN")                { df.nan                = v.to_string(); }
    if let Some(v) = read_attribute(node, "percent")
        { let c = first_char(v, "percent")?; df.percent = c; set_percent = Some(c); }
    if let Some(v) = read_attribute(node, "per-mille")
        { let c = first_char(v, "per-mille")?; df.per_mille = c; set_per_mille = Some(c); }
    if let Some(v) = read_attribute(node, "zero-digit")
        { let c = first_char(v, "zero-digit")?;
          // XSLT 2.0 §16.4.1 / XTSE1295 — the zero-digit character
          // must be the *first* (numeric-value zero) digit of some
          // Unicode digit set.  Anything else (`2`, `a`, `!`) is
          // a static error.
          if !is_unicode_zero_digit(c) {
              return Err(XsltError::InvalidStylesheet(format!(
                  "xsl:decimal-format zero-digit='{c}' is not a valid \
                   zero-digit (XTSE1295)"
              )));
          }
          df.zero_digit = c; set_zero_digit = Some(c); }
    if let Some(v) = read_attribute(node, "digit")
        { let c = first_char(v, "digit")?; df.digit = c; set_digit = Some(c); }
    if let Some(v) = read_attribute(node, "pattern-separator")
        { let c = first_char(v, "pattern-separator")?; df.pattern_separator = c;
          set_pattern_sep = Some(c); }
    // XTSE1300 — every role-bearing character in a decimal-format
    // must be distinct.  Only check pairs where BOTH characters
    // were explicitly set (or where exactly one was set and it
    // collides with the OTHER role's default that the same
    // declaration didn't override — caught below by the
    // explicit-vs-default pair check).
    let role_chars: [(&str, Option<char>, char); 8] = [
        ("decimal-separator",  set_decimal_sep,  df.decimal_separator),
        ("grouping-separator", set_grouping_sep, df.grouping_separator),
        ("minus-sign",         set_minus,        df.minus_sign),
        ("percent",            set_percent,      df.percent),
        ("per-mille",          set_per_mille,    df.per_mille),
        ("zero-digit",         set_zero_digit,   df.zero_digit),
        ("digit",              set_digit,        df.digit),
        ("pattern-separator",  set_pattern_sep,  df.pattern_separator),
    ];
    for (i, (n1, set1, c1)) in role_chars.iter().enumerate() {
        for (n2, set2, c2) in &role_chars[i + 1..] {
            // Only flag conflicts where BOTH characters were
            // explicitly authored.  Same-named decimal-format
            // declarations may merge later (XSLT 2.0 §16.4.2) and
            // override a default that would otherwise look like
            // a conflict against an explicit setting.
            if set1.is_none() || set2.is_none() { continue; }
            if c1 == c2 {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:decimal-format: {n1} and {n2} both use {c1:?} (XTSE1300)"
                )));
            }
        }
    }
    // Resolve any prefix in `name=` so the table key is the QName's
    // Clark form (`{uri}local`).  format-number's third argument
    // resolves the same way at call time, so a name declared with
    // one prefix still matches a lookup that uses a different
    // prefix bound to the same URI.
    let key = match read_attribute(node, "name") {
        Some(raw) => {
            // The name= attribute is a QName, not an AVT — a `{…}`
            // value (or any other non-QName lexical) is XTSE0020.
            let qname = parse_qname_on(node, raw)?;
            reject_reserved_name(&qname, "xsl:decimal-format")?;
            decimal_format_key(node, raw)
        }
        None      => String::new(),
    };
    // Compute this declaration's bitmask of explicitly-set attributes
    // (see [`StylesheetAst::decimal_format_explicit`] for bit layout).
    let mut mask: u16 = 0;
    if set_decimal_sep.is_some()                          { mask |= 1 << 0; }
    if set_grouping_sep.is_some()                         { mask |= 1 << 1; }
    if read_attribute(node, "infinity").is_some()         { mask |= 1 << 2; }
    if set_minus.is_some()                                { mask |= 1 << 3; }
    if read_attribute(node, "NaN").is_some()              { mask |= 1 << 4; }
    if set_percent.is_some()                              { mask |= 1 << 5; }
    if set_per_mille.is_some()                            { mask |= 1 << 6; }
    if set_zero_digit.is_some()                           { mask |= 1 << 7; }
    if set_digit.is_some()                                { mask |= 1 << 8; }
    if set_pattern_sep.is_some()                          { mask |= 1 << 9; }
    // XSLT 2.0 §16.4.2 / XTSE1290 — declarations with the same name
    // (and same import precedence) must agree on every attribute
    // they BOTH set explicitly.  Non-overlapping attribute sets
    // merge field-by-field.  In forwards-compat mode skip the check
    // entirely.  Import-precedence overrides are still handled in
    // [`compile_with_imports`]; here we only need to merge entries
    // arriving from the same module.
    if let Some(existing) = ast.decimal_formats.get(&key) {
        let prev_mask = ast.decimal_format_explicit.get(&key).copied().unwrap_or(0);
        if !in_forwards_compat_mode() {
            // Two declarations in the SAME module share import precedence;
            // an attribute they both set to different values is a conflict.
            // Record it rather than erroring now — a higher-precedence
            // module may still override the attribute (XSLT 2.0 §16.4.2),
            // so the decision is deferred to `finalize_decimal_formats`.
            let overlap = mask & prev_mask;
            let attrs: [(u16, &dyn Fn(&DecimalFormat, &DecimalFormat) -> bool); 10] = [
                (1 << 0, &|a, b| a.decimal_separator  != b.decimal_separator),
                (1 << 1, &|a, b| a.grouping_separator != b.grouping_separator),
                (1 << 2, &|a, b| a.infinity           != b.infinity),
                (1 << 3, &|a, b| a.minus_sign         != b.minus_sign),
                (1 << 4, &|a, b| a.nan                != b.nan),
                (1 << 5, &|a, b| a.percent            != b.percent),
                (1 << 6, &|a, b| a.per_mille          != b.per_mille),
                (1 << 7, &|a, b| a.zero_digit         != b.zero_digit),
                (1 << 8, &|a, b| a.digit              != b.digit),
                (1 << 9, &|a, b| a.pattern_separator  != b.pattern_separator),
            ];
            let mut conflict = 0u16;
            for (bit, neq) in &attrs {
                if overlap & bit != 0 && neq(existing, &df) { conflict |= bit; }
            }
            if conflict != 0 {
                *ast.decimal_format_conflicts.entry(key.clone()).or_insert(0) |= conflict;
            }
        }
        // Merge: keep the existing's explicit fields; overlay the new
        // declaration's explicit-only fields onto a fresh default.
        let mut merged = existing.clone();
        if mask & (1 << 0) != 0 { merged.decimal_separator  = df.decimal_separator;  }
        if mask & (1 << 1) != 0 { merged.grouping_separator = df.grouping_separator; }
        if mask & (1 << 2) != 0 { merged.infinity           = df.infinity.clone();   }
        if mask & (1 << 3) != 0 { merged.minus_sign         = df.minus_sign;         }
        if mask & (1 << 4) != 0 { merged.nan                = df.nan.clone();        }
        if mask & (1 << 5) != 0 { merged.percent            = df.percent;            }
        if mask & (1 << 6) != 0 { merged.per_mille          = df.per_mille;          }
        if mask & (1 << 7) != 0 { merged.zero_digit         = df.zero_digit;         }
        if mask & (1 << 8) != 0 { merged.digit              = df.digit;              }
        if mask & (1 << 9) != 0 { merged.pattern_separator  = df.pattern_separator;  }
        ast.decimal_formats.insert(key.clone(), merged);
        ast.decimal_format_explicit.insert(key, prev_mask | mask);
    } else {
        ast.decimal_formats.insert(key.clone(), df);
        ast.decimal_format_explicit.insert(key, mask);
    }
    Ok(())
}

/// Convert a `xsl:decimal-format name=` (or `format-number(name)`
/// third arg) into the Clark-form key we store in the table.  When
/// `raw` has no prefix the key is just the local name; when it
/// carries a prefix we resolve it against the in-scope namespaces.
/// Unbound prefixes fall back to the raw string so the lookup at
/// least surfaces a stable not-found error rather than silently
/// matching the wrong format.
pub(crate) fn decimal_format_key(node: &Node, raw: &str) -> String {
    let (prefix, local) = match raw.split_once(':') {
        Some(pl) => pl,
        None     => return raw.to_string(),
    };
    let mut cur = Some(node);
    while let Some(n) = cur {
        for (p, href) in n.ns_declarations() {
            if p == Some(prefix) {
                return format!("{{{href}}}{local}");
            }
        }
        cur = n.parent.get();
    }
    raw.to_string()
}

/// Parse a single-codepoint attribute value into `char`.  XSLT 1.0
/// §12.3 requires these to be one character; we treat them as
/// Unicode scalars (Rust's `char`).
fn first_char(s: &str, attr_name: &str) -> Result<char, XsltError> {
    let mut chars = s.chars();
    let first = chars.next().ok_or_else(|| XsltError::InvalidStylesheet(format!(
        "xsl:decimal-format '{attr_name}' attribute is empty"
    )))?;
    if chars.next().is_some() {
        return Err(XsltError::InvalidStylesheet(format!(
            "xsl:decimal-format '{attr_name}' must be a single character (got {s:?})"
        )));
    }
    Ok(first)
}

fn compile_namespace_alias(
    node: &Node,
) -> Result<Option<(String, String, Option<String>)>, XsltError> {
    let style_prefix  = require_attr(node, "stylesheet-prefix", "xsl:namespace-alias")?;
    let result_prefix = require_attr(node, "result-prefix",     "xsl:namespace-alias")?;
    // XSLT 1.0 §7.1.1 — `#default` resolves to the in-scope default
    // namespace, or the null namespace ("") when none is declared
    // at the alias-source location.  Treat None from
    // resolve_alias_prefix as the null namespace rather than
    // silently dropping the alias entry.
    let style_uri  = resolve_alias_prefix(node, style_prefix)?.unwrap_or_default();
    let result_uri = resolve_alias_prefix(node, result_prefix)?.unwrap_or_default();
    // `#default` means "result becomes the no-prefix default xmlns";
    // anything else carries the literal prefix to the emitted name.
    let result_prefix_owned: Option<String> = if result_prefix == "#default" {
        None
    } else {
        Some(result_prefix.to_owned())
    };
    Ok(Some((style_uri, result_uri, result_prefix_owned)))
}

/// Resolve `prefix` (which may be the special `#default` token)
/// against ancestor xmlns declarations.  Returns the URI string,
/// or None if `#default` was used and no default namespace is in
/// scope at `node`'s position.
fn resolve_alias_prefix(node: &Node, prefix: &str) -> Result<Option<String>, XsltError> {
    // XML Namespaces 1.0 §3 — the prefixes `xml` and `xmlns` are
    // implicitly bound to fixed URIs and need no explicit
    // declaration; namespace-alias may name them directly.
    match prefix {
        "xml"   => return Ok(Some("http://www.w3.org/XML/1998/namespace".to_string())),
        "xmlns" => return Ok(Some("http://www.w3.org/2000/xmlns/".to_string())),
        _       => {}
    }
    let target = if prefix == "#default" { None } else { Some(prefix) };
    let mut cur = Some(node);
    while let Some(n) = cur {
        for (p, href) in n.ns_declarations() {
            if p == target {
                return Ok(Some(href.to_string()));
            }
        }
        cur = n.parent.get();
    }
    if target.is_some() {
        return Err(XsltError::InvalidStylesheet(format!(
            "xsl:namespace-alias references undeclared prefix '{prefix}'"
        )));
    }
    Ok(None)
}

fn collect_whitespace_rules(
    node: &Node, strip: bool, out: &mut Vec<WhitespaceRule>,
) -> Result<(), XsltError> {
    let who = if strip { "xsl:strip-space" } else { "xsl:preserve-space" };
    validate_must_be_empty(node, who)?;
    let s = require_attr(node, "elements", who)?;
    let default_uri = xpath_default_namespace_for(node);
    for tok in s.split_whitespace() {
        // `elements=` accepts NameTests (XSLT 1.0 §3.4 / 2.0 §4.4):
        // a NCName, `prefix:NCName`, `*`, `prefix:*`, or `*:NCName`.
        // The wildcard forms aren't QNames so parse_qname_on can't
        // handle them; rewrite them into a sentinel QName that the
        // matching path already understands ("*" / "prefix:*" via
        // wildcard local; "*:local" via empty-prefix wildcard).
        let mut q = parse_name_test_token(node, tok)?;
        // XSLT 2.0 §5.1.1 — an unprefixed element NameTest in the
        // `elements=` list uses the in-scope xpath-default-namespace,
        // not the null namespace.
        if q.prefix.is_none() && q.local != "*" && q.uri.is_empty() {
            if let Some(uri) = &default_uri {
                q.uri = uri.clone();
            }
        }
        // Precedence is stamped after compilation in
        // [`compile_with_imports`]; default to the top-level value
        // here so a standalone (no-import) compile still sees a
        // consistent precedence.
        out.push(if strip {
            WhitespaceRule::Strip(q, TOP_LEVEL_IMPORT_PRECEDENCE)
        } else {
            WhitespaceRule::Preserve(q, TOP_LEVEL_IMPORT_PRECEDENCE)
        });
    }
    Ok(())
}

// ── instruction dispatch ─────────────────────────────────────────

fn compile_instr_into(node: &Node, out: &mut Vec<Instr>) -> Result<(), XsltError> {
    // Text / CData → emit as literal text.  Comments and PIs in
    // the stylesheet are XSLT-source comments, not result-tree
    // content; the spec says they're ignored during transformation.
    match node.kind {
        NodeKind::Text | NodeKind::CData => {
            let text = node.content();
            // XSLT 3.0 §5.4.2 — when `[xsl:]expand-text` is in scope, a
            // literal text node is a *text value template*: `{expr}`
            // substitutions are evaluated (like an AVT) and `{{`/`}}`
            // are literal braces.  Each `{expr}` becomes the equivalent
            // of `xsl:value-of select="expr"` (sequence items joined by
            // a single space, XSLT 2.0 §11.4.4).
            if expand_text_in_scope(node) && (text.contains('{') || text.contains('}')) {
                for part in avt(node, text)?.parts {
                    match part {
                        AvtPart::Literal(s) => {
                            if !s.is_empty() {
                                out.push(Instr::LiteralText { text: s, dose: false });
                            }
                        }
                        AvtPart::Expr(e) => out.push(Instr::ValueOf {
                            select: e,
                            dose: false,
                            separator: Some(Avt::literal(" ")),
                        }),
                    }
                }
                return Ok(());
            }
            out.push(Instr::LiteralText {
                text: text.to_string(),
                dose: false,
            });
            return Ok(());
        }
        NodeKind::Comment | NodeKind::Pi => return Ok(()),
        NodeKind::Element => {}
        _ => return Ok(()),
    }

    // XSLT 2.0 §3.10.2 `use-when` on instructions inside templates.
    // The attribute is unprefixed on XSLT elements (`use-when="…"`)
    // and `xsl:use-when="…"` on literal result elements; pick the
    // right qualifier based on the host element's namespace.
    let use_when_attr = if is_xslt_element(node) {
        read_attribute(node, "use-when")
    } else {
        read_xsl_attribute(node, "use-when")
    };
    if let Some(uw) = use_when_attr {
        if !evaluate_use_when_at(uw, Some(node))? {
            return Ok(());
        }
    }

    if !is_xslt_element(node) {
        // Literal result element.
        out.push(compile_literal_element(node)?);
        return Ok(());
    }

    // XSLT instruction.
    let name = node.local_name();
    let instr = match name {
        "apply-templates" => compile_apply_templates(node)?,
        "apply-imports"   => {
            // XSLT 1.0 §5.6 / XSLT 2.0 §9.4 — `xsl:apply-imports`
            // has an empty content model in 1.0, and only allows
            // `xsl:with-param` children in 2.0.  Anything else is
            // XTSE0010.  No attributes are allowed at all (XTSE0090).
            validate_xslt_only_attributes(node, "xsl:apply-imports", &[])?;
            for child in node.children() {
                if !child.is_element() { continue; }
                let allowed = is_xslt_2_0_compile()
                    && is_xslt_element(child)
                    && child.local_name() == "with-param";
                if !allowed {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:apply-imports may not contain <{}> (XTSE0010)",
                        child.name(),
                    )));
                }
            }
            let (_, with_params) = collect_sort_and_with_params(node)?;
            Instr::ApplyImports { with_params }
        }
        "next-match" => {
            let (_, with_params) = collect_sort_and_with_params(node)?;
            Instr::NextMatch { with_params }
        }
        "call-template"   => compile_call_template(node)?,
        "choose"          => compile_choose(node)?,
        "if"              => compile_if(node)?,
        "for-each"        => compile_for_each(node)?,
        "value-of"        => compile_value_of(node)?,
        "copy"            => compile_copy(node)?,
        "copy-of"         => compile_copy_of(node)?,
        "element"         => compile_element(node)?,
        "attribute"       => compile_attribute(node)?,
        "text"            => compile_text(node)?,
        "comment"         => {
            reject_select_with_content(node, "xsl:comment", "XTSE0940", false)?;
            Instr::Comment {
                select: read_attribute(node, "select")
                    .map(|s| parse_xpath_at(node, s).map_err(XsltError::from))
                    .transpose()?,
                body:   compile_body(node)?,
            }
        },
        "processing-instruction" => {
            reject_select_with_content(node, "xsl:processing-instruction", "XTSE0880", false)?;
            Instr::ProcessingInstruction {
                name: avt(node, require_attr(node, "name", "xsl:processing-instruction")?)?,
                select: read_attribute(node, "select")
                    .map(|s| parse_xpath_at(node, s).map_err(XsltError::from))
                    .transpose()?,
                body: compile_body(node)?,
            }
        },
        "number"          => compile_number(node)?,
        "variable"        => Instr::Variable(compile_variable(node)?),
        "message"         => {
            // Validate select/terminate/message attribute set.
            validate_xslt_only_attributes(node, "xsl:message",
                &["select", "terminate"])?;
            let term_attr = read_attribute(node, "terminate");
            let term_avt = term_attr.map(|s| avt(node, s)).transpose()?;
            // XSLT 2.0 §17.1 — when `terminate=` has no AVT braces
            // (i.e. it's a literal value), it must be exactly
            // "yes" or "no".  Catch the static error at compile so
            // tests like terminate="NO" surface XTSE0020.
            if let Some(a) = &term_avt {
                if a.is_literal() {
                    let mut lit = String::new();
                    for p in &a.parts {
                        if let crate::ast::AvtPart::Literal(s) = p { lit.push_str(s); }
                    }
                    if !matches!(lit.trim(), "yes" | "no") {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:message terminate='{lit}' must be 'yes' or 'no' \
                             (XTSE0020)"
                        )));
                    }
                }
            }
            Instr::Message {
                terminate: term_avt,
                body:      compile_body(node)?,
            }
        },
        "fallback"        => {
            // XSLT 2.0 §3.6 — xsl:fallback only takes the generic
            // attributes (use-when, version, …); flag XSLT-namespaced
            // attrs like `xsl:use-when` via the standard validator.
            validate_xslt_only_attributes(node, "xsl:fallback", &[])?;
            Instr::Fallback { body: compile_body(node)? }
        }
        // XSLT 2.0 `<xsl:sequence select="…"/>` — only recognised in
        // 2.0 mode; in 1.0 mode it falls through to the
        // forwards-compatible "unknown instruction" handler.
        "sequence" => {
            // XSLT 2.0 §7.1 — xsl:sequence takes only `select=`.
            // The `as=` attribute permitted in early drafts was
            // removed; presence is XTSE0090.
            validate_xslt_only_attributes(node, "xsl:sequence", &["select"])?;
            let sel = require_attr(node, "select", "xsl:sequence")?;
            // XSLT 2.0 §7.1 / XTSE0010 — the element body must be
            // empty except for optional xsl:fallback children.  Any
            // other content (real instruction, LRE, non-whitespace
            // text) is a static error.
            for child in node.children() {
                if child.is_element() {
                    let is_fallback = is_xslt_element(child)
                        && child.local_name() == "fallback";
                    if !is_fallback {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:sequence body may not contain <{}> \
                             (only xsl:fallback is allowed — XTSE0010)",
                            child.name()
                        )));
                    }
                } else if is_text_like(child) && !child.content().trim().is_empty() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:sequence body may not contain text content \
                         (XTSE0010)".into()));
                }
            }
            Instr::Sequence { select: parse_xpath_at(node, sel).map_err(XsltError::from)? }
        }
        "map"            => Instr::Map { body: compile_body(node)? },
        "map-entry"      => {
            let key = require_attr(node, "key", "xsl:map-entry")?;
            let key = parse_xpath_at(node, key).map_err(XsltError::from)?;
            let select = match read_attribute(node, "select") {
                Some(s) => Some(parse_xpath_at(node, &s).map_err(XsltError::from)?),
                None => None,
            };
            let body = if select.is_some() { Vec::new() } else { compile_body(node)? };
            Instr::MapEntry { key, select, body }
        }
        "for-each-group" => compile_for_each_group(node)?,
        "source-document" | "stream" => compile_source_document(node)?,
        "fork"           => Instr::Fork { body: compile_body(node)? },
        "where-populated" => Instr::WherePopulated { body: compile_body(node)? },
        "on-empty"        => Instr::OnEmpty { body: select_or_body(node, "xsl:on-empty")? },
        "on-non-empty"    => Instr::OnNonEmpty { body: select_or_body(node, "xsl:on-non-empty")? },
        "evaluate"       => compile_evaluate(node)?,
        "merge"          => compile_merge(node)?,
        "analyze-string" => compile_analyze_string(node)?,
        "perform-sort"   => compile_perform_sort(node)?,
        "document"       => Instr::Document { body: compile_body(node)? },
        "namespace"      => compile_namespace_instr(node)?,
        "try"            => compile_try(node)?,
        // XSLT 3.0 instructions — only recognised when the stylesheet
        // declares a version greater than 2.0.  In a 2.0 stylesheet
        // they are unknown elements and (outside forwards-compat) a
        // static error, which is what the W3C suite expects.
        "iterate"        if in_forwards_compat_mode() => compile_iterate(node)?,
        "next-iteration" if in_forwards_compat_mode() => compile_next_iteration(node)?,
        "break"          if in_forwards_compat_mode() => compile_break(node)?,
        // xsl:result-document — XSLT 2.0 §19.1.  With `href=` the body
        // becomes a secondary result document written to the resolved
        // URI; without `href=` (or with an empty one) it targets the
        // principal output URI, which is valid only when it is the sole
        // writer of that destination (otherwise XTRE1495 at run time).
        "result-document" => {
            // XTSE0020 — the output-property attributes inherit
            // xsl:output's value grammar: yes/no booleans and a numeric
            // html-version.  An invalid literal is a static error.
            for a in ["standalone", "omit-xml-declaration", "indent",
                      "include-content-type", "undeclare-prefixes",
                      "escape-uri-attributes", "byte-order-mark"] {
                if let Some(v) = read_attribute(node, a) {
                    parse_yesno_strict(v, "xsl:result-document", a)?;
                }
            }
            if let Some(v) = read_attribute(node, "html-version") {
                if v.trim().parse::<f64>().is_err() {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:result-document html-version='{v}' must be numeric (XTSE0020)")));
                }
            }
            let href = avt(node, read_attribute(node, "href").unwrap_or_default())?;
            let format = read_attribute(node, "format")
                .map(|s| avt(node, s))
                .transpose()?;
            // Capture the in-scope namespaces on this element so a
            // runtime AVT expansion of `format=` can be QName-validated
            // (XTDE1460) without re-walking the source tree.  Only
            // populated when `format=` is present so unused branches
            // pay no storage.
            let format_namespaces: Vec<(Option<String>, String)> = if format.is_some() {
                collect_in_scope_namespaces(node)
            } else {
                Vec::new()
            };
            Instr::ResultDocument {
                href, format, format_namespaces, body: compile_body(node)?,
            }
        }

        // `xsl:sort` / `xsl:with-param` / `xsl:param` / `xsl:when`
        // / `xsl:otherwise` / `xsl:catch` only appear as children
        // of specific parents; finding one here is a structural
        // error.
        "sort" | "with-param" | "param" | "when" | "otherwise" | "catch" => {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:{name} is misplaced (must be a child of a specific parent)"
            )));
        }

        // Top-level-only declarations finding themselves inside a
        // template body — XTSE0010 again (covers tests like
        // error-0010aa which nests xsl:template inside xsl:template).
        "template" | "import" | "include" | "key" | "output"
        | "strip-space" | "preserve-space" | "namespace-alias"
        | "decimal-format" | "attribute-set" | "function"
        | "character-map" | "import-schema" => {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:{name} must appear at the top level of the stylesheet (XTSE0010)"
            )));
        }

        other => {
            // XSLT 2.0 §3.5 — outside forwards-compat mode (i.e.,
            // when the stylesheet's `version` is at or below the
            // processor's supported version), an unknown XSLT-
            // namespace element is XTSE0010 even if no template
            // evaluation ever reaches it.  In forwards-compat mode
            // we defer to runtime so the `xsl:fallback` branch can
            // substitute behaviour (XSLT 1.0 §15).
            //
            // The `version` attribute on any XSLT-namespace ancestor
            // (or `xsl:version` on a literal-result-element ancestor)
            // opens a forwards-compat scope for its sub-tree when the
            // declared version exceeds the processor's supported
            // version — the global thread-local only tracks the
            // stylesheet-root version, so we walk ancestors here.
            if !in_forwards_compat_mode() && !ancestor_enables_forwards_compat(node) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:{other} is not a recognised XSLT instruction (XTSE0010)"
                )));
            }
            let mut fallback: Vec<Instr> = Vec::new();
            for child in node.children() {
                if child.is_element()
                    && is_xslt_element(child)
                    && child.local_name() == "fallback"
                {
                    fallback.extend(compile_body(child)?);
                }
            }
            Instr::Unsupported { name: other.to_string(), fallback }
        }
    };
    out.push(instr);
    Ok(())
}

fn compile_body(node: &Node) -> Result<Vec<Instr>, XsltError> {
    // XSLT 1.0 §3.4 strips whitespace-only text nodes from stylesheet
    // bodies, but XML parsers may split a logical character run into
    // (text, CDATA, text…) — without that knowledge, the WHITESPACE
    // around a CDATA gets dropped even though the run as a whole has
    // non-whitespace content.  Pre-scan each adjacent text/CDATA run
    // and treat the whole run as one for the "is this whitespace-
    // only" decision.
    let children: Vec<&Node> = node.children().collect();
    let keep_run = run_keep_mask(&children);
    let mut out = Vec::new();
    for (i, child) in children.iter().enumerate() {
        if child.is_element() {
            compile_instr_into(child, &mut out)?;
        } else if is_text_like(child) {
            if keep_run[i] { compile_instr_into(child, &mut out)?; }
        }
    }
    Ok(out)
}

/// Per-child boolean: should this text/CDATA child be kept under the
/// XSLT 1.0 §3.4 whitespace-strip rules?  Each run of adjacent
/// text-or-CDATA (with intervening comments / PIs allowed — they're
/// invisible after stripping) is treated as one logical text node;
/// the whole run is preserved iff ANY of its character members has a
/// non-whitespace character.
fn run_keep_mask(children: &[&Node]) -> Vec<bool> {
    let mut keep = vec![false; children.len()];
    let mut i = 0;
    while i < children.len() {
        // Skip leading non-character siblings (elements terminate
        // any prior run; comments/PIs don't, but they don't START
        // one either — they're only spanned WITHIN a run).
        if !is_text_like(children[i]) { i += 1; continue; }
        // Build the run: text/CDATA contiguous, optionally
        // bridged by comment/PI siblings whose only role is to be
        // dropped at compile time.
        let start = i;
        let mut last_text = i;
        i += 1;
        while i < children.len() {
            if is_text_like(children[i]) {
                last_text = i;
                i += 1;
            } else if matches!(children[i].kind, NodeKind::Comment | NodeKind::Pi) {
                i += 1;
            } else {
                break;
            }
        }
        // Trim the run to end at the last text/CDATA seen — any
        // trailing comment/PI doesn't extend the run beyond it.
        let end = last_text + 1;
        let run_significant = (start..end).any(|j| {
            is_text_like(children[j])
                && !crate::whitespace::is_xslt_whitespace_only(children[j].content())
        });
        if run_significant {
            for j in start..end {
                if is_text_like(children[j]) { keep[j] = true; }
            }
        } else {
            // Whitespace-only run: keep nothing UNLESS xml:space=preserve
            // would have kept it under the original per-node check.
            for j in start..end {
                if is_text_like(children[j]) && is_significant_text(children[j]) {
                    keep[j] = true;
                }
            }
        }
    }
    keep
}

fn is_text_like(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Text | NodeKind::CData)
}

/// True iff `node` has any element / text / CData / PI / comment
/// child (i.e. is not completely empty).  Used by XSLT 2.0 §11.5
/// to detect the "no select, no body" form of xsl:value-of.
fn has_any_child(node: &Node) -> bool {
    node.children().any(|c| matches!(c.kind,
        NodeKind::Element | NodeKind::Text | NodeKind::CData
        | NodeKind::Pi | NodeKind::Comment
    ) && !(is_text_like(c) && c.content().chars().all(|ch| ch.is_whitespace())))
}

/// True iff `node` has any element / non-whitespace-only text child.
/// Used to enforce XSLT 2.0 §11.5 / XTSE0870 — when select= is
/// present, an xsl:value-of body must be empty (or whitespace-only).
fn has_non_whitespace_child(node: &Node) -> bool {
    node.children().any(|c| match c.kind {
        NodeKind::Element | NodeKind::Pi | NodeKind::Comment => true,
        NodeKind::Text | NodeKind::CData =>
            !c.content().chars().all(|ch| ch.is_whitespace()),
        _ => false,
    })
}

// ── specific instructions ────────────────────────────────────────

fn compile_apply_templates(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:apply-templates",
        &["select", "mode"])?;
    let select = read_attribute(node, "select")
        .map(|s| parse_xpath_at(node, s)).transpose().map_err(XsltError::from)?;
    // XSLT 2.0 §6.7 reserves `#current` and `#default` as `mode=`
    // values.  `#default` resolves to "no mode" (same as omitting
    // the attribute); `#current` defers to apply-time and signals
    // through `mode_current`.
    let mode_attr = read_attribute(node, "mode").map(str::trim);
    let (mode, mode_current) = match mode_attr {
        None | Some("#default")              => (None, false),
        Some("#current") if is_xslt_2_0_compile() => (None, true),
        Some(s) => (Some(parse_qname_on(node, s)?), false),
    };
    // XSLT 1.0 §5.4 / XSLT 2.0 §6.4 — apply-templates' children must
    // be xsl:sort or xsl:with-param.  Anything else is XTSE0010.
    validate_xslt_only_children(node, "xsl:apply-templates", &["sort", "with-param"])?;
    let (sort, with_params) = collect_sort_and_with_params(node)?;
    Ok(Instr::ApplyTemplates { select, mode, sort, with_params, mode_current })
}

fn compile_call_template(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:call-template", &["name"])?;
    let name = required_qname_attr(node, "name", "xsl:call-template")?;
    // XSLT 1.0 §6 / XSLT 2.0 §10.1 — the only legal children of
    // xsl:call-template are xsl:with-param elements (and XSLT 2.0
    // adds xsl:fallback, but only for forwards-compat scenarios on
    // unknown instructions, not on call-template itself).  Anything
    // else is XTSE0010.
    validate_xslt_only_children(node, "xsl:call-template", &["with-param"])?;
    let (_, with_params) = collect_sort_and_with_params(node)?;
    Ok(Instr::CallTemplate { name, with_params })
}

/// Reject unprefixed attributes on the XSLT element `node` that
/// aren't in `allowed`.  XSLT 2.0 §3.6: any unprefixed attribute on
/// an XSLT-namespace element outside the spec-defined set is
/// XTSE0090.  Prefixed (foreign-namespace) attributes are
/// extension data and pass through.  In forwards-compatible mode
/// (caller decides) the check should be skipped — pass an empty
/// `allowed` if so.
/// Reject any element / non-whitespace text content on an XSLT
/// element whose spec content model is empty (e.g. xsl:include,
/// xsl:import, xsl:output, xsl:character).  Comments and PIs are
/// always allowed; whitespace-only text is ignored to tolerate the
/// `<xsl:include href="…">\n  </xsl:include>` shape produced by
/// pretty-printers.  Forwards-compat mode skips the check entirely
/// so future-version stylesheets can carry extension content.
fn validate_must_be_empty(node: &Node, who: &str) -> Result<(), XsltError> {
    if in_forwards_compat_mode() { return Ok(()); }
    // XSLT 2.0 §3.6 / XTSE0260: even xml:space="preserve" whitespace
    // is forbidden inside an "empty" XSLT element.  Whitespace text
    // otherwise gets stripped by the stylesheet's whitespace rules so
    // it never reaches here in the common case.
    let xml_space_preserve = node.attributes().any(|a|
        a.local_name() == "space"
            && a.namespace.get().and_then(|n| n.prefix()) == Some("xml")
            && a.value() == "preserve");
    for child in node.children() {
        if child.is_element() {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} must be empty — found <{}> (XTSE0260)",
                child.name(),
            )));
        }
        if is_text_like(child)
            && (xml_space_preserve || !child.content().trim().is_empty())
        {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} must be empty — found text content (XTSE0260)"
            )));
        }
    }
    Ok(())
}

fn validate_xslt_only_attributes(
    node: &Node, who: &str, allowed: &[&str],
) -> Result<(), XsltError> {
    if in_forwards_compat_mode() { return Ok(()); }
    // XSLT 2.0 §3.6 — these attributes apply to every XSLT element
    // (and to literal result elements via the `xsl:` prefix).  The
    // per-element validator only lists element-specific attributes;
    // the generics are always allowed.
    const GENERIC_XSLT_ATTRS: &[&str] = &[
        "use-when", "default-collation", "xpath-default-namespace",
        "extension-element-prefixes", "exclude-result-prefixes",
        "version", "expand-text",
    ];
    for attr in node.attributes() {
        let n = attr.name();
        if n.starts_with("xmlns") { continue; }
        let attr_ns = attr.namespace.get().map(|ns| ns.href()).unwrap_or("");
        // XSLT 2.0 §3.6 — an attribute on an XSLT-namespace element
        // that is *itself* in the XSLT namespace is XTSE0090.  The
        // generic attributes are allowed unprefixed only.
        if attr_ns == "http://www.w3.org/1999/XSL/Transform" {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who}: attribute '{n}' in the XSLT namespace is not \
                 permitted on an XSLT element (XTSE0090)"
            )));
        }
        // Foreign-namespace attributes are extension data and pass
        // through.
        if attr.namespace.get().is_some() || n.contains(':') { continue; }
        if !allowed.iter().any(|a| *a == n)
            && !GENERIC_XSLT_ATTRS.iter().any(|a| *a == n)
        {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who}: unrecognised attribute '{n}' (XTSE0090)"
            )));
        }
    }
    Ok(())
}

/// Reject XSLT-namespaced children of `node` whose local name is
/// not in `allowed`.  Used by instructions that have a closed list
/// of legal child elements (xsl:call-template, xsl:apply-templates,
/// xsl:choose, ...).  Non-XSLT (literal-result-element) children
/// and text/comment/PI nodes are not the responsibility of this
/// validator — caller decides whether to permit them as a
/// sequence-constructor.
fn validate_xslt_only_children(
    node: &Node, who: &str, allowed: &[&str],
) -> Result<(), XsltError> {
    for child in node.children() {
        if !child.is_element() { continue; }
        if !is_xslt_element(child) {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} cannot contain literal result element \
                 <{}> (XTSE0010)", child.name(),
            )));
        }
        let local = child.local_name();
        if !allowed.iter().any(|a| *a == local) {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} cannot contain xsl:{local} (XTSE0010 — \
                 allowed children: xsl:{})",
                allowed.join(", xsl:"),
            )));
        }
    }
    Ok(())
}

fn collect_sort_and_with_params(node: &Node)
    -> Result<(Vec<Sort>, Vec<WithParam>), XsltError>
{
    let mut sort   = Vec::new();
    let mut params = Vec::new();
    for child in node.children() {
        if !child.is_element() || !is_xslt_element(child) { continue; }
        // XSLT 2.0 §3.10.2 — `use-when` works on xsl:sort and
        // xsl:with-param too; when the static expression is false,
        // the element is treated as absent.
        if let Some(uw) = read_attribute(child, "use-when") {
            if !evaluate_use_when_at(uw, Some(child))? { continue; }
        }
        match child.local_name() {
            "sort"       => {
                // XSLT 2.0 §13.1 / XTSE1017 — `stable=` is meaningful
                // only on the first xsl:sort in a sibling sequence;
                // its presence on a later sibling is a static error.
                if !sort.is_empty() && read_attribute(child, "stable").is_some() {
                    return Err(XsltError::InvalidStylesheet(
                        "stable= attribute is only allowed on the first \
                         xsl:sort in a sibling sequence (XTSE1017)".into()));
                }
                sort.push(compile_sort(child)?);
            }
            "with-param" => {
                let p = compile_with_param(child)?;
                // XSLT 2.0 §10.1 / XTSE0670 — sibling xsl:with-param
                // elements may not share an expanded name.
                let key = qname_key(&p.name);
                if params.iter().any(|q: &WithParam| qname_key(&q.name) == key) {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "duplicate xsl:with-param '{key}' (XTSE0670)"
                    )));
                }
                params.push(p);
            }
            _ => {}
        }
    }
    Ok((sort, params))
}

/// True iff `s` matches the xml:lang / BCP 47 lexical form: one or
/// more ASCII letters, optionally followed by hyphen-separated
/// subtags of ASCII letters or digits.  The empty string is also
/// accepted (xml:lang="" is a valid undeclaration).
pub(crate) fn is_valid_xml_lang(s: &str) -> bool {
    if s.is_empty() { return true; }
    let mut parts = s.split('-');
    let Some(first) = parts.next() else { return false; };
    if first.is_empty() || !first.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for sub in parts {
        if sub.is_empty()
            || !sub.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return false;
        }
    }
    true
}

fn compile_sort(node: &Node) -> Result<Sort, XsltError> {
    // XSLT 2.0 §13 adds `collation` / `stable` / `as` to the 1.0
    // select / lang / data-type / order / case-order set.
    validate_xslt_only_attributes(node, "xsl:sort",
        &["select", "lang", "data-type", "order", "case-order",
          "collation", "stable", "as"])?;
    reject_select_with_content(node, "xsl:sort", "XTSE1015", false)?;
    let mut select = read_attribute(node, "select")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    // XSLT 2.0 §13.1 — when no `select=` is given, the body sequence
    // constructor produces the sort key.  Lower the body to an XPath
    // expression when possible (handles `xsl:sequence`, value-of,
    // text, choose/if — same shapes the function-body desugarer
    // accepts).  Bodies that don't reduce (xsl:apply-templates,
    // xsl:call-template, …) stay as None and currently fall back to
    // the default key (the context-node string-value).
    if select.is_none() {
        let body = compile_body(node)?;
        if !body.is_empty() {
            if let Some(e) = desugar_body_to_xpath(&body) {
                select = Some(e);
            }
        }
    }
    // XSLT 2.0 §13.1 — a literal collation= URI the processor doesn't
    // recognise is an error (XTDE1035).  AVT collations (`{…}`) and
    // forwards-compat mode defer to the runtime check in `sort.rs`.
    if let Some(c) = read_attribute(node, "collation") {
        if !in_forwards_compat_mode() && !is_recognised_collation(c) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:sort collation='{c}' is not recognised by the processor (XTDE1035)"
            )));
        }
    }
    // XSLT 2.0 §13.1 / XTDE0030 — a literal `lang=` (no AVT braces)
    // must conform to xml:lang (BCP 47), otherwise it's a static
    // error; AVT-bearing values defer to the runtime check.  Empty
    // is allowed.
    if let Some(s) = read_attribute(node, "lang") {
        if !s.contains('{') && !is_valid_xml_lang(s) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:sort lang='{s}' is not a valid xml:lang value (XTDE0030)"
            )));
        }
    }
    // XSLT 2.0 §13.1 / XTTE1100 — `stable=` must be 'yes' or 'no'.
    if let Some(s) = read_attribute(node, "stable") {
        if !s.contains('{') {
            parse_yesno_strict(s, "xsl:sort", "stable")?;
        }
    }
    Ok(Sort {
        select,
        lang:       read_attribute(node, "lang").map(|s| avt(node, s)).transpose()?,
        data_type:  read_attribute(node, "data-type").map(|s| avt(node, s)).transpose()?,
        order:      read_attribute(node, "order").map(|s| avt(node, s)).transpose()?,
        case_order: read_attribute(node, "case-order").map(|s| avt(node, s)).transpose()?,
        collation:  read_attribute(node, "collation").map(|s| avt(node, s)).transpose()?,
    })
}

fn compile_with_param(node: &Node) -> Result<WithParam, XsltError> {
    validate_xslt_only_attributes(node, "xsl:with-param",
        &["name", "select", "as", "tunnel"])?;
    let name = required_qname_attr(node, "name", "xsl:with-param")?;
    let (select, body) = split_select_and_body(node)?;
    reject_select_with_body(node, &select, &body, "xsl:with-param")?;
    let tunnel = if is_xslt_2_0_compile() {
        match read_attribute(node, "tunnel") {
            Some(v) => parse_yesno_strict(v, "xsl:with-param", "tunnel")?,
            None    => false,
        }
    } else { false };
    let as_type = read_attribute(node, "as").map(str::to_string);
    Ok(WithParam { name, select, body, tunnel, as_type })
}

fn compile_choose(node: &Node) -> Result<Instr, XsltError> {
    let mut whens     = Vec::new();
    let mut otherwise = None;
    for child in node.children() {
        // Significant text (anything not whitespace-only) inside
        // xsl:choose is XTSE0010 — text content isn't allowed
        // between when/otherwise.
        if matches!(child.kind, NodeKind::Text | NodeKind::CData)
            && !is_xslt_whitespace_only(child.content())
        {
            return Err(XsltError::InvalidStylesheet(
                "xsl:choose cannot contain non-whitespace text content \
                 (XTSE0010 — only xsl:when / xsl:otherwise allowed)".into(),
            ));
        }
        if !child.is_element() { continue; }
        if !is_xslt_element(child) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:choose cannot contain literal result element \
                 <{}> (XTSE0010 — only xsl:when / xsl:otherwise allowed)",
                child.name(),
            )));
        }
        match child.local_name() {
            "when" => {
                if otherwise.is_some() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:when must precede xsl:otherwise in \
                         xsl:choose (XTSE0010)".into(),
                    ));
                }
                let test = require_attr(child, "test", "xsl:when")?;
                whens.push((parse_xpath_at(node, test).map_err(XsltError::from)?,
                            compile_body(child)?));
            }
            "otherwise" => {
                if otherwise.is_some() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:choose can have at most one xsl:otherwise \
                         (XTSE0010)".into(),
                    ));
                }
                otherwise = Some(compile_body(child)?);
            }
            _ => return Err(XsltError::InvalidStylesheet(format!(
                "xsl:choose children must be xsl:when or xsl:otherwise, got xsl:{}",
                child.local_name(),
            ))),
        }
    }
    if whens.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:choose requires at least one xsl:when".into(),
        ));
    }
    Ok(Instr::Choose { whens, otherwise })
}

fn compile_if(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:if", &["test"])?;
    let test = require_attr(node, "test", "xsl:if")?;
    Ok(Instr::If {
        test: parse_xpath_at(node, test).map_err(XsltError::from)?,
        body: compile_body(node)?,
    })
}

fn compile_for_each(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:for-each", &["select"])?;
    let select = require_attr(node, "select", "xsl:for-each")?;
    // XSLT 1.0 §8 / XSLT 2.0 §11.2 — `xsl:sort` children must
    // precede any sequence-constructor content.  A sort interleaved
    // with content (or trailing it) is XTSE0010.
    let mut seen_non_sort = false;
    for child in node.children() {
        if !child.is_element() { continue; }
        let is_sort = is_xslt_element(child) && child.local_name() == "sort";
        if is_sort {
            if seen_non_sort {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:sort must precede sequence-constructor content \
                     inside xsl:for-each (XTSE0010)".into(),
                ));
            }
        } else {
            seen_non_sort = true;
        }
    }
    let (sort, _) = collect_sort_and_with_params(node)?;
    let mut body = Vec::new();
    for child in node.children() {
        if child.is_element() && is_xslt_element(child) && child.local_name() == "sort" {
            continue;
        }
        if !child.is_element() && !is_significant_text(child) { continue; }
        compile_instr_into(child, &mut body)?;
    }
    Ok(Instr::ForEach {
        select: parse_xpath_at(node, select).map_err(XsltError::from)?,
        sort,
        body,
    })
}

/// XSLT 2.0 §14 `xsl:for-each-group` — exactly one of the four
/// grouping-criterion attributes is required.  Body and sorts mirror
/// the `xsl:for-each` shape.
fn compile_for_each_group(node: &Node) -> Result<Instr, XsltError> {
    let select = require_attr(node, "select", "xsl:for-each-group")?;
    let (kind, key_raw) = match (
        read_attribute(node, "group-by"),
        read_attribute(node, "group-adjacent"),
        read_attribute(node, "group-starting-with"),
        read_attribute(node, "group-ending-with"),
    ) {
        (Some(k), None, None, None) => (GroupingKind::By, k),
        (None, Some(k), None, None) => (GroupingKind::Adjacent, k),
        (None, None, Some(k), None) => (GroupingKind::StartingWith, k),
        (None, None, None, Some(k)) => (GroupingKind::EndingWith, k),
        (None, None, None, None) => return Err(XsltError::InvalidStylesheet(
            "xsl:for-each-group requires one of group-by, group-adjacent, group-starting-with, group-ending-with".into()
        )),
        _ => return Err(XsltError::InvalidStylesheet(
            "xsl:for-each-group accepts exactly one grouping criterion".into()
        )),
    };
    // XSLT 2.0 §14 / XTSE1090 — the `collation` attribute applies
    // to value-equality comparisons and is meaningful only for
    // group-by / group-adjacent.  Pairing it with the positional
    // forms (group-starting-with / group-ending-with) is a static
    // error.
    let mut collation = read_attribute(node, "collation").map(|c| c.trim().to_string());
    if let Some(c) = &collation {
        if !in_forwards_compat_mode()
            && !matches!(kind, GroupingKind::By | GroupingKind::Adjacent)
        {
            return Err(XsltError::InvalidStylesheet(
                "xsl:for-each-group collation= only applies to group-by or \
                 group-adjacent (XTSE1090)".into()
            ));
        }
        // XTSE1190 — collation URI must be recognised.
        if !in_forwards_compat_mode() && !is_recognised_collation(c) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:for-each-group collation='{c}' is not recognised (XTSE1190)"
            )));
        }
    }
    if collation.is_none() {
        // Inherit the in-scope default-collation when no explicit
        // attribute is present.
        if matches!(kind, GroupingKind::By | GroupingKind::Adjacent) {
            collation = effective_default_collation(node);
        }
    }
    let (sort, _) = collect_sort_and_with_params(node)?;
    let mut body = Vec::new();
    for child in node.children() {
        if child.is_element() && is_xslt_element(child) && child.local_name() == "sort" {
            continue;
        }
        if !child.is_element() && !is_significant_text(child) { continue; }
        compile_instr_into(child, &mut body)?;
    }
    Ok(Instr::ForEachGroup {
        select: parse_xpath_at(node, select).map_err(XsltError::from)?,
        kind,
        key:    parse_xpath_at(node, key_raw).map_err(XsltError::from)?,
        sort,
        body,
        collation,
    })
}

/// XSLT 3.0 §18.1 `xsl:source-document` (and the older `xsl:stream`).
/// `href=` is an AVT giving the document URI; the body is evaluated
/// against the loaded document node.  We process it non-streamed, so
/// `streamable=` / `validation=` / `type=` are accepted and ignored.
fn compile_source_document(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:source-document",
        &["href", "streamable", "validation", "type", "use-accumulators"])?;
    let href = avt(node, require_attr(node, "href", "xsl:source-document")?)?;
    let body = compile_body(node)?;
    Ok(Instr::SourceDocument { href, body })
}

/// XSLT 3.0 §10.4 `xsl:evaluate` — `xpath=` (required) supplies the
/// dynamic expression string; `context-item=` its context; child
/// `xsl:with-param`s bind variables visible to it.
fn compile_evaluate(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:evaluate", &[
        "xpath", "context-item", "as", "base-uri", "namespace-context",
        "schema-aware", "with-params",
    ])?;
    let xpath = parse_xpath_at(node, require_attr(node, "xpath", "xsl:evaluate")?)
        .map_err(XsltError::from)?;
    let context_item = read_attribute(node, "context-item")
        .map(|s| parse_xpath_at(node, s)).transpose().map_err(XsltError::from)?;
    let mut with_params = Vec::new();
    for child in node.children() {
        if child.is_element() && is_xslt_element(child)
            && child.local_name() == "with-param"
        {
            with_params.push(compile_with_param(child)?);
        }
    }
    Ok(Instr::Evaluate { xpath, context_item, with_params })
}

/// XSLT 3.0 §15 `xsl:merge` — one or more `xsl:merge-source` children
/// (each with `xsl:merge-key` children) followed by an
/// `xsl:merge-action` whose body runs once per distinct merge key.
fn compile_merge(node: &Node) -> Result<Instr, XsltError> {
    let mut sources = Vec::new();
    let mut action: Option<Vec<Instr>> = None;
    for child in node.children() {
        if !child.is_element() {
            if is_significant_text(child) {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:merge cannot contain text content (XTSE0010)".into()));
            }
            continue;
        }
        if !is_xslt_element(child) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:merge cannot contain literal result element <{}> (XTSE0010)",
                child.name())));
        }
        match child.local_name() {
            "merge-source" => sources.push(compile_merge_source(child)?),
            "merge-action" => {
                let mut body = Vec::new();
                for gc in child.children() {
                    if !gc.is_element() && !is_significant_text(gc) { continue; }
                    compile_instr_into(gc, &mut body)?;
                }
                action = Some(body);
            }
            other => return Err(XsltError::InvalidStylesheet(format!(
                "xsl:merge cannot contain xsl:{other} (XTSE0010)"))),
        }
    }
    if sources.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:merge requires at least one xsl:merge-source (XTSE0010)".into()));
    }
    let action = action.ok_or_else(|| XsltError::InvalidStylesheet(
        "xsl:merge requires an xsl:merge-action (XTSE0010)".into()))?;
    Ok(Instr::Merge { sources, action })
}

fn compile_merge_source(node: &Node) -> Result<MergeSource, XsltError> {
    let name = read_attribute(node, "name").map(|s| s.to_string());
    let select = parse_xpath_at(node, require_attr(node, "select", "xsl:merge-source")?)
        .map_err(XsltError::from)?;
    // `for-each-item` was renamed `for-each-source` between drafts;
    // accept both spellings.
    let for_each_source = read_attribute(node, "for-each-source")
        .or_else(|| read_attribute(node, "for-each-item"))
        .map(|s| parse_xpath_at(node, s)).transpose().map_err(XsltError::from)?;
    let mut keys = Vec::new();
    for child in node.children() {
        if child.is_element() && is_xslt_element(child) && child.local_name() == "merge-key" {
            // xsl:merge-key shares xsl:sort's comparison attributes.
            keys.push(compile_sort(child)?);
        }
    }
    if keys.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:merge-source requires at least one xsl:merge-key (XTSE0010)".into()));
    }
    Ok(MergeSource { name, select, for_each_source, keys })
}

/// XSLT 2.0 §15.1 `xsl:analyze-string` — `regex` is required; children
/// are at most one `xsl:matching-substring` and one
/// `xsl:non-matching-substring` (either or both).  An empty body is
/// also legal (matches the spec but does nothing useful).
fn compile_analyze_string(node: &Node) -> Result<Instr, XsltError> {
    let select = require_attr(node, "select", "xsl:analyze-string")?;
    let regex  = avt(node, require_attr(node, "regex", "xsl:analyze-string")?)?;
    let flags  = read_attribute(node, "flags").map(|s| avt(node, s)).transpose()?
                  .unwrap_or_default();
    let mut matching:     Option<Vec<Instr>> = None;
    let mut non_matching: Option<Vec<Instr>> = None;
    for child in node.children() {
        if !child.is_element() { continue; }
        if !is_xslt_element(child) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:analyze-string cannot contain literal result element \
                 <{}> (XTSE0010)", child.name(),
            )));
        }
        match child.local_name() {
            "matching-substring"     => {
                if matching.is_some() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:analyze-string can have at most one \
                         xsl:matching-substring (XTSE0010)".into()));
                }
                matching = Some(compile_body(child)?);
            }
            "non-matching-substring" => {
                if non_matching.is_some() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:analyze-string can have at most one \
                         xsl:non-matching-substring (XTSE0010)".into()));
                }
                non_matching = Some(compile_body(child)?);
            }
            "fallback" => {}
            other => return Err(XsltError::InvalidStylesheet(format!(
                "unexpected child of xsl:analyze-string: xsl:{other}"
            ))),
        }
    }
    // XSLT 2.0 §15.1 — at least one of xsl:matching-substring /
    // xsl:non-matching-substring must be present (XTSE1130).
    if matching.is_none() && non_matching.is_none() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:analyze-string requires at least one of \
             xsl:matching-substring or xsl:non-matching-substring \
             (XTSE1130)".into()));
    }
    Ok(Instr::AnalyzeString {
        select: parse_xpath_at(node, select).map_err(XsltError::from)?,
        regex, flags,
        matching:     matching.unwrap_or_default(),
        non_matching: non_matching.unwrap_or_default(),
    })
}

/// XSLT 2.0 §13.3 `xsl:perform-sort` — `select=` is optional; if
/// absent the (sequence-constructor) body's sequence is sorted.
/// `xsl:sort` element children are sort directives and aren't part
/// of the body's sequence — they're collected separately and skipped
/// when compiling the body.
fn compile_perform_sort(node: &Node) -> Result<Instr, XsltError> {
    let select = read_attribute(node, "select")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    // XSLT 2.0 §14.2 / XTSE0010 — every xsl:sort sibling must precede
    // any other content (instructions, LREs, non-whitespace text).
    // Sorts intermixed with content can't define the input order.
    require_xsl_sort_first(node, "xsl:perform-sort")?;
    // XSLT 2.0 §13.3 / XTSE1040 — when select= is present the input
    // sequence comes from select=, so the only other content allowed
    // is xsl:sort (the keys) and xsl:fallback (forwards-compat).
    // Any text content / other instructions would be silently
    // discarded; the spec promotes that to a static error.
    if select.is_some() {
        for child in node.children() {
            if child.is_element() {
                let xslt_local = is_xslt_element(child).then(|| child.local_name());
                match xslt_local.as_deref() {
                    Some("sort") | Some("fallback") => {}
                    _ => return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:perform-sort with select= may only contain \
                         xsl:sort and xsl:fallback (XTSE1040)"
                    ))),
                }
            } else if is_text_like(child) && !child.content().trim().is_empty() {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:perform-sort with select= may only contain \
                     xsl:sort and xsl:fallback (XTSE1040)".into()
                ));
            }
        }
    }
    let (sort, _) = collect_sort_and_with_params(node)?;
    let body = if select.is_some() {
        Vec::new()
    } else {
        compile_body_skipping_xsl_sort(node)?
    };
    Ok(Instr::PerformSort { select, sort, body })
}

/// Reject any `xsl:sort` that appears after non-sort content in a
/// parent instruction.  The sort elements must form a leading run so
/// the sort keys are defined before the input is sorted.
fn require_xsl_sort_first(parent: &Node, who: &str) -> Result<(), XsltError> {
    let mut seen_other = false;
    for child in parent.children() {
        if child.is_element() {
            let xslt_local = is_xslt_element(child).then(|| child.local_name());
            match xslt_local.as_deref() {
                Some("sort") => {
                    if seen_other {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:sort in {who} must precede any other \
                             content (XTSE0010)"
                        )));
                    }
                }
                // xsl:with-param is allowed alongside xsl:sort on
                // xsl:apply-templates — also part of the leading run.
                Some("with-param") => {}
                _ => seen_other = true,
            }
        } else if is_text_like(child) && !child.content().trim().is_empty() {
            seen_other = true;
        }
    }
    Ok(())
}

/// Like [`compile_body`] but skips child `xsl:sort` elements (those
/// are collected by [`collect_sort_and_with_params`] into the sort
/// key list, not the sequence constructor).
fn compile_body_skipping_xsl_sort(node: &Node) -> Result<Vec<Instr>, XsltError> {
    let children: Vec<&Node> = node.children().collect();
    let keep_run = run_keep_mask(&children);
    let mut out = Vec::new();
    for (i, child) in children.iter().enumerate() {
        if child.is_element() {
            if is_xslt_element(child) && child.local_name() == "sort" {
                continue;
            }
            compile_instr_into(child, &mut out)?;
        } else if is_text_like(child) && keep_run[i] {
            compile_instr_into(child, &mut out)?;
        }
    }
    Ok(out)
}

/// XSLT 2.0 §11.7 `xsl:namespace` — `name=` is the prefix (AVT);
/// the URI comes from `select=` if present, otherwise from the body.
fn compile_namespace_instr(node: &Node) -> Result<Instr, XsltError> {
    // XTSE0910 — a select attribute requires content that is empty (or
    // only xsl:fallback children).
    reject_select_with_content(node, "xsl:namespace", "XTSE0910", true)?;
    let name   = avt(node, require_attr(node, "name", "xsl:namespace")?)?;
    let select = read_attribute(node, "select")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    let body   = if select.is_none() { compile_body(node)? } else { Vec::new() };
    Ok(Instr::Namespace { name, select, body })
}

/// XSLT 3.0 §15 `xsl:try` — split children into the protected
/// body and the trailing `<xsl:catch>` handlers.  An optional
/// `select=` on `xsl:try` collapses the body into a single
/// expression evaluation; the more common form is body-only.
fn compile_try(node: &Node) -> Result<Instr, XsltError> {
    let mut body    = Vec::new();
    let mut catches = Vec::new();
    let mut seen_catch = false;
    for child in node.children() {
        if !child.is_element() {
            // Whitespace / text children inside xsl:try are
            // tolerated (they're already filtered upstream by
            // compile_body's whitespace pass when applicable);
            // keep parity by ignoring non-element nodes here.
            continue;
        }
        // XSLT 3.0 §3.6 — `xsl:fallback` is permitted as a child of
        // any instruction and is processed only when the parent is an
        // *unrecognised* instruction.  `xsl:try` is recognised, so its
        // fallback children are inert; ignore them wherever they sit
        // (notably interleaved with `xsl:catch`, which must not trip
        // the "instruction after catch" gate below).
        if is_xslt_element(child) && child.local_name() == "fallback" {
            continue;
        }
        let is_catch = is_xslt_element(child) && child.local_name() == "catch";
        if is_catch {
            seen_catch = true;
            catches.push(compile_catch(child)?);
            continue;
        }
        if seen_catch {
            // XSLT 3.0 §15 — all xsl:catch handlers must come
            // after the body.  An instruction after a catch is
            // XTSE0010.
            return Err(XsltError::InvalidStylesheet(
                "xsl:try body must precede all xsl:catch handlers (XTSE0010)".into(),
            ));
        }
        compile_instr_into(child, &mut body)?;
    }
    if catches.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:try requires at least one xsl:catch handler (XTSE0010)".into(),
        ));
    }
    // `select=` shortcut: evaluate the expression in the protected
    // context.  When supplied, the body must be empty (apart from
    // catch handlers) — match xsl:value-of's pattern.
    if let Some(sel) = read_attribute(node, "select") {
        if !body.is_empty() {
            return Err(XsltError::InvalidStylesheet(
                "xsl:try select= and body are mutually exclusive (XTSE0010)".into(),
            ));
        }
        let expr = parse_xpath_at(node, sel).map_err(XsltError::from)?;
        body.push(Instr::Sequence { select: expr });
    }
    Ok(Instr::Try { body, catches })
}

/// XSLT 3.0 §8.3 `xsl:iterate`.  Content model is
/// `(xsl:param*, xsl:on-completion?, sequence-constructor)`.
fn compile_iterate(node: &Node) -> Result<Instr, XsltError> {
    let select = require_attr(node, "select", "xsl:iterate")?;
    let select = parse_xpath_at(node, select).map_err(XsltError::from)?;
    let mut params = Vec::new();
    let mut on_completion = Vec::new();
    let mut body = Vec::new();
    // Route xsl:param → loop-carried params and xsl:on-completion →
    // its own body; everything else is the iteration body.  Strip
    // whitespace-only text runs the same way compile_body does (XSLT
    // 1.0 §3.4) so layout whitespace doesn't pollute the body.
    let children: Vec<&Node> = node.children().collect();
    let keep_run = run_keep_mask(&children);
    for (i, child) in children.iter().enumerate() {
        if child.is_element() {
            if is_xslt_element(child) {
                match child.local_name() {
                    "param"         => { params.push(compile_param(child)?); continue; }
                    "on-completion" => { on_completion = compile_body(child)?; continue; }
                    _ => {}
                }
            }
            compile_instr_into(child, &mut body)?;
        } else if is_text_like(child) && keep_run[i] {
            compile_instr_into(child, &mut body)?;
        }
    }
    Ok(Instr::Iterate { select, params, on_completion, body })
}

/// XSLT 3.0 §8.3 `xsl:next-iteration` — carries `xsl:with-param`
/// values for the next iteration.
fn compile_next_iteration(node: &Node) -> Result<Instr, XsltError> {
    let (_, with_params) = collect_sort_and_with_params(node)?;
    Ok(Instr::NextIteration { with_params })
}

/// XSLT 3.0 §8.3 `xsl:break` — optional `select=` or body is the
/// break's output.
fn compile_break(node: &Node) -> Result<Instr, XsltError> {
    let select = read_attribute(node, "select")
        .map(|s| parse_xpath_at(node, s)).transpose().map_err(XsltError::from)?;
    let body = compile_body(node)?;
    // XSLT 3.0 §8.3 / XTSE0010 — select= and a non-empty sequence
    // constructor are mutually exclusive.
    if select.is_some() && !body.is_empty() {
        return Err(XsltError::InvalidStylesheet(
            "xsl:break: select= and a non-empty body are mutually exclusive (XTSE0010)".into(),
        ));
    }
    Ok(Instr::Break { select, body })
}

fn compile_catch(node: &Node) -> Result<crate::ast::TryCatch, XsltError> {
    use crate::ast::CatchMatcher;
    // `errors=` is a whitespace-separated list of NameTests.
    // Empty / missing means catch-all.
    let errors_attr = read_attribute(node, "errors").unwrap_or("*").trim().to_string();
    let mut errors  = Vec::new();
    for tok in errors_attr.split_ascii_whitespace() {
        let m = if tok == "*" {
            CatchMatcher::Any
        } else if let Some(rest) = tok.strip_prefix("*:") {
            CatchMatcher::LocalNameOnly(rest.to_string())
        } else if let Some(prefix) = tok.strip_suffix(":*") {
            CatchMatcher::PrefixWildcard(prefix.to_string())
        } else {
            CatchMatcher::QName(parse_qname_on(node, tok)?)
        };
        errors.push(m);
    }
    let body = if let Some(sel) = read_attribute(node, "select") {
        // `xsl:catch select=` shortcut: evaluate the expression in
        // the catch context.  Body must be empty when present.
        if node.children().any(|c| c.is_element()) {
            return Err(XsltError::InvalidStylesheet(
                "xsl:catch select= and body are mutually exclusive (XTSE0010)".into(),
            ));
        }
        vec![Instr::Sequence { select: parse_xpath_at(node, sel).map_err(XsltError::from)? }]
    } else {
        compile_body(node)?
    };
    Ok(crate::ast::TryCatch { errors, body })
}

fn compile_value_of(node: &Node) -> Result<Instr, XsltError> {
    validate_xslt_only_attributes(node, "xsl:value-of",
        &["select", "disable-output-escaping", "separator"])?;
    // XSLT 2.0 §11.6 allows `<xsl:value-of>` with no `select=` and
    // a sequence-constructor body instead.  We compile that body
    // into a synthetic XPath: the string-value of the constructed
    // result tree.  An RTF variable + string() conversion would be
    // strictly more faithful, but for the conformance corpus the
    // shorter desugaring is enough — convert the body into a
    // captured RTF that the engine stringifies at apply time.
    let select_attr = read_attribute(node, "select");
    let explicit_sep = read_attribute(node, "separator").map(|s| avt(node, s)).transpose()?;
    // XSLT 2.0 §11.5 default separator:
    //   * with select=  → single space
    //   * with body     → zero-length string
    // A nearer `[xsl:]version="1.0"` ancestor switches this value-of
    // back to XSLT 1.0 semantics (first item only), even inside a 2.0
    // stylesheet (XSLT 2.0 §3.8) — represented as `separator: None`.
    // But `separator=` is an XSLT 2.0 attribute; specifying it
    // explicitly always joins the whole sequence, even in a 1.0 scope
    // (the W3C `backwards-009` / `xpath-compat-0401` cases), so the
    // backwards-compat default is overridden whenever it is present.
    let bc = ancestor_forces_backwards_compat(node);
    let separator_for_select = if explicit_sep.is_some() {
        explicit_sep.clone()
    } else if is_xslt_2_0_compile() && !bc {
        Some(Avt::literal(" "))
    } else {
        None
    };
    let separator_for_body = if explicit_sep.is_some() {
        explicit_sep.clone()
    } else if is_xslt_2_0_compile() && !bc {
        Some(Avt::literal(""))
    } else {
        None
    };
    let select = match select_attr {
        Some(s) => {
            // XSLT 2.0 §11.5 / XTSE0870 — when select= is present a
            // non-empty sequence-constructor body is a static error.
            // The empty / whitespace-only body is conventionally
            // present (formatting) so don't flag.
            if is_xslt_2_0_compile() && has_non_whitespace_child(node) {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:value-of with select= must have an empty \
                     sequence-constructor body (XTSE0870)".into()
                ));
            }
            parse_xpath_at(node, s).map_err(XsltError::from)?
        }
        None if is_xslt_2_0_compile() => {
            // XSLT 2.0 §11.5 / XTSE0870 — the body is the source of
            // the value when select= is absent.  An empty
            // xsl:value-of is therefore a static error.
            if !has_any_child(node) {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:value-of must specify select= or a non-empty \
                     sequence constructor (XTSE0870)".into()
                ));
            }
            return Ok(Instr::ValueOfBody {
                body: compile_body(node)?,
                dose: read_attribute(node, "disable-output-escaping")
                    .map(parse_yesno).unwrap_or(false),
                separator: separator_for_body,
            });
        }
        None => return Err(XsltError::InvalidStylesheet(
            "xsl:value-of requires select= attribute".into())),
    };
    let separator = separator_for_select;
    Ok(Instr::ValueOf {
        select,
        dose: read_attribute(node, "disable-output-escaping")
            .map(parse_yesno).unwrap_or(false),
        separator,
    })
}

fn compile_copy(node: &Node) -> Result<Instr, XsltError> {
    // XSLT 2.0 §11.1 adds `copy-namespaces` / `inherit-namespaces`
    // / `type` / `validation` to the 1.0 `use-attribute-sets` attr.
    validate_xslt_only_attributes(node, "xsl:copy",
        &["use-attribute-sets", "copy-namespaces",
          "inherit-namespaces", "type", "validation"])?;
    // XSLT 2.0 §11.9.1 — `copy-namespaces` (default `yes`) controls
    // whether the namespace nodes of the copied element are carried to
    // the copy.  `no` keeps only the bindings its own name needs.
    let copy_namespaces = match read_attribute(node, "copy-namespaces") {
        Some(v) => match v.trim() {
            "yes" => true,
            "no"  => false,
            _ => return Err(XsltError::InvalidStylesheet(format!(
                "xsl:copy copy-namespaces='{v}' must be 'yes' or 'no' (XTSE0020)"
            ))),
        },
        None => true,
    };
    Ok(Instr::Copy {
        use_attribute_sets: parse_qname_list(
            node, read_attribute(node, "use-attribute-sets").unwrap_or(""),
        )?,
        body: compile_body(node)?,
        copy_namespaces,
    })
}

fn compile_copy_of(node: &Node) -> Result<Instr, XsltError> {
    // XSLT 1.0 §11.3: only `select=` is permitted.  XSLT 2.0 adds
    // `copy-namespaces`, `validation`, `type` — accept those when
    // the stylesheet declares version >= 2.0 (we don't implement
    // schema validation, so they're effectively no-ops).
    let two_oh = is_xslt_2_0_compile();
    for attr in node.attributes() {
        let n = attr.name();
        if n.starts_with("xmlns") { continue; }
        let local = n.rsplit_once(':').map(|(_, l)| l).unwrap_or(n);
        let in_xsl_ns = attr.namespace.get()
            .map(|ns| ns.href() == "http://www.w3.org/1999/XSL/Transform")
            .unwrap_or(false);
        if !in_xsl_ns && (attr.namespace.get().is_some() || n.contains(':')) { continue; }
        let is_2_0_attr = two_oh && matches!(local,
            "copy-namespaces" | "validation" | "type");
        if local != "select" && !is_2_0_attr {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:copy-of: unrecognised attribute '{n}' (XTSE0090)"
            )));
        }
    }
    let select = require_attr(node, "select", "xsl:copy-of")?;
    // copy-namespaces= must be yes/no (no/false/0/1/true also tolerated
    // by lenient implementations, but the W3C suite's enum test rejects
    // values outside the spec set — XTSE0020).
    // XSLT 2.0 §11.9.1 — `copy-namespaces` (default `yes`) controls
    // whether the namespace nodes of copied elements are carried over.
    // `no` copies only the namespaces needed for the element's and its
    // attributes' own names, dropping inherited in-scope declarations.
    let copy_namespaces = match read_attribute(node, "copy-namespaces") {
        Some(v) => match v.trim() {
            "yes" => true,
            "no"  => false,
            _ => return Err(XsltError::InvalidStylesheet(format!(
                "xsl:copy-of copy-namespaces='{v}' must be 'yes' or 'no' (XTSE0020)"
            ))),
        },
        None => true,
    };
    // XSLT 1.0 §11.3: xsl:copy-of has no content.  Non-whitespace
    // children, element children, or `xsl:fallback` apart, are an
    // XTSE0260 static error.
    for child in node.children() {
        match child.kind {
            NodeKind::Element => {
                if !(is_xslt_element(child) && child.local_name() == "fallback") {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:copy-of must have an empty content model \
                         (XTSE0260)".into()));
                }
            }
            NodeKind::Text | NodeKind::CData => {
                if !is_xslt_whitespace_only(child.content()) {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:copy-of must have an empty content model \
                         (XTSE0260)".into()));
                }
            }
            _ => {}
        }
    }
    Ok(Instr::CopyOf {
        select: parse_xpath_at(node, select).map_err(XsltError::from)?,
        copy_namespaces,
    })
}

fn compile_element(node: &Node) -> Result<Instr, XsltError> {
    // XSLT 2.0 §11.2 closed attribute set.  The XSLT 3.0 `select=`
    // (which substitutes for the body) is XTSE0090 in 2.0.
    validate_xslt_only_attributes(node, "xsl:element",
        &["name", "namespace", "inherit-namespaces",
          "use-attribute-sets", "type", "validation"])?;
    Ok(Instr::Element {
        name:               avt(node, require_attr(node, "name", "xsl:element")?)?,
        namespace:          read_attribute(node, "namespace").map(|s| avt(node, s)).transpose()?,
        use_attribute_sets: parse_qname_list(
            node, read_attribute(node, "use-attribute-sets").unwrap_or(""),
        )?,
        body: compile_body(node)?,
        in_scope_namespaces: collect_in_scope_namespaces(node),
    })
}

/// True when `node` has sequence-constructor content: any element child
/// or significant (non-whitespace) text.  When `allow_fallback`, an
/// `xsl:fallback` child does not count.
fn has_constructor_content(node: &Node, allow_fallback: bool) -> bool {
    node.children().any(|c| {
        if c.is_element() {
            !(allow_fallback && is_xslt_element(c) && c.local_name() == "fallback")
        } else {
            is_significant_text(c)
        }
    })
}

/// Several instructions take a `select` attribute as a shortcut for the
/// sequence constructor and so require empty content when it is present
/// (XSLT 2.0 §11): xsl:attribute (XTSE0840), xsl:comment (XTSE0940),
/// xsl:processing-instruction (XTSE0880), xsl:namespace (XTSE0910 — but
/// xsl:fallback children are allowed), xsl:sort (XTSE1015).
fn reject_select_with_content(
    node: &Node, who: &str, code: &str, allow_fallback: bool,
) -> Result<(), XsltError> {
    if read_attribute(node, "select").is_some() && has_constructor_content(node, allow_fallback) {
        return Err(XsltError::InvalidStylesheet(format!(
            "{who} has a select attribute and non-empty content ({code})"
        )));
    }
    Ok(())
}

fn compile_attribute(node: &Node) -> Result<Instr, XsltError> {
    // XSLT 2.0 §10.1.1 — `select` is a shortcut for the sequence
    // constructor, so the content must be empty when it is present.
    reject_select_with_content(node, "xsl:attribute", "XTSE0840", false)?;
    let select = read_attribute(node, "select")
        .map(|s| parse_xpath_at(node, s).map_err(XsltError::from))
        .transpose()?;
    Ok(Instr::Attribute {
        name:      avt(node, require_attr(node, "name", "xsl:attribute")?)?,
        namespace: read_attribute(node, "namespace").map(|s| avt(node, s)).transpose()?,
        select,
        separator: read_attribute(node, "separator").map(|s| avt(node, s)).transpose()?,
        body:      compile_body(node)?,
        in_scope_namespaces: collect_in_scope_namespaces(node),
        schema_type: read_attribute(node, "type")
            .and_then(|v| resolve_type_qname(node, v)),
    })
}

/// Walk `node` and its ancestors collecting every `xmlns` / `xmlns:p`
/// declaration in scope, with closer (inner) declarations shadowing
/// farther (outer) ones.  Returns `(prefix, uri)` pairs — `prefix:
/// None` is the default namespace; an entry with `uri == ""` is the
/// `xmlns=""` undeclaration.
fn collect_in_scope_namespaces(node: &Node) -> Vec<(Option<String>, String)> {
    let mut seen: std::collections::HashSet<Option<String>>
        = std::collections::HashSet::new();
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        for (prefix, uri) in n.ns_declarations() {
            let key: Option<String> = prefix.map(str::to_owned);
            if seen.contains(&key) { continue; }
            seen.insert(key.clone());
            out.push((key, uri.to_owned()));
        }
        cur = n.parent.get();
    }
    out
}

fn compile_text(node: &Node) -> Result<Instr, XsltError> {
    let mut text = String::new();
    for child in node.children() {
        match child.kind {
            NodeKind::Text | NodeKind::CData => text.push_str(child.content()),
            NodeKind::Element => {
                // XSLT 1.0 §7.2 / XSLT 2.0 §11.2 — xsl:text's content
                // is character data only; any nested element is
                // XTSE0010 (illegal-content for the instruction).
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:text content must be character data; found <{}> (XTSE0010)",
                    child.name(),
                )));
            }
            _ => {}
        }
    }
    Ok(Instr::LiteralText {
        text,
        dose: read_attribute(node, "disable-output-escaping")
            .map(parse_yesno).unwrap_or(false),
    })
}

fn compile_number(node: &Node) -> Result<Instr, XsltError> {
    let value = read_attribute(node, "value")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    // XSLT 2.0 §13.7 / XTSE0975 — when `value=` is present, `select`,
    // `level`, `count`, and `from` must all be absent.  Mixing them
    // is a static error.
    if value.is_some() && !in_forwards_compat_mode() {
        for attr in &["select", "level", "count", "from"] {
            if read_attribute(node, attr).is_some() {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:number value='{}' cannot combine with {attr}= (XTSE0975)",
                    read_attribute(node, "value").unwrap_or(""),
                )));
            }
        }
    }
    let level = match read_attribute(node, "level").unwrap_or("single") {
        "single"   => crate::ast::NumberLevel::Single,
        "any"      => crate::ast::NumberLevel::Any,
        "multiple" => crate::ast::NumberLevel::Multiple,
        other => return Err(XsltError::InvalidStylesheet(format!(
            "xsl:number level= must be single|any|multiple (got {other:?})"
        ))),
    };
    let count = read_attribute(node, "count")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    let from = read_attribute(node, "from")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    let select = read_attribute(node, "select")
        .map(|src| parse_xpath_at(node, src)).transpose().map_err(XsltError::from)?;
    // XSLT 2.0 §14 / XTSE1060 / XTSE1070 — patterns may not call
    // current-group() / current-grouping-key() / regex-group();
    // they're defined only in the dynamic context of a grouping
    // or analyze-string instruction.
    for (slot, attr) in [(&count, "count"), (&from, "from")] {
        if let Some(e) = slot { reject_pattern_grouping_calls(e, attr)?; }
    }
    // XSLT 2.0 §5.5.2 / XTSE0340 — count= and from= are *patterns*,
    // not arbitrary XPath expressions; reject obviously-non-pattern
    // top-level forms (arithmetic, comparison, conditional, etc.)
    // that the general XPath parser happens to accept.
    for (slot, attr) in [(&count, "xsl:number count="), (&from, "xsl:number from=")] {
        if let Some(e) = slot { ensure_pattern_shape(e, attr)?; }
    }
    let format = avt(node, read_attribute(node, "format").unwrap_or("1"))?;
    let grouping_separator = read_attribute(node, "grouping-separator")
        .map(|s| avt(node, s)).transpose()?;
    let grouping_size = read_attribute(node, "grouping-size")
        .map(|s| avt(node, s)).transpose()?;
    let ordinal = read_attribute(node, "ordinal").map(|s| avt(node, s)).transpose()?;
    let lang = read_attribute(node, "lang").map(|s| avt(node, s)).transpose()?;
    // XSLT 2.0 §13.7 / XTSE0020 — a literal lang= (no AVT braces)
    // must be a valid xml:lang token.  AVT forms defer to the
    // runtime check in eval.rs.
    if let Some(s) = read_attribute(node, "lang") {
        if !s.contains('{') && !is_valid_xml_lang(s) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:number lang='{s}' is not a valid xml:lang value (XTSE0020)"
            )));
        }
    }
    let letter_value = read_attribute(node, "letter-value").map(|s| avt(node, s)).transpose()?;
    let start_at = read_attribute(node, "start-at").map(|s| avt(node, s)).transpose()?;
    Ok(Instr::Number {
        value, select, level, count, from, format,
        grouping_separator, grouping_size,
        ordinal, lang, letter_value, start_at,
    })
}

fn compile_literal_element(node: &Node) -> Result<Instr, XsltError> {
    let name = node_qname(node)?;
    // XSLT 1.0 §14.1 — an element whose namespace appears in a
    // (containing-element-or-ancestor) `xsl:extension-element-prefixes`
    // is an *extension element*, not a literal result element.
    // Unknown extension instructions fall back to their `xsl:fallback`
    // children at execution time, same as unknown XSLT instructions.
    let ext_uris = collect_extension_element_uris(node);
    if !ext_uris.is_empty() && ext_uris.contains(&name.uri) {
        let mut fallback: Vec<Instr> = Vec::new();
        for child in node.children() {
            if child.is_element()
                && is_xslt_element(child)
                && child.local_name() == "fallback"
            {
                fallback.extend(compile_body(child)?);
            }
        }
        return Ok(Instr::Unsupported {
            name: format!("{}{}", name.uri, name.local),
            fallback,
        });
    }
    let mut attributes = Vec::new();
    let mut use_attribute_sets: Vec<QName> = Vec::new();
    let mut schema_type: Option<(String, String)> = None;
    for attr in node.attributes() {
        // Skip namespace declarations — they're collected separately
        // below into `namespaces`, not emitted as attribute templates.
        let aname_lex = attr.name();
        if aname_lex == "xmlns" || aname_lex.starts_with("xmlns:") { continue; }
        let aname = attr_qname(node, attr)?;
        // XSLT 1.0 §7.1.1 — attributes in the XSLT namespace on a
        // literal result element are stylesheet directives, not
        // emittable attributes (xsl:version, xsl:use-attribute-sets,
        // xsl:exclude-result-prefixes, xsl:extension-element-prefixes).
        if aname.uri == "http://www.w3.org/1999/XSL/Transform" {
            // XSLT 2.0 §11.2.1 / XTSE0085 — only a closed set of
            // XSLT-namespaced attributes may appear on a literal
            // result element; everything else is a static error.
            const LRE_ALLOWED_XSL_ATTRS: &[&str] = &[
                "version", "use-attribute-sets", "default-collation",
                "exclude-result-prefixes", "extension-element-prefixes",
                "xpath-default-namespace", "expand-text",
                "type", "use-when", "validation", "inherit-namespaces",
            ];
            if !LRE_ALLOWED_XSL_ATTRS.contains(&aname.local.as_str())
                && !in_forwards_compat_mode()
            {
                return Err(XsltError::InvalidStylesheet(format!(
                    "literal result element <{}>: attribute 'xsl:{}' is not \
                     permitted (XTSE0085)", node.name(), aname.local,
                )));
            }
            if aname.local == "use-attribute-sets" {
                use_attribute_sets = parse_qname_list(node, attr.value())?;
            }
            if aname.local == "type" {
                schema_type = resolve_type_qname(node, attr.value());
            }
            continue;
        }
        attributes.push((aname, avt(node, attr.value())?));
    }
    // Collect in-scope namespaces: walk node + ancestors, with inner
    // declarations shadowing outer ones.  Per XSLT 1.0 §7.1.1, these
    // become namespace nodes on the result element; XSLT-specific
    // namespaces and the user-listed exclusion set get filtered out.
    //
    // Exception: an explicit `xmlns=""` on the LRE itself (a
    // *default-namespace undeclaration*) is not carried through to
    // the result tree.  Resolves XSLT 2.0 spec bug 5857 — the
    // undeclaration has no observable effect on the LRE (the element
    // is in the namespace its name resolves to, regardless) and
    // serializers shouldn't emit redundant `xmlns=""` decls.  The
    // rule applies only at the LRE-construction site; an `xmlns=""`
    // on a source-document element copied through `xsl:copy-of` IS
    // preserved.
    // Distinguish LOCAL declarations (on the LRE itself) from INHERITED
    // ones (on ancestors): XSLT 2.0 §11.1.3 says
    // `exclude-result-prefixes` excludes inherited prefixes only —
    // namespace declarations the author wrote directly on the LRE
    // survive even when `#all` appears on an ancestor.
    let mut local_by_prefix: std::collections::HashMap<Option<String>, String>
        = std::collections::HashMap::new();
    let mut inherited_by_prefix: std::collections::HashMap<Option<String>, String>
        = std::collections::HashMap::new();
    let mut cur = Some(node);
    let mut is_lre_itself = true;
    while let Some(n) = cur {
        for (prefix, uri) in n.ns_declarations() {
            if is_lre_itself && prefix.is_none() && uri.is_empty() {
                // Skip default-namespace undeclaration on the LRE.
                continue;
            }
            let prefix = prefix.map(str::to_owned);
            if is_lre_itself {
                local_by_prefix.entry(prefix).or_insert_with(|| uri.to_owned());
            } else {
                // Inner declarations shadow outer ones; only record the
                // closest binding for each prefix.
                inherited_by_prefix.entry(prefix).or_insert_with(|| uri.to_owned());
            }
        }
        is_lre_itself = false;
        cur = n.parent.get();
    }
    // XSLT 1.0 §7.1.1 / 2.0 §11.1.3 — exclude-result-prefixes is
    // resolved against the in-scope namespaces AT THE ANCESTOR that
    // carries the attribute.  `#all` on `<xsl:stylesheet>` therefore
    // means "all prefixes declared on the stylesheet element", not
    // "everything visible at this LRE" — so downstream local
    // declarations (`<out xmlns:c="…"/>`) survive even when an
    // ancestor said `#all`.
    let exclude_uris = compile_lre_exclusion_uris(node);
    // Validate the listed prefixes resolve to something in scope.
    // `#all` / `#default` are always fine; other names must be
    // bound somewhere up the ancestor chain.
    let exclude_prefixes = compile_lre_exclusion_set(node);
    for p in &exclude_prefixes {
        if p == "#all" || p == "#default" { continue; }
        let resolved = local_by_prefix.get(&Some(p.clone()))
            .or_else(|| inherited_by_prefix.get(&Some(p.clone())));
        if resolved.is_none() {
            return Err(XsltError::InvalidStylesheet(format!(
                "exclude-result-prefixes references undeclared prefix '{p}' (XTSE0808)"
            )));
        }
    }
    if exclude_prefixes.contains("#default")
        && local_by_prefix.get(&None).is_none()
        && inherited_by_prefix.get(&None).is_none()
    {
        return Err(XsltError::InvalidStylesheet(
            "exclude-result-prefixes='#default' but no default namespace \
             is in scope (XTSE0809)".into()));
    }
    // Compose the in-scope set: local takes precedence over inherited
    // for any shared prefix, but BOTH slices go through the same URI
    // exclusion filter — local declarations aren't immune.
    let mut by_prefix: std::collections::HashMap<Option<String>, String> = local_by_prefix;
    for (prefix, uri) in inherited_by_prefix {
        by_prefix.entry(prefix).or_insert(uri);
    }
    let mut namespaces: Vec<(Option<String>, String)> = by_prefix.into_iter()
        .filter(|(_p, uri)| {
            // The XSLT namespace itself never propagates to the result.
            if uri == "http://www.w3.org/1999/XSL/Transform" { return false; }
            !exclude_uris.contains(uri.as_str())
        })
        .collect();
    // Stable ordering so AST diffs/tests are deterministic.
    namespaces.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Instr::LiteralElement {
        name,
        attributes,
        namespaces,
        use_attribute_sets,
        schema_type,
        body: compile_body(node)?,
    })
}

/// Resolve a QName-valued attribute (e.g. an `xsl:type="xs:integer"`)
/// against the namespaces in scope on `node`, returning the expanded
/// `(namespace, local)`.  Walks `node` and its ancestors for the
/// prefix binding; an unprefixed name resolves to the in-scope default
/// namespace (or no namespace).  `None` when the prefix is undeclared.
fn resolve_type_qname(node: &Node, value: &str) -> Option<(String, String)> {
    let value = value.trim();
    let (prefix, local) = match value.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None         => (None, value),
    };
    if prefix == Some("xml") {
        return Some(("http://www.w3.org/XML/1998/namespace".into(), local.into()));
    }
    let mut cur = Some(node);
    while let Some(n) = cur {
        for (p, uri) in n.ns_declarations() {
            match (prefix, p) {
                (Some(want), Some(got)) if want == got =>
                    return Some((uri.to_string(), local.to_string())),
                (None, None) =>
                    return Some((uri.to_string(), local.to_string())),
                _ => {}
            }
        }
        cur = n.parent.get();
    }
    // Unprefixed with no in-scope default namespace → no namespace.
    prefix.is_none().then(|| (String::new(), local.to_string()))
}

/// Read `exclude-result-prefixes` and `extension-element-prefixes`
/// from the literal-result element and its ancestors, building the
/// set of prefix strings that must NOT propagate as namespace nodes.
/// XSLT 1.0 §7.1.1 says extension-element prefixes are also
/// auto-excluded from the result.
fn compile_lre_exclusion_set(node: &Node) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        // On a literal result element the attributes are prefixed
        // `xsl:exclude-result-prefixes` / `xsl:extension-element-prefixes`;
        // on `<xsl:stylesheet>` they're unprefixed.  `read_attribute`
        // skips prefixed names, so look the prefixed form up directly.
        for attr in n.attributes() {
            let name = attr.name();
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            let in_xsl_ns = attr.namespace.get()
                .map(|ns| ns.href() == "http://www.w3.org/1999/XSL/Transform")
                .unwrap_or(false);
            let bare = attr.namespace.get().is_none() && !name.contains(':');
            let is_exclude = local == "exclude-result-prefixes"
                && (bare || in_xsl_ns || name == "xsl:exclude-result-prefixes");
            let is_extension = local == "extension-element-prefixes"
                && (bare || in_xsl_ns || name == "xsl:extension-element-prefixes");
            if is_exclude || is_extension {
                for tok in attr.value().split_whitespace() {
                    out.insert(tok.to_owned());
                }
            }
        }
        cur = n.parent.get();
    }
    out
}

/// Build the set of NAMESPACE URIs to exclude from `node`'s result
/// namespace nodes by walking ancestors and resolving every
/// `exclude-result-prefixes` / `extension-element-prefixes` token to
/// a URI using that ancestor's own in-scope namespace bindings.
/// `#all` resolves to all prefixes in scope on the bearing element
/// (not on `node`), so a `<xsl:stylesheet exclude-result-prefixes="#all">`
/// excludes only the prefixes the stylesheet itself declared, not
/// later locally-declared bindings on downstream LREs.
fn compile_lre_exclusion_uris(node: &Node) -> std::collections::HashSet<String> {
    let mut uris: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        // Collect this ancestor's exclude tokens.
        let mut tokens: Vec<String> = Vec::new();
        for attr in n.attributes() {
            let name = attr.name();
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            let in_xsl_ns = attr.namespace.get()
                .map(|ns| ns.href() == "http://www.w3.org/1999/XSL/Transform")
                .unwrap_or(false);
            let bare = attr.namespace.get().is_none() && !name.contains(':');
            let is_exclude = local == "exclude-result-prefixes"
                && (bare || in_xsl_ns || name == "xsl:exclude-result-prefixes");
            let is_extension = local == "extension-element-prefixes"
                && (bare || in_xsl_ns || name == "xsl:extension-element-prefixes");
            if is_exclude || is_extension {
                for tok in attr.value().split_whitespace() {
                    tokens.push(tok.to_owned());
                }
            }
        }
        if !tokens.is_empty() {
            // Build the in-scope namespace map AT THIS ANCESTOR by
            // walking from `n` outward.  Inner declarations shadow
            // outer, so the first encounter for each prefix wins.
            let mut ancestor_scope: std::collections::HashMap<Option<String>, String>
                = std::collections::HashMap::new();
            let mut walk = Some(n);
            while let Some(w) = walk {
                for (prefix, uri) in w.ns_declarations() {
                    let p = prefix.map(str::to_owned);
                    ancestor_scope.entry(p).or_insert_with(|| uri.to_owned());
                }
                walk = w.parent.get();
            }
            for tok in tokens {
                match tok.as_str() {
                    "#all" => {
                        // Every prefix in scope at this ancestor — minus
                        // the XSLT namespace, which is auto-stripped
                        // from result trees regardless.
                        for (_p, uri) in &ancestor_scope {
                            if uri != "http://www.w3.org/1999/XSL/Transform" {
                                uris.insert(uri.clone());
                            }
                        }
                    }
                    "#default" => {
                        if let Some(uri) = ancestor_scope.get(&None) {
                            uris.insert(uri.clone());
                        }
                    }
                    name => {
                        if let Some(uri) = ancestor_scope.get(&Some(name.to_owned())) {
                            uris.insert(uri.clone());
                        }
                    }
                }
            }
        }
        cur = n.parent.get();
    }
    uris
}

/// Walk `node` and its ancestors collecting every URI that's bound
/// to a prefix listed in an in-scope `xsl:extension-element-prefixes`
/// (or `extension-element-prefixes` on `<xsl:stylesheet>`).  Used by
/// the LRE compiler to recognise extension-element invocations and
/// route them through the same forward-compat `xsl:fallback`
/// machinery as unknown XSLT instructions.
/// XSLT 1.0 §7.1.4 (XTSE0710): every `use-attribute-sets` reference
/// — on `xsl:attribute-set`, `xsl:element`, `xsl:copy`, or a literal
/// result element — must name a declared attribute-set.  Walks the
/// AST and surfaces the first dangling reference.
/// XSLT 2.0 §6.4 (XTSE0660) — within a single stylesheet, two
/// named templates may not share an expanded name AND the same
/// import precedence (a higher-precedence template would shadow
/// any same-name lower-precedence one, but tied precedence is
/// a static ambiguity).  Walks the fully-assembled AST after
/// import / include resolution so cross-module duplicates surface
/// correctly.
/// XSLT 2.0 §9.1 / XTSE0630 — within a single import-precedence
/// stratum, a global variable / parameter may not be declared
/// twice with the same expanded name, unless another declaration
/// at a higher precedence shadows the duplicate.
pub fn validate_global_variable_uniqueness(ast: &StylesheetAst) -> Result<(), XsltError> {
    use std::collections::HashMap;
    let mut seen: HashMap<(String, i32), usize> = HashMap::new();
    let mut max_prec: HashMap<String, i32> = HashMap::new();
    let entries = ast.global_variables.iter().map(|v| (&v.name, TOP_LEVEL_IMPORT_PRECEDENCE))
        .chain(ast.global_params.iter().map(|p| (&p.name, TOP_LEVEL_IMPORT_PRECEDENCE)));
    for (n, prec) in entries {
        let key = qname_key(n);
        *seen.entry((key.clone(), prec)).or_insert(0) += 1;
        let entry = max_prec.entry(key).or_insert(prec);
        if prec > *entry { *entry = prec; }
    }
    for ((name, prec), count) in &seen {
        if *count < 2 { continue; }
        if max_prec.get(name).copied().unwrap_or(*prec) > *prec { continue; }
        return Err(XsltError::InvalidStylesheet(format!(
            "duplicate global xsl:variable / xsl:param '{name}' at the same \
             import precedence ({prec}) (XTSE0630)"
        )));
    }
    Ok(())
}

/// XSLT 2.0 §20 / XTSE1560 — when more than one xsl:output
/// declaration shares the same name (or all share the unnamed
/// default), every attribute set on multiple of them must agree.
/// `cdata-section-elements` and `use-character-maps` are exempt
/// because they accumulate by spec.
pub fn validate_output_declarations(ast: &StylesheetAst) -> Result<(), XsltError> {
    use crate::ast::OutputSpec;
    // XSLT 2.0 §20 / XTSE1590 — every name listed in
    // `use-character-maps=` (on xsl:output OR on a chained
    // xsl:character-map) must resolve to a declared character map.
    // Check independently of the multi-output conflict pass below
    // so single-output stylesheets are still validated.
    let declared: std::collections::HashSet<String> =
        ast.character_maps.iter().map(|m| qname_key(&m.name)).collect();
    for o in &ast.outputs {
        for q in &o.use_character_maps {
            let k = qname_key(q);
            if !declared.contains(&k) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:output use-character-maps references undeclared map '{k}' \
                     (XTSE1590)"
                )));
            }
        }
    }
    for m in &ast.character_maps {
        for q in &m.use_character_maps {
            let k = qname_key(q);
            if !declared.contains(&k) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:character-map use-character-maps references undeclared map \
                     '{k}' (XTSE1590)"
                )));
            }
        }
    }
    // XSLT 2.0 §20 / XTSE1580 — two character maps sharing a name
    // and the same import precedence are a static error.  We don't
    // currently track per-cm precedence, so this check fires on any
    // duplicate name; conformance tests that exercise the
    // precedence-override path use distinct names per module so no
    // regression there.
    {
        use std::collections::HashSet as HS;
        let mut seen: HS<String> = HS::new();
        for m in &ast.character_maps {
            let k = qname_key(&m.name);
            if !seen.insert(k.clone()) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "duplicate xsl:character-map name '{k}' at the same \
                     import precedence (XTSE1580)"
                )));
            }
        }
    }
    // XSLT 2.0 §20 / XTSE1600 — a character map may not reference
    // itself transitively through use-character-maps.  DFS once per
    // declared name; visiting an in-progress vertex is the cycle.
    use std::collections::{HashMap as HM, HashSet as HS};
    let by_name: HM<String, &Vec<QName>> = ast.character_maps.iter()
        .map(|m| (qname_key(&m.name), &m.use_character_maps))
        .collect();
    fn visit(
        name: &str, on_stack: &mut HS<String>, done: &mut HS<String>,
        by_name: &HM<String, &Vec<QName>>,
    ) -> Result<(), XsltError> {
        if done.contains(name) { return Ok(()); }
        if !on_stack.insert(name.to_string()) {
            return Err(XsltError::InvalidStylesheet(format!(
                "xsl:character-map '{name}' references itself \
                 transitively via use-character-maps (XTSE1600)"
            )));
        }
        if let Some(used) = by_name.get(name) {
            for q in *used {
                visit(&qname_key(q), on_stack, done, by_name)?;
            }
        }
        on_stack.remove(name);
        done.insert(name.to_string());
        Ok(())
    }
    let mut done = HS::new();
    for name in by_name.keys() {
        let mut on_stack = HS::new();
        visit(name, &mut on_stack, &mut done, &by_name)?;
    }
    let outs: Vec<&OutputSpec> = ast.outputs.iter().collect();
    if outs.len() < 2 { return Ok(()); }
    macro_rules! check {
        ($field:ident, $label:expr) => {
            let mut prev: Option<&_> = None;
            for o in &outs {
                if let Some(v) = &o.$field {
                    if let Some(p) = prev {
                        if p != v {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "conflicting xsl:output {}='{:?}' vs '{:?}' \
                                 (XTSE1560)", $label, p, v,
                            )));
                        }
                    } else {
                        prev = Some(v);
                    }
                }
            }
        };
    }
    macro_rules! check_bool {
        ($field:ident, $label:expr) => {
            let mut prev: Option<bool> = None;
            for o in &outs {
                if let Some(v) = o.$field {
                    if let Some(p) = prev {
                        if p != v {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "conflicting xsl:output {}='{}' vs '{}' \
                                 (XTSE1560)", $label, p, v,
                            )));
                        }
                    } else {
                        prev = Some(v);
                    }
                }
            }
        };
    }
    check!(method, "method");
    check!(encoding, "encoding");
    check_bool!(indent, "indent");
    check_bool!(omit_xml_declaration, "omit-xml-declaration");
    check_bool!(standalone, "standalone");
    check!(media_type, "media-type");
    check!(doctype_public, "doctype-public");
    check!(doctype_system, "doctype-system");
    check!(version, "version");
    Ok(())
}

/// XSLT 2.0 §10.1.1 / XTSE0680 — every xsl:with-param child of an
/// xsl:call-template must match a corresponding xsl:param on the
/// called template, both by expanded name AND by tunnel state.
/// A non-tunnel with-param does NOT satisfy a tunnel="yes" param;
/// a tunnel with-param does NOT satisfy a tunnel="no" param.
pub fn validate_call_template_with_params(ast: &StylesheetAst) -> Result<(), XsltError> {
    use crate::ast::Instr;
    use std::collections::HashMap;
    // XSLT 1.0 §11.4 — backwards-compatibility mode silently ignores
    // an unmatched xsl:with-param rather than raising the 2.0-era
    // XTSE0680.  Skip the validator entirely for 1.0 stylesheets so
    // the BC semantics survive even when the host engine compiles
    // them with the 2.0 instruction set.
    if !version_enables_2_0(&ast.version) {
        return Ok(());
    }
    // Build name → template map keyed by Clark form.
    let mut by_name: HashMap<String, &crate::ast::Template> = HashMap::new();
    for t in &ast.templates {
        if let Some(n) = &t.name {
            by_name.insert(qname_key(n), t);
        }
    }
    fn walk_body<'a>(
        body: &'a [Instr],
        by_name: &'a HashMap<String, &'a crate::ast::Template>,
    ) -> Result<(), XsltError> {
        for instr in body {
            match instr {
                Instr::CallTemplate { name, with_params } => {
                    let key = qname_key(name);
                    let Some(t) = by_name.get(&key) else { continue; };
                    for wp in with_params {
                        // XSLT 2.0 §10.1.1 — only NON-tunnel
                        // with-param values need a matching xsl:param
                        // on the callee.  Tunnel params propagate
                        // through; the callee may not declare them
                        // at all (they reach deeper templates).
                        if wp.tunnel { continue; }
                        let wp_key = qname_key(&wp.name);
                        let matched = t.params.iter().any(|p|
                            qname_key(&p.name) == wp_key && !p.tunnel);
                        if !matched {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "xsl:call-template name='{key}' has \
                                 xsl:with-param '{wp_key}' that doesn't \
                                 match any non-tunnel xsl:param of the \
                                 called template (XTSE0680)"
                            )));
                        }
                    }
                }
                Instr::If { body, .. } => walk_body(body, by_name)?,
                Instr::ForEach { body, .. } => walk_body(body, by_name)?,
                Instr::ForEachGroup { body, .. } => walk_body(body, by_name)?,
                Instr::Merge { action, .. } => walk_body(action, by_name)?,
                Instr::Choose { whens, otherwise } => {
                    for (_, b) in whens { walk_body(b, by_name)?; }
                    if let Some(b) = otherwise { walk_body(b, by_name)?; }
                }
                Instr::LiteralElement { body, .. }
                | Instr::Element { body, .. }
                | Instr::Attribute { body, .. }
                | Instr::Copy { body, .. }
                | Instr::Document { body, .. }
                | Instr::ProcessingInstruction { body, .. }
                | Instr::Comment { body, .. }
                | Instr::Namespace { body, .. }
                | Instr::ValueOfBody { body, .. } =>
                    walk_body(body, by_name)?,
                Instr::Try { body, catches } => {
                    walk_body(body, by_name)?;
                    for c in catches { walk_body(&c.body, by_name)?; }
                }
                Instr::PerformSort { body, .. } => walk_body(body, by_name)?,
                Instr::AnalyzeString { matching, non_matching, .. } => {
                    walk_body(matching, by_name)?;
                    walk_body(non_matching, by_name)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    for t in &ast.templates {
        walk_body(&t.body, &by_name)?;
    }
    for f in &ast.functions {
        walk_body(&f.body, &by_name)?;
    }
    Ok(())
}

/// XSLT 3.0 §8.3 static constraints on `xsl:iterate`:
///  * `xsl:break` / `xsl:next-iteration` must be lexically within an
///    `xsl:iterate`, with only `xsl:if` / `xsl:choose` intervening
///    (XTSE3120), and must be the last instruction in their sequence
///    constructor (XTSE0010 tail position);
///  * an `xsl:next-iteration` parameter must name a parameter declared
///    on the enclosing `xsl:iterate` (XTSE3130);
///  * the `xsl:param`s of one `xsl:iterate` must have distinct names.
pub fn validate_iterate_constraints(ast: &StylesheetAst) -> Result<(), XsltError> {
    for t in &ast.templates       { check_iterate(&t.body, None)?; }
    for v in &ast.global_variables { check_iterate(&v.body, None)?; }
    for p in &ast.global_params    { check_iterate(&p.body, None)?; }
    for f in &ast.functions        { check_iterate(&f.body, None)?; }
    for s in &ast.attribute_sets   { check_iterate(&s.attributes, None)?; }
    Ok(())
}

/// Recurse `body` enforcing the `xsl:iterate` constraints.  `params`
/// is `Some(names)` of the directly-enclosing iterate (reached through
/// only `xsl:if` / `xsl:choose`), `None` at any other boundary.
fn check_iterate(
    body: &[Instr],
    params: Option<&std::collections::HashSet<String>>,
) -> Result<(), XsltError> {
    use crate::ast::Instr::*;
    let last = body.len().saturating_sub(1);
    for (i, instr) in body.iter().enumerate() {
        match instr {
            Break { .. } | NextIteration { .. } => {
                let Some(declared) = params else {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:break / xsl:next-iteration must appear within an \
                         xsl:iterate (only xsl:if / xsl:choose may intervene) (XTSE3120)".into(),
                    ));
                };
                if i != last {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:break / xsl:next-iteration must be the last instruction \
                         in its sequence constructor (XTSE0010)".into(),
                    ));
                }
                if let NextIteration { with_params } = instr {
                    for w in with_params {
                        if !declared.contains(&qname_key(&w.name)) {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "xsl:next-iteration sets parameter '{}' not declared on \
                                 the enclosing xsl:iterate (XTSE3130)", w.name.local,
                            )));
                        }
                    }
                }
            }
            // Transparent containers — break/next-iteration may sit
            // inside them and still belong to the enclosing iterate.
            If { body, .. } => check_iterate(body, params)?,
            Choose { whens, otherwise } => {
                for (_, b) in whens { check_iterate(b, params)?; }
                if let Some(b) = otherwise { check_iterate(b, params)?; }
            }
            Iterate { params: ip, on_completion, body, .. } => {
                let mut names = std::collections::HashSet::new();
                for p in ip {
                    if !names.insert(qname_key(&p.name)) {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:iterate has two parameters named '{}' (XTSE0580)", p.name.local,
                        )));
                    }
                    check_iterate(&p.body, None)?;
                }
                // on-completion is not part of the iteration body, so
                // break/next-iteration are not permitted there.
                check_iterate(on_completion, None)?;
                check_iterate(body, Some(&names))?;
            }
            // Every other instruction is a boundary: a break/next-
            // iteration inside it does NOT belong to an outer iterate.
            LiteralElement { body, .. } | Element { body, .. }
            | Attribute { body, .. } | Copy { body, .. }
            | Comment { body, .. } | ProcessingInstruction { body, .. }
            | ForEach { body, .. } | Document { body }
            | Namespace { body, .. } | Message { body, .. }
            | Fallback { body } | PerformSort { body, .. }
            | ValueOfBody { body, .. } => check_iterate(body, None)?,
            ForEachGroup { body, .. } => check_iterate(body, None)?,
            Merge { action, .. } => check_iterate(action, None)?,
            SourceDocument { body, .. } | ResultDocument { body, .. } => check_iterate(body, None)?,
            Fork { body } => check_iterate(body, None)?,
            WherePopulated { body } => check_iterate(body, None)?,
            OnEmpty { body } | OnNonEmpty { body } => check_iterate(body, None)?,
            Map { body } => check_iterate(body, None)?,
            MapEntry { body, .. } => check_iterate(body, None)?,
            AnalyzeString { matching, non_matching, .. } => {
                check_iterate(matching, None)?;
                check_iterate(non_matching, None)?;
            }
            Try { body, catches } => {
                check_iterate(body, None)?;
                for c in catches { check_iterate(&c.body, None)?; }
            }
            ApplyTemplates { with_params, .. } | CallTemplate { with_params, .. }
            | NextMatch { with_params } | ApplyImports { with_params }
            | Evaluate { with_params, .. } => {
                for w in with_params { check_iterate(&w.body, None)?; }
            }
            Variable(v) => check_iterate(&v.body, None)?,
            Unsupported { fallback, .. } => check_iterate(fallback, None)?,
            LiteralText { .. } | ValueOf { .. } | CopyOf { .. }
            | Number { .. } | Sequence { .. } => {}
        }
    }
    Ok(())
}

pub fn validate_named_template_uniqueness(ast: &StylesheetAst) -> Result<(), XsltError> {
    use std::collections::HashMap;
    // Map (name, precedence) → count.
    let mut seen: HashMap<(String, i32), usize> = HashMap::new();
    for t in &ast.templates {
        let Some(name) = &t.name else { continue; };
        let key = qname_key(name);
        *seen.entry((key, t.import_precedence)).or_insert(0) += 1;
    }
    // Per spec, a duplicate at one precedence is OK only if some
    // template with the same name has *higher* precedence
    // (shadowing).  Build a max-precedence-per-name table first.
    let mut max_prec: HashMap<String, i32> = HashMap::new();
    for t in &ast.templates {
        if let Some(name) = &t.name {
            let key = qname_key(name);
            let entry = max_prec.entry(key).or_insert(t.import_precedence);
            if t.import_precedence > *entry { *entry = t.import_precedence; }
        }
    }
    for ((name, prec), count) in &seen {
        if *count < 2 { continue; }
        if max_prec.get(name).copied().unwrap_or(*prec) > *prec { continue; }
        return Err(XsltError::InvalidStylesheet(format!(
            "duplicate xsl:template name='{name}' at the same import \
             precedence ({prec}) with no higher-precedence override \
             (XTSE0660)"
        )));
    }
    Ok(())
}

pub fn validate_attribute_set_refs(ast: &StylesheetAst) -> Result<(), XsltError> {
    let known: std::collections::HashSet<String> = ast.attribute_sets.iter()
        .map(|s| {
            if s.name.uri.is_empty() { s.name.local.clone() }
            else { format!("{{{}}}{}", s.name.uri, s.name.local) }
        })
        .collect();
    let qkey = |q: &QName| -> String {
        if q.uri.is_empty() { q.local.clone() }
        else { format!("{{{}}}{}", q.uri, q.local) }
    };
    let check_list = |list: &[QName]| -> Result<(), XsltError> {
        for q in list {
            if !known.contains(&qkey(q)) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:use-attribute-sets references undeclared set '{}' (XTSE0710)",
                    q.local,
                )));
            }
        }
        Ok(())
    };
    for s in &ast.attribute_sets { check_list(&s.use_attribute_sets)?; }
    for t in &ast.templates {
        walk_attr_set_refs(&t.body, &check_list)?;
    }
    for v in &ast.global_variables { walk_attr_set_refs(&v.body, &check_list)?; }
    for p in &ast.global_params    { walk_attr_set_refs(&p.body, &check_list)?; }
    Ok(())
}

fn walk_attr_set_refs<F>(body: &[Instr], check: &F) -> Result<(), XsltError>
where F: Fn(&[QName]) -> Result<(), XsltError>,
{
    use Instr::*;
    for instr in body {
        match instr {
            LiteralElement { use_attribute_sets, body, .. } => {
                check(use_attribute_sets)?;
                walk_attr_set_refs(body, check)?;
            }
            Element { use_attribute_sets, body, .. } => {
                check(use_attribute_sets)?;
                walk_attr_set_refs(body, check)?;
            }
            Copy { use_attribute_sets, body, .. } => {
                check(use_attribute_sets)?;
                walk_attr_set_refs(body, check)?;
            }
            Attribute { body, .. } | Comment { body, .. }
            | Variable(crate::ast::Variable { body, .. })
            | Message { body, .. } | Fallback { body } => walk_attr_set_refs(body, check)?,
            ApplyTemplates { with_params, .. } | CallTemplate { with_params, .. }
            | Evaluate { with_params, .. } => {
                for w in with_params { walk_attr_set_refs(&w.body, check)?; }
            }
            ForEach { body, .. } => walk_attr_set_refs(body, check)?,
            If { body, .. } => walk_attr_set_refs(body, check)?,
            Choose { whens, otherwise } => {
                for (_, b) in whens { walk_attr_set_refs(b, check)?; }
                if let Some(b) = otherwise { walk_attr_set_refs(b, check)?; }
            }
            ProcessingInstruction { body, .. } => walk_attr_set_refs(body, check)?,
            Unsupported { fallback, .. } => walk_attr_set_refs(fallback, check)?,
            ApplyImports { .. } | LiteralText { .. } | ValueOf { .. } | CopyOf { .. }
            | Number { .. } | Sequence { .. } => {}
            NextMatch { with_params } => {
                for w in with_params { walk_attr_set_refs(&w.body, check)?; }
            }
            ForEachGroup { body, .. } => walk_attr_set_refs(body, check)?,
            Merge { action, .. } => walk_attr_set_refs(action, check)?,
            SourceDocument { body, .. } | ResultDocument { body, .. } => walk_attr_set_refs(body, check)?,
            Fork { body } => walk_attr_set_refs(body, check)?,
            WherePopulated { body } => walk_attr_set_refs(body, check)?,
            OnEmpty { body } | OnNonEmpty { body } => walk_attr_set_refs(body, check)?,
            Map { body } | MapEntry { body, .. } => walk_attr_set_refs(body, check)?,
            AnalyzeString { matching, non_matching, .. } => {
                walk_attr_set_refs(matching, check)?;
                walk_attr_set_refs(non_matching, check)?;
            }
            PerformSort { body, .. } => walk_attr_set_refs(body, check)?,
            Document { body } => walk_attr_set_refs(body, check)?,
            Namespace { body, .. } => walk_attr_set_refs(body, check)?,
            ValueOfBody { body, .. } => walk_attr_set_refs(body, check)?,
            Try { body, catches } => {
                walk_attr_set_refs(body, check)?;
                for c in catches { walk_attr_set_refs(&c.body, check)?; }
            }
            Iterate { params, on_completion, body, .. } => {
                for p in params { walk_attr_set_refs(&p.body, check)?; }
                walk_attr_set_refs(on_completion, check)?;
                walk_attr_set_refs(body, check)?;
            }
            NextIteration { with_params } => {
                for w in with_params { walk_attr_set_refs(&w.body, check)?; }
            }
            Break { body, .. } => walk_attr_set_refs(body, check)?,
        }
    }
    Ok(())
}

fn collect_extension_element_uris(node: &Node) -> std::collections::HashSet<String> {
    let mut prefixes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(node);
    while let Some(n) = cur {
        for attr in n.attributes() {
            let name = attr.name();
            let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
            let in_xsl_ns = attr.namespace.get()
                .map(|ns| ns.href() == "http://www.w3.org/1999/XSL/Transform")
                .unwrap_or(false);
            let bare = attr.namespace.get().is_none() && !name.contains(':');
            if local == "extension-element-prefixes"
                && (bare || in_xsl_ns || name == "xsl:extension-element-prefixes")
            {
                for tok in attr.value().split_whitespace() {
                    prefixes.insert(tok.to_owned());
                }
            }
        }
        cur = n.parent.get();
    }
    if prefixes.is_empty() {
        return std::collections::HashSet::new();
    }
    // Resolve each prefix to its current URI by walking ancestors
    // again; the closer xmlns declaration wins (XML Names § 6.1).
    let mut uris: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cur = Some(node);
    let mut seen_prefix: std::collections::HashSet<String> = std::collections::HashSet::new();
    while let Some(n) = cur {
        for (prefix, uri) in n.ns_declarations() {
            let key = prefix.map(str::to_owned).unwrap_or_else(|| "#default".to_string());
            if seen_prefix.contains(&key) { continue; }
            seen_prefix.insert(key.clone());
            // `#default` matches the default namespace; otherwise
            // match the bound prefix verbatim.
            let bound = match prefix {
                Some(p) => prefixes.contains(p),
                None    => prefixes.contains("#default"),
            };
            if bound && !uri.is_empty() {
                uris.insert(uri.to_owned());
            }
        }
        cur = n.parent.get();
    }
    uris
}

// ── AVT compilation ──────────────────────────────────────────────

/// Compile an XSLT Attribute Value Template into a sequence of
/// literal and expression parts.  XSLT 1.0 §7.6.2:
///
/// * Outside `{` `}`: literal text.  Doubled `{{` and `}}` denote
///   single `{` and `}` in the output.
/// * Inside `{` ... `}`: an XPath expression.  The expression may
///   itself contain `}` inside string literals — we track a
///   shallow quote state to avoid prematurely closing.
/// True iff the AVT-expression text contains nothing but whitespace
/// and XPath 2.0 `(: … :)` comments — `{}`, `{ }`, `{ (:c:) }` etc.
/// XSLT 2.0 §5.6.1 (bug 29226) treats these as no-op AVT
/// expressions; the surrounding literal text is the only output.
fn avt_is_empty_expr(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {}
            '(' if chars.peek() == Some(&':') => {
                chars.next();
                // Skip until matching `:)` (nesting allowed).
                let mut depth = 1u32;
                while let Some(cc) = chars.next() {
                    if cc == '(' && chars.peek() == Some(&':') {
                        chars.next(); depth += 1;
                    } else if cc == ':' && chars.peek() == Some(&')') {
                        chars.next(); depth -= 1;
                        if depth == 0 { break; }
                    }
                }
                if depth > 0 { return false; }
            }
            _ => return false,
        }
    }
    true
}

fn avt(node: &Node, s: &str) -> Result<Avt, XsltError> {
    // Embedded `{expr}` substitutions inherit the surrounding element's
    // XPath-1.0 backwards-compatibility scope (XSLT 2.0 §3.8): in a
    // `[xsl:]version="1.0"` scope a sequence-valued AVT takes the first
    // item only.  Computed once per AVT — every part shares the node.
    let bc = ancestor_forces_backwards_compat(node);
    let mut parts = Vec::new();
    let mut buf = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                buf.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                buf.push('}');
            }
            '{' => {
                if !buf.is_empty() {
                    parts.push(AvtPart::Literal(std::mem::take(&mut buf)));
                }
                let mut expr_text = String::new();
                let mut in_apos = false;
                let mut in_quot = false;
                let mut paren_depth = 0u32;
                let mut bracket_depth = 0u32;
                // XPath 2.0 `(: … :)` comments nest; depth tracks
                // the open count so a `}` inside a comment is not
                // mistaken for the end of the AVT expression.
                let mut comment_depth = 0u32;
                let mut closed = false;
                while let Some(ec) = chars.next() {
                    if comment_depth > 0 {
                        // Inside a comment — only `(:` (open nested)
                        // and `:)` (close) matter.
                        if ec == '(' && chars.peek() == Some(&':') {
                            chars.next();
                            comment_depth += 1;
                            expr_text.push('(');
                            expr_text.push(':');
                        } else if ec == ':' && chars.peek() == Some(&')') {
                            chars.next();
                            comment_depth -= 1;
                            expr_text.push(':');
                            expr_text.push(')');
                        } else {
                            expr_text.push(ec);
                        }
                        continue;
                    }
                    match ec {
                        '\'' if !in_quot => { in_apos = !in_apos; expr_text.push(ec); }
                        '"'  if !in_apos => { in_quot = !in_quot; expr_text.push(ec); }
                        // XPath 2.0 comment open `(:` — only when
                        // not inside a string literal.
                        '(' if !in_apos && !in_quot && chars.peek() == Some(&':') => {
                            chars.next();
                            comment_depth += 1;
                            expr_text.push('(');
                            expr_text.push(':');
                        }
                        // Track paren / bracket depth so a `}`
                        // inside `(...)` or `[...]` doesn't end the
                        // AVT prematurely.  Strings (handled above)
                        // already opt out of this counting.
                        '(' if !in_apos && !in_quot => { paren_depth += 1;   expr_text.push(ec); }
                        ')' if !in_apos && !in_quot && paren_depth > 0
                            => { paren_depth -= 1; expr_text.push(ec); }
                        '[' if !in_apos && !in_quot => { bracket_depth += 1;   expr_text.push(ec); }
                        ']' if !in_apos && !in_quot && bracket_depth > 0
                            => { bracket_depth -= 1; expr_text.push(ec); }
                        '}' if !in_apos && !in_quot && paren_depth == 0
                              && bracket_depth == 0 => { closed = true; break; }
                        _ => expr_text.push(ec),
                    }
                }
                if !closed {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "unterminated AVT expression in '{s}'"
                    )));
                }
                // Empty (`{}`) or whitespace/comment-only (`{ }`,
                // `{ (:c:) }`) expressions are valid AVTs that
                // contribute nothing to the result (XSLT 2.0 §5.6.1,
                // bug 29226 fix in the spec).  Skip them rather than
                // erroring out in the XPath parser.
                if avt_is_empty_expr(&expr_text) {
                    continue;
                }
                let mut expr = parse_xpath(&expr_text).map_err(XsltError::from)?;
                if bc {
                    expr = Expr::BackwardsCompat(Box::new(expr));
                }
                parts.push(AvtPart::Expr(expr));
            }
            '}' => {
                return Err(XsltError::InvalidStylesheet(format!(
                    "unmatched '}}' in AVT '{s}' (use '}}}}' for a literal '}}')"
                )));
            }
            other => buf.push(other),
        }
    }
    if !buf.is_empty() {
        parts.push(AvtPart::Literal(buf));
    }
    Ok(Avt { parts })
}

#[allow(dead_code)]
fn avt_from_body_or_value(node: &Node) -> Result<Avt, XsltError> {
    // xsl:attribute (inside attribute-set) has a body sequence
    // constructor; we currently support the common case of a
    // single text child as an AVT.  Element-bearing bodies are
    // routed through the evaluator and not via this AVT helper.
    let mut buf = String::new();
    for child in node.children() {
        match child.kind {
            NodeKind::Text | NodeKind::CData => buf.push_str(child.content()),
            _ => {}
        }
    }
    avt(node, &buf)
}

// ── helpers ───────────────────────────────────────────────────────

fn is_xslt_element(node: &Node) -> bool {
    node.is_element()
        && node.namespace.get().map(|ns| ns.href()) == Some(XSLT_NS)
}

/// True if `node` carries an `xsl:version` attribute — the marker
/// that triggers XSLT 1.0 §2.3's simplified-stylesheet shorthand.
fn has_xsl_version_attr(node: &Node) -> bool {
    node.attributes().any(|a| {
        a.name().rsplit(':').next() == Some("version")
            && a.namespace.get().map(|ns| ns.href()) == Some(XSLT_NS)
    })
}

/// Compile a simplified-stylesheet root (XSLT 1.0 §2.3): the root
/// element is itself a literal result element representing the body
/// of a single `xsl:template match="/"`.  We surface this as if the
/// stylesheet author had written
/// `<xsl:stylesheet><xsl:template match="/">…</xsl:template></xsl:stylesheet>`.
fn compile_simplified(root: &Node) -> Result<StylesheetAst, XsltError> {
    let mut ast = StylesheetAst::default();
    // Capture the version from xsl:version for accurate diagnostics
    // and to gate XSLT 2.0 instruction compilation in the body.
    ast.version = root
        .attributes()
        .find(|a| {
            a.name().rsplit(':').next() == Some("version")
                && a.namespace.get().map(|ns| ns.href()) == Some(XSLT_NS)
        })
        .map(|a| a.value().to_string())
        .unwrap_or_default();
    // Honour the simplified-stylesheet's `xsl:version` for
    // instruction-set selection: a `<root xsl:version="2.0">` body
    // can contain `xsl:for-each-group`, `xsl:sequence`,
    // `xsl:perform-sort`, etc. and we'd otherwise reject them with
    // "not implemented in this build".  Drop guard restores the
    // previous mode when this fn returns.
    let xslt_2_0_on = version_enables_2_0(&ast.version);
    let in_fwd_compat = version_is_greater_than(&ast.version, "2.0");
    let _xslt_mode = XsltModeGuard::enter_full(xslt_2_0_on, true, in_fwd_compat);
    for (prefix, uri) in root.ns_declarations() {
        match prefix {
            Some(p) => { ast.namespaces.insert(p.to_string(), uri.to_string()); }
            None    => { ast.namespaces.insert(String::new(), uri.to_string()); }
        }
    }
    // Build a template that matches the document root and whose body
    // is the simplified-stylesheet's root LRE.
    let lre = compile_literal_element(root)?;
    let template = crate::ast::Template {
        match_pattern: Some(parse_xpath("/").map_err(XsltError::from)?),
        name: None,
        mode: None,
        modes: Vec::new(),
        modes_match_all: false,
        priority: None,
        import_precedence: TOP_LEVEL_IMPORT_PRECEDENCE,
        source_path: vec![0],
        params: Vec::new(),
        body: vec![lre],
        as_type: None,
    };
    ast.templates.push(template);
    ast.documents_to_load = crate::walk::collect_static_document_uris(&ast);
    Ok(ast)
}

fn node_qname(node: &Node) -> Result<QName, XsltError> {
    let ns_href = node.namespace.get().map(|ns| ns.href().to_string()).unwrap_or_default();
    let ns_pref = node.namespace.get().and_then(|ns| ns.prefix()).map(str::to_string);
    Ok(QName {
        prefix: ns_pref,
        local:  node.local_name().to_string(),
        uri:    ns_href,
    })
}

fn attr_qname(_parent: &Node, attr: &Attribute) -> Result<QName, XsltError> {
    let n = attr.name();
    let (prefix, local) = match n.split_once(':') {
        Some((p, l)) => (Some(p.to_string()), l.to_string()),
        None         => (None, n.to_string()),
    };
    let uri = attr.namespace.get().map(|ns| ns.href().to_string()).unwrap_or_default();
    Ok(QName { prefix, local, uri })
}

/// Read an attribute's value by local name only — XSLT attributes
/// on XSLT elements live in *no* namespace (XSLT 1.0 §2.7), so
/// matching by local name is correct for the common case of
/// reading `select` / `match` / `name` etc. off an xsl:* element.
/// Walk `node` and every descendant element, adding each
/// `xmlns:p="…"` (or default `xmlns="…"`) binding to `map`
/// using `entry().or_insert(...)` so outer / earlier bindings
/// win.  Best-effort approximation of XSLT 2.0's per-element
/// in-scope namespace rule — sufficient for stylesheets that
/// declare a prefix on a template and use it in that template's
/// XPath expressions.
fn collect_inner_ns(node: &Node, map: &mut std::collections::HashMap<String, String>) {
    for child in node.children() {
        if !child.is_element() { continue; }
        for (prefix, uri) in child.ns_declarations() {
            let key = match prefix {
                Some(p) => p.to_string(),
                None    => String::new(),
            };
            map.entry(key).or_insert_with(|| uri.to_string());
        }
        collect_inner_ns(child, map);
    }
}

fn read_attribute<'doc>(node: &'doc Node, local: &str) -> Option<&'doc str> {
    for attr in node.attributes() {
        let n = attr.name();
        // Skip any prefixed attribute; XSLT attributes on XSLT
        // elements are unprefixed by spec.
        if attr.namespace.get().is_some() || n.contains(':') { continue; }
        if n == local { return Some(attr.value()); }
    }
    None
}

/// Read an XSLT-namespaced attribute (`xsl:NAME`) on a non-XSLT
/// element.  Used for attributes the XSLT 2.0 spec mandates appear
/// on literal result elements with the `xsl:` prefix: `xsl:version`,
/// `xsl:use-when`, `xsl:exclude-result-prefixes`,
/// `xsl:extension-element-prefixes`, etc.  Matches by local name and
/// XSLT namespace URI rather than the raw `xsl:` prefix so a
/// stylesheet that bound the XSLT URI to a different prefix still
/// works.
fn read_xsl_attribute<'doc>(node: &'doc Node, local: &str) -> Option<&'doc str> {
    for attr in node.attributes() {
        let n = attr.name();
        let attr_local = n.rsplit_once(':').map(|(_, l)| l).unwrap_or(n);
        if attr_local != local { continue; }
        let in_xsl_ns = attr.namespace.get()
            .map(|ns| ns.href() == "http://www.w3.org/1999/XSL/Transform")
            .unwrap_or(false);
        if in_xsl_ns { return Some(attr.value()); }
    }
    None
}

fn require_attr<'doc>(
    node: &'doc Node, name: &str, who: &str,
) -> Result<&'doc str, XsltError> {
    read_attribute(node, name).ok_or_else(|| XsltError::InvalidStylesheet(
        format!("{who} requires {name}= attribute"),
    ))
}

fn required_qname_attr(
    node: &Node, name: &str, who: &str,
) -> Result<QName, XsltError> {
    let raw = require_attr(node, name, who)?;
    let qn = parse_qname_on(node, raw)?;
    // Required name= attributes on `xsl:template`, `xsl:variable`,
    // `xsl:key`, `xsl:attribute-set`, `xsl:function`, `xsl:param`,
    // `xsl:with-param`, etc. must be valid QNames (XML Names §3).
    // XSLT 2.0 raises XTSE0020 / XTSE0280 on names like `12foo` or
    // `name/1223` — surface those via this single chokepoint so the
    // wildcard-bearing attributes (`xsl:strip-space elements="*"`,
    // pattern qnames) keep going through `parse_qname_on` without
    // the strict check.
    if !is_valid_ncname(&qn.local) {
        return Err(XsltError::InvalidStylesheet(format!(
            "{who}: '{raw}' is not a valid QName (XTSE0020)"
        )));
    }
    if let Some(p) = &qn.prefix {
        if !is_valid_ncname(p) {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who}: '{raw}' has an invalid prefix '{p}' (XTSE0020)"
            )));
        }
    }
    Ok(qn)
}

/// Parse a `prefix:local` (or `local`) string against the
/// namespace declarations in scope at `context_node`.  The
/// stylesheet's xmlns declarations supply the prefix→URI map for
/// every QName-bearing attribute (`mode`, `match`-pattern names,
/// `use-attribute-sets`, etc.).
/// Parse one of the NameTest tokens accepted by xsl:strip-space /
/// xsl:preserve-space `elements=` (and xsl:template `mode="#default"`
/// etc. extensions): a plain QName, or one of `*`, `prefix:*`,
/// `*:local`.  The wildcard forms encode as a QName whose local /
/// prefix is `*`, which the matching path interprets.
fn parse_name_test_token(context_node: &Node, tok: &str) -> Result<QName, XsltError> {
    if tok == "*" {
        return Ok(QName { prefix: None, local: "*".into(), uri: String::new() });
    }
    if let Some(prefix) = tok.strip_suffix(":*") {
        // Resolve `prefix` to a URI for "everything in this namespace".
        let mut uri = String::new();
        let mut cur = Some(context_node);
        while let Some(n) = cur {
            for (p, href) in n.ns_declarations() {
                if p == Some(prefix) { uri = href.to_string(); break; }
            }
            if !uri.is_empty() { break; }
            cur = n.parent.get();
        }
        if uri.is_empty() {
            if prefix == "xml" {
                uri = "http://www.w3.org/XML/1998/namespace".to_string();
            } else {
                // XSLT 2.0 §5.1 — a `prefix:*` NameTest whose prefix is not
                // declared in scope is a static error.
                return Err(XsltError::InvalidStylesheet(format!(
                    "undeclared namespace prefix '{prefix}' in NameTest \
                     '{tok}' (XTSE0280)"
                )));
            }
        }
        return Ok(QName {
            prefix: Some(prefix.to_string()),
            local:  "*".into(), uri,
        });
    }
    if let Some(local) = tok.strip_prefix("*:") {
        return Ok(QName {
            prefix: Some("*".into()),
            local:  local.to_string(),
            uri:    String::new(),
        });
    }
    parse_qname_on(context_node, tok)
}

fn parse_qname_on(context_node: &Node, s: &str) -> Result<QName, XsltError> {
    let s = s.trim();
    let (prefix, local) = match s.split_once(':') {
        Some((p, l)) => (Some(p.to_string()), l.to_string()),
        None         => (None, s.to_string()),
    };
    // XML Names: both parts of a QName must be valid NCNames.  An
    // empty local part, a `/` or space in the name, or a leading
    // digit all violate the production — surface as XTSE0020 so the
    // W3C suite's "bad QName" cases (template/0020e, decimal-
    // format/0020b after AVT detection, etc.) line up.
    if !is_valid_ncname(&local) {
        return Err(XsltError::InvalidStylesheet(format!(
            "qname '{s}' is not a valid QName (XTSE0020)"
        )));
    }
    if let Some(p) = &prefix {
        if !is_valid_ncname(p) {
            return Err(XsltError::InvalidStylesheet(format!(
                "qname '{s}' has invalid prefix (XTSE0020)"
            )));
        }
    }
    let uri = match &prefix {
        None => String::new(),
        Some(p) => {
            // Walk up ancestor chain looking for the prefix binding.
            let mut cur = Some(context_node);
            let mut found = None;
            while let Some(n) = cur {
                for (pref, href) in n.ns_declarations() {
                    if pref == Some(p.as_str()) {
                        found = Some(href.to_string());
                        break;
                    }
                }
                if found.is_some() { break; }
                cur = n.parent.get();
            }
            found.ok_or_else(|| XsltError::InvalidStylesheet(format!(
                "qname '{s}' references undeclared prefix '{p}'"
            )))?
        }
    };
    Ok(QName { prefix, local, uri })
}

/// XSLT 2.0 §5.5 — patterns are a restricted XPath subset that
/// only permits the child / attribute / descendant-or-self axes
/// in step positions.  This catches the spec-blessed XTSE0340 the
/// W3C suite exercises on xsl:key match='namespace::*' and
/// match='ancestor::foo'.  Predicates may contain any XPath
/// expression so we don't recurse into them.
fn reject_invalid_pattern_axes(expr: &Expr, who: &str) -> Result<(), XsltError> {
    use sup_xml_core::xpath::ast::{Axis, LocationPath};
    fn walk_step(s: &sup_xml_core::xpath::ast::Step, who: &str) -> Result<(), XsltError> {
        let ok = matches!(s.axis,
            Axis::Child | Axis::Attribute | Axis::DescendantOrSelf | Axis::Self_);
        if !ok {
            return Err(XsltError::InvalidStylesheet(format!(
                "{who} pattern axis '{:?}' not permitted in a pattern (XTSE0340)",
                s.axis,
            )));
        }
        Ok(())
    }
    fn walk(e: &Expr, who: &str) -> Result<(), XsltError> {
        match e {
            Expr::Path(LocationPath::Absolute(steps))
            | Expr::Path(LocationPath::Relative(steps)) => {
                for s in steps { walk_step(s, who)?; }
            }
            Expr::FilterPath { primary, steps, .. } => {
                // XSLT 2.0 §5.5.2 PatternStep grammar — a FilterPath
                // primary at the head of a pattern must be a call to
                // id() or key().  Variable references and arbitrary
                // function calls (doc(), normalize-space(), …) are
                // XSLT 3.0+ extensions.
                match primary.as_ref() {
                    Expr::FunctionCall(name, _) => {
                        let local = name.rsplit_once(':')
                            .map(|(_, l)| l).unwrap_or(name);
                        if !matches!(local, "id" | "key") {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "{who} pattern primary '{local}(...)' is not \
                                 id() or key() — only those function calls may \
                                 head a pattern (XSLT 2.0 §5.5.2)"
                            )));
                        }
                    }
                    Expr::Variable(name) => return Err(XsltError::InvalidStylesheet(format!(
                        "{who} pattern starts with a variable reference \
                         '${name}' — variables aren't permitted at the head \
                         of an XSLT 2.0 pattern (XPST0003)"
                    ))),
                    _ => {}
                }
                for s in steps { walk_step(s, who)?; }
            }
            Expr::Variable(name) => return Err(XsltError::InvalidStylesheet(format!(
                "{who} pattern '${name}' — a bare variable reference is \
                 not a legal XSLT 2.0 pattern (XPST0003)"
            ))),
            Expr::Union(a, b) => { walk(a, who)?; walk(b, who)?; }
            _ => {}
        }
        Ok(())
    }
    walk(expr, who)
}

/// XSLT 2.0 §5.5.3 — patterns and the `count` / `from` attributes
/// of xsl:number may not call functions that depend on a dynamic
/// grouping or regex context (current-group / current-grouping-key
/// / regex-group).  Walks the parsed expression for those names
/// and surfaces XTSE1060 / XTSE1070 / XTSE1140.
/// XSLT 2.0 §5.5.3 — when a pattern uses `key(name, value)`, the
/// `name` argument must be a string literal and the `value`
/// argument must be a string literal or a reference to a variable.
/// Anything else (arithmetic, function call, …) is XPST0017.
fn reject_invalid_pattern_key_calls(expr: &Expr, who: &str) -> Result<(), XsltError> {
    fn walk(e: &Expr, who: &str) -> Result<(), XsltError> {
        match e {
            Expr::FunctionCall(name, args) => {
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
                if local == "id" {
                    // XSLT 2.0 §5.5.3 — `id()` in a pattern must take
                    // exactly one argument (a string literal or a
                    // variable reference).  The 2-argument form
                    // `id($ids, $doc)` is XSLT 2.1+ / XPath 2.0
                    // (selecting a different document) and isn't
                    // permitted at the head of an XSLT 2.0 pattern.
                    if args.len() != 1 {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "{who} pattern call id(): must have exactly one \
                             argument — the 2-arg form is not permitted in \
                             an XSLT 2.0 pattern (XTSE0340)"
                        )));
                    }
                    if !matches!(args[0], Expr::Literal(_) | Expr::Variable(_)) {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "{who} pattern call id(): argument must be a \
                             string literal or variable reference (XPST0017)"
                        )));
                    }
                }
                if local == "key" && args.len() >= 1 {
                    if !matches!(args[0], Expr::Literal(_)) {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "{who} pattern call key(): first argument must be a \
                             string literal (XPST0017)"
                        )));
                    }
                    if args.len() >= 2 {
                        // XSLT 2.0 §5.5.3 names string literals,
                        // variable references, and FilterPaths that
                        // start with a variable (e.g. `$x/y`) as
                        // permissible.  Reject only the unambiguously
                        // illegal forms: arithmetic / FunctionCall /
                        // path expressions that start from the context.
                        // XSLT 2.0 §5.5.3 allows any literal that is
                        // castable to xs:string (string / numeric)
                        // plus a variable reference (including a
                        // FilterPath rooted at a variable, e.g.
                        // `$v/key`).  Arithmetic / FunctionCall in
                        // the 2nd argument position is XPST0017.
                        let ok = matches!(args[1],
                            Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_)
                            | Expr::Variable(_)
                            | Expr::FilterPath { .. });
                        if !ok {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "{who} pattern call key(): second argument must be a \
                                 literal or variable reference (XPST0017)"
                            )));
                        }
                    }
                }
                for a in args { walk(a, who)?; }
                Ok(())
            }
            Expr::FilterPath { primary, predicates, steps } => {
                walk(primary, who)?;
                for p in predicates { walk(p, who)?; }
                for s in steps { for p in &s.predicates { walk(p, who)?; } }
                Ok(())
            }
            Expr::Union(a, b) => { walk(a, who)?; walk(b, who)?; Ok(()) }
            _ => Ok(()),
        }
    }
    walk(expr, who)
}

/// Reject expressions whose top-level shape can never be a Pattern
/// (XSLT 2.0 §5.5.2 — Pattern is a union of PathPatterns, each a
/// restricted path expression).  The XPath grammar is a superset of
/// the pattern grammar, so a misused arithmetic / comparison /
/// conditional expression parses successfully but is XTSE0340.  We
/// check only the unmistakable cases here; legal patterns whose inner
/// predicates use any of these constructs pass through untouched.
fn ensure_pattern_shape(expr: &Expr, who: &str) -> Result<(), XsltError> {
    use sup_xml_core::xpath::ast::{Expr as E, LocationPath};
    let illegal = matches!(expr,
        E::Add(_, _) | E::Sub(_, _) | E::Mul(_, _) | E::Div(_, _)
        | E::Mod(_, _) | E::IDiv(_, _) | E::Neg(_)
        | E::And(_, _) | E::Or(_, _)
        | E::Eq(_, _) | E::Ne(_, _)
        | E::Lt(_, _) | E::Gt(_, _) | E::Le(_, _) | E::Ge(_, _)
        | E::ValueEq(_, _) | E::ValueNe(_, _)
        | E::ValueLt(_, _) | E::ValueGt(_, _)
        | E::ValueLe(_, _) | E::ValueGe(_, _)
        | E::IfThenElse { .. } | E::For { .. } | E::Let { .. }
        | E::Quantified { .. } | E::Range(_, _));
    if illegal {
        return Err(XsltError::InvalidStylesheet(format!(
            "{who} is not a valid pattern (XTSE0340)"
        )));
    }
    // XSLT 2.0 §5.5.2 pattern grammar — function calls inside a path
    // are restricted to `id(...)` / `key(...)` and only as the
    // LEADING step of a relative pattern (not after `/`).  Other
    // function calls anywhere in the path, or id/key after another
    // step, are XPST0017.
    fn check_path_steps(
        steps: &[sup_xml_core::xpath::ast::Step], absolute: bool,
        who: &str,
    ) -> Result<(), XsltError> {
        for (i, s) in steps.iter().enumerate() {
            let Some(filter) = &s.filter else { continue; };
            match filter.as_ref() {
                Expr::FunctionCall(name, _) => {
                    let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
                    let is_id_key = matches!(local, "id" | "key");
                    // `key()` / `id()` allowed only as the leading step of
                    // a *relative* pattern: `key(...)/foo` is OK, but
                    // `/key(...)` or `foo/key(...)` is not.
                    let allowed = is_id_key && i == 0 && !absolute;
                    if !allowed {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "{who} uses a function call '{local}()' in a position \
                             that the pattern grammar doesn't permit (XPST0017)"
                        )));
                    }
                }
                // XSLT 2.0 §5.5.2 — a step expression in a pattern is
                // an AxisStep, an IdKeyPattern, or `(` pattern `)`.
                // Parenthesised general expressions, sequences, and
                // unions inside a path (e.g. `/(a|b)` or `//(bar|baz)`)
                // aren't admitted by the grammar — XPST0003 / XPST0017.
                Expr::Sequence(_) | Expr::Union(_, _) | Expr::Range(_, _) => {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "{who} contains a parenthesised expression in a path \
                         step that the pattern grammar doesn't permit (XPST0003)"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }
    if let E::Path(LocationPath::Relative(steps)) = expr {
        check_path_steps(steps, false, who)?;
    }
    if let E::Path(LocationPath::Absolute(steps)) = expr {
        check_path_steps(steps, true, who)?;
    }
    if let E::Union(l, r) = expr {
        ensure_pattern_shape(l, who)?;
        ensure_pattern_shape(r, who)?;
    }
    Ok(())
}

fn reject_pattern_grouping_calls(expr: &Expr, who: &str) -> Result<(), XsltError> {
    fn walk(e: &Expr) -> Option<(&'static str, &'static str)> {
        use sup_xml_core::xpath::ast::LocationPath;
        match e {
            Expr::FunctionCall(name, args) => {
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
                match local {
                    "current-group"        => return Some(("current-group()",       "XTSE1060")),
                    "current-grouping-key" => return Some(("current-grouping-key()","XTSE1070")),
                    "regex-group"          => return Some(("regex-group()",         "XTSE1140")),
                    _ => {}
                }
                for a in args { if let Some(r) = walk(a) { return Some(r); } }
                None
            }
            Expr::Or(a, b) | Expr::And(a, b)
            | Expr::Eq(a, b) | Expr::Ne(a, b)
            | Expr::Lt(a, b) | Expr::Gt(a, b) | Expr::Le(a, b) | Expr::Ge(a, b)
            | Expr::ValueEq(a, b) | Expr::ValueNe(a, b)
            | Expr::ValueLt(a, b) | Expr::ValueGt(a, b)
            | Expr::ValueLe(a, b) | Expr::ValueGe(a, b)
            | Expr::Add(a, b) | Expr::Sub(a, b)
            | Expr::Mul(a, b) | Expr::Div(a, b) | Expr::Mod(a, b)
            | Expr::Union(a, b)
            | Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b)
            | Expr::Range(a, b) | Expr::SimpleMap(a, b)
            | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) =>
                walk(a).or_else(|| walk(b)),
            Expr::Neg(a)
            | Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
            | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => walk(a),
            Expr::Sequence(args) => {
                for a in args { if let Some(r) = walk(a) { return Some(r); } }
                None
            }
            Expr::IfThenElse { cond, then_branch, else_branch } =>
                walk(cond).or_else(|| walk(then_branch)).or_else(|| walk(else_branch)),
            Expr::For { bindings, body } | Expr::Let { bindings, body } | Expr::Quantified { bindings, test: body, .. } => {
                for (_, e) in bindings { if let Some(r) = walk(e) { return Some(r); } }
                walk(body)
            }
            Expr::FilterPath { primary, predicates, steps } => {
                if let Some(r) = walk(primary) { return Some(r); }
                for p in predicates { if let Some(r) = walk(p) { return Some(r); } }
                for s in steps { for p in &s.predicates { if let Some(r) = walk(p) { return Some(r); } } }
                None
            }
            Expr::Path(p) => {
                let steps = match p { LocationPath::Absolute(s) | LocationPath::Relative(s) => s };
                for s in steps { for p in &s.predicates { if let Some(r) = walk(p) { return Some(r); } } }
                None
            }
            Expr::TryCatch { body, catches } => {
                if let Some(r) = walk(body) { return Some(r); }
                for c in catches { if let Some(r) = walk(&c.body) { return Some(r); } }
                None
            }
            Expr::WithDefaultCollation(_, inner) => walk(inner),
            Expr::BackwardsCompat(inner) => walk(inner),
            Expr::MapConstructor(es) => es.iter()
                .find_map(|(k, v)| walk(k).or_else(|| walk(v))),
            Expr::ArrayConstructor { members, .. } => members.iter().find_map(walk),
            Expr::Lookup(b, key) => walk(b).or_else(||
                match key {
                    sup_xml_core::xpath::ast::LookupKey::Expr(e) => walk(e),
                    _ => None,
                }),
            Expr::UnaryLookup(key) => match key {
                sup_xml_core::xpath::ast::LookupKey::Expr(e) => walk(e),
                _ => None,
            },
            Expr::InlineFunction { body, .. } => walk(body),
            Expr::DynamicCall { func, args } =>
                walk(func).or_else(|| args.iter().find_map(walk)),
            Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => None,
            Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) | Expr::Variable(_) => None,
        }
    }
    if let Some((fname, code)) = walk(expr) {
        return Err(XsltError::InvalidStylesheet(format!(
            "{fname} cannot be used in {who} (a pattern context — {code})"
        )));
    }
    Ok(())
}

/// XSLT 2.0 §16.4.1 / XTSE1295 — a valid zero-digit is the
/// character with Unicode numeric value 0 *and* General_Category
/// Nd (decimal digit).  ASCII `'0'` is the common case; other
/// scripts have their own (Arabic-Indic 0660, Devanagari 0966,
/// Bengali 09E6, etc.).  We cover the well-known sets the W3C
/// suite exercises.
fn is_unicode_zero_digit(c: char) -> bool {
    // The full Unicode Nd category lists ~30 zero-digit
    // codepoints; the suite only needs the common scripts plus
    // a sentinel for ASCII.
    matches!(c as u32,
        0x0030 |              // ASCII 0
        0x0660 |              // Arabic-Indic
        0x06F0 |              // Extended Arabic-Indic
        0x07C0 |              // Nko
        0x0966 |              // Devanagari
        0x09E6 |              // Bengali
        0x0A66 |              // Gurmukhi
        0x0AE6 |              // Gujarati
        0x0B66 |              // Oriya
        0x0BE6 |              // Tamil (only 1-9 normally; 0 exists)
        0x0C66 |              // Telugu
        0x0CE6 |              // Kannada
        0x0D66 |              // Malayalam
        0x0DE6 |              // Sinhala Lith
        0x0E50 |              // Thai
        0x0ED0 |              // Lao
        0x0F20 |              // Tibetan
        0x1040 |              // Myanmar
        0x1090 |              // Myanmar Shan
        0x17E0 |              // Khmer
        0x1810 |              // Mongolian
        0x1946 |              // Limbu
        0x19D0 |              // New Tai Lue
        0x1A80 | 0x1A90 |     // Tai Tham Hora / Tham
        0x1B50 |              // Balinese
        0x1BB0 |              // Sundanese
        0x1C40 |              // Lepcha
        0x1C50 |              // Ol Chiki
        0xA620 |              // Vai
        0xA8D0 |              // Saurashtra
        0xA900 |              // Kayah Li
        0xA9D0 |              // Javanese
        0xA9F0 |              // Myanmar Tai Laing
        0xAA50 |              // Cham
        0xABF0 |              // Meetei Mayek
        0xFF10 |              // Fullwidth
        0x104A0 |             // Osmanya
        0x11066 |             // Brahmi
        0x110F0 |             // Sora Sompeng
        0x11136 |             // Chakma
        0x111D0 |             // Sharada
        0x112F0 |             // Khudawadi
        0x114D0 |             // Tirhuta
        0x11650 |             // Modi
        0x116C0 |             // Takri
        0x11730 |             // Ahom
        0x118E0 |             // Warang Citi
        0x11C50 |             // Bhaiksuki
        0x11D50 |             // Masaram Gondi
        0x11DA0 |             // Gunjala Gondi
        0x16A60 |             // Mro
        0x16B50 |             // Pahawh Hmong
        0x1D7CE | 0x1D7D8 | 0x1D7E2 | 0x1D7EC | 0x1D7F6 |
                              // Mathematical digits (Bold/Italic/etc.)
        0x1E950                // Adlam
    )
}

/// XSLT 2.0 §16.4 — collations the processor recognises.  Today
/// only the codepoint collation is implemented; everything else
/// (Saxon's `?strength=primary` URIs, ICU collation URIs) is
/// rejected when surfaced via an xsl:key / xsl:sort / fn:compare
/// `collation=` parameter.
pub(crate) fn is_recognised_collation(uri: &str) -> bool {
    let t = uri.trim();
    // AVT-containing values are resolved at runtime — let them
    // through statically and validate (or trust) at apply time.
    if t.contains('{') { return true; }
    matches!(t,
        "" | "http://www.w3.org/2005/xpath-functions/collation/codepoint"
        | "http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive")
}

/// Clark-form key for a QName — `local` when there's no URI,
/// `{uri}local` otherwise.  Mirrors the runtime evaluator's
/// `qname_key` so duplicate-name diagnostics here use the same
/// comparison shape as later lookup.
fn qname_key(q: &QName) -> String {
    if q.uri.is_empty() { q.local.clone() }
    else { format!("{{{uri}}}{local}", uri = q.uri, local = q.local) }
}

/// XSLT 2.0 §3.7 — namespaces reserved for processor use, where a
/// stylesheet may not declare named constructs (templates,
/// functions, variables, parameters, keys, modes, attribute-sets,
/// decimal-formats, output definitions, character maps).  Covers
/// the XSLT namespace, the XPath / XQuery function namespaces, the
/// XML Schema namespaces, and the XSI namespace.  W3C XSLT 2.0
/// expected-error tests for XTSE0080 exercise xs: and fn: as well
/// as xsl:.
const RESERVED_NAME_URIS: &[&str] = &[
    "http://www.w3.org/1999/XSL/Transform",
    "http://www.w3.org/2005/xpath-functions",
    "http://www.w3.org/2005/xpath-functions/math",
    "http://www.w3.org/2005/xpath-functions/map",
    "http://www.w3.org/2005/xpath-functions/array",
    "http://www.w3.org/2005/xquery-local-functions",
    "http://www.w3.org/2001/XMLSchema",
    "http://www.w3.org/2001/XMLSchema-instance",
];

/// XSLT 2.0 §6.5 — the named-construct rejection rule.  Surface
/// XTSE0080 when the construct's `name=` attribute resolves to a
/// reserved namespace URI.
///
/// `xsl:initial-template` is the XSLT 3.0 §2.4 well-known entry
/// point: the only spec-reserved template name a stylesheet is
/// allowed to declare under the XSLT namespace.  All other
/// xsl:* / fn:* / xs:* / xsi:* names remain rejected.
fn reject_reserved_name(name: &QName, who: &str) -> Result<(), XsltError> {
    let is_initial_template = name.uri == "http://www.w3.org/1999/XSL/Transform"
        && name.local == "initial-template";
    if !is_initial_template && RESERVED_NAME_URIS.iter().any(|u| *u == name.uri) {
        return Err(XsltError::InvalidStylesheet(format!(
            "{who} name='{}:{}' uses a reserved namespace (XTSE0080)",
            name.prefix.as_deref().unwrap_or(""), name.local,
        )));
    }
    Ok(())
}

/// XML Names §3 NCName production: `(Letter | '_') (NameChar)*`.
/// `NameChar` allows letters, digits, `.`, `-`, `_`, and the
/// non-ASCII NameChar codepoint ranges.  We accept the conservative
/// ASCII subset plus any non-ASCII alphabetic / digit codepoint —
/// the W3C suite's invalid-QName tests (XTSE0020 / XTSE0280) trip
/// on common ASCII errors (`/`, leading digit, embedded space).
fn is_valid_ncname(s: &str) -> bool {
    if s.is_empty() { return false; }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    let is_name_start = |c: char|
        c.is_alphabetic() || c == '_';
    let is_name_char = |c: char|
        c.is_alphanumeric() || c == '_' || c == '-' || c == '.';
    if !is_name_start(first) { return false; }
    chars.all(is_name_char)
}

fn parse_qname_list(
    context_node: &Node, raw: &str,
) -> Result<Vec<QName>, XsltError> {
    let mut out = Vec::new();
    for tok in raw.split_whitespace() {
        out.push(parse_qname_on(context_node, tok)?);
    }
    Ok(out)
}

fn parse_yesno(s: &str) -> bool {
    matches!(s.trim(), "yes" | "true" | "1")
}

/// Strict variant of [`parse_yesno`] — errors on anything that
/// isn't `yes` / `no` / `true` / `false` / `0` / `1`.  Used for
/// XSLT 2.0+ boolean-valued attributes where the spec calls out
/// the closed value set (e.g. `tunnel=`, `required=`).
fn parse_yesno_strict(s: &str, who: &str, attr: &str) -> Result<bool, XsltError> {
    match s.trim() {
        "yes" | "true" | "1"  => Ok(true),
        "no"  | "false" | "0" => Ok(false),
        other => Err(XsltError::InvalidStylesheet(format!(
            "{who} {attr}='{other}' must be 'yes' or 'no' (XTSE0020)"
        ))),
    }
}

/// Whitespace-only text nodes between XSLT structural elements
/// (e.g. between `<xsl:template/>` and the next sibling) are
/// stripped by XSLT 1.0 §3.4.  We treat them as non-significant
/// during top-level / structural traversal; literal whitespace
/// inside template bodies is preserved by `compile_instr_into`'s
/// text-node branch.
///
/// Exception: when an ancestor element carries `xml:space="preserve"`
/// the whitespace-only text node IS significant — preserve attribute
/// disables stripping for the subtree (XSLT 1.0 §3.4).  Walk up the
/// parent chain looking for an explicit `xml:space` value: `preserve`
/// keeps the text, `default` (or unset, the default) lets it drop.
fn is_significant_text(node: &Node) -> bool {
    if !matches!(node.kind, NodeKind::Text | NodeKind::CData) {
        return false;
    }
    if !is_xslt_whitespace_only(node.content()) {
        return true;
    }
    // Whitespace-only: keep iff the nearest enclosing element that
    // carries an `xml:space` attribute declares `preserve` (XSLT 1.0
    // §3.4).  The bearing element may be a literal result element or
    // an XSLT instruction — `xml:space="preserve"` on `xsl:choose`
    // (etc.) preserves the whitespace between its child instructions.
    let mut cur = node.parent.get();
    while let Some(p) = cur {
        for a in p.attributes() {
            if a.local_name() == "space"
               && a.namespace.get().and_then(|n| n.prefix()) == Some("xml") {
                return a.value() == "preserve";
            }
        }
        cur = p.parent.get();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::InMemoryLoader;

    fn parse(src: &str) -> Document {
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true,
            ..sup_xml_core::ParseOptions::default()
        };
        sup_xml_core::parse_str(src, &opts).expect("parse")
    }

    // ── root-element validation ─────────────────────────────────

    #[test]
    fn root_not_in_xslt_namespace_errors() {
        let doc = parse(r#"<bogus version="1.0"/>"#);
        let r = compile(&doc);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("not in the XSLT namespace"), "got {msg}");
        }
    }

    #[test]
    fn root_xslt_but_wrong_local_errors() {
        let doc = parse(r#"<xsl:not-a-stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0"/>"#);
        let r = compile(&doc);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("xsl:stylesheet"), "got {msg}");
        }
    }

    #[test]
    fn transform_root_is_accepted() {
        let doc = parse(r#"<xsl:transform
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0"/>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.version, "1.0");
    }

    // ── top-level element handling ──────────────────────────────

    #[test]
    fn top_level_non_xslt_element_silently_ignored() {
        // <rdf:Description> at top level — not an error.
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:rdf="http://example.com/rdf"
            version="1.0">
            <rdf:Description>extension data</rdf:Description>
            <xsl:template match="/"><x/></xsl:template>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.templates.len(), 1);
    }

    #[test]
    fn top_level_include_captures_href() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:include href="other.xsl"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.includes, vec!["other.xsl"]);
    }

    #[test]
    fn top_level_import_captures_href() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:import href="base.xsl"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.imports, vec!["base.xsl"]);
    }

    #[test]
    fn top_level_decimal_format_accepted() {
        // decimal-format is the "structural-only — accepted, ignored" branch.
        // Override BOTH decimal and grouping separators so the XTSE1300
        // distinct-character check doesn't fire on the defaults.
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:decimal-format name="custom"
                                decimal-separator=","
                                grouping-separator="."/>
        </xsl:stylesheet>"#);
        compile(&doc).unwrap();
    }

    #[test]
    fn top_level_unknown_xslt_element_rejected_at_supported_version() {
        // Stylesheet version="2.0" matches our processor's supported
        // version, so XSLT 2.0 §3.5 says unknown top-level XSLT
        // elements are XTSE0010 (no forwards-compatible processing
        // kicks in).
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="2.0">
            <xsl:something-from-the-future name="x"/>
        </xsl:stylesheet>"#);
        assert!(compile(&doc).is_err());
    }

    #[test]
    fn top_level_unknown_xslt_element_accepted_in_forwards_compat() {
        // Stylesheet declares a version higher than the processor
        // (`9.5`), so forwards-compatible processing accepts and
        // ignores unknown XSLT elements at top level.
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="9.5">
            <xsl:something-from-the-future name="x"/>
        </xsl:stylesheet>"#);
        compile(&doc).unwrap();
    }

    // ── xsl:template ────────────────────────────────────────────

    #[test]
    fn template_with_only_name_attribute_compiles() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template name="my-template"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.templates.len(), 1);
        assert!(ast.templates[0].name.is_some());
        assert!(ast.templates[0].match_pattern.is_none());
    }

    #[test]
    fn template_without_match_or_name_errors() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template/>
        </xsl:stylesheet>"#);
        let r = compile(&doc);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("match= or name="), "got {msg}");
        }
    }

    #[test]
    fn template_with_invalid_priority_errors() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template match="/" priority="not-a-number"/>
        </xsl:stylesheet>"#);
        let r = compile(&doc);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("priority"), "got {msg}");
        }
    }

    #[test]
    fn template_with_valid_priority_compiles() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template match="/" priority="2.5"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.templates[0].priority, Some(2.5));
    }

    // ── xsl:attribute-set ───────────────────────────────────────

    #[test]
    fn attribute_set_with_attribute_children_compiles() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:attribute-set name="my-set">
                <xsl:attribute name="id">x</xsl:attribute>
                <xsl:attribute name="class">y</xsl:attribute>
            </xsl:attribute-set>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.attribute_sets.len(), 1);
        assert_eq!(ast.attribute_sets[0].attributes.len(), 2);
    }

    #[test]
    fn attribute_set_with_non_attribute_child_errors() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:attribute-set name="my-set">
                <xsl:template name="bogus"/>
            </xsl:attribute-set>
        </xsl:stylesheet>"#);
        let r = compile(&doc);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("xsl:attribute"), "got {msg}");
        }
    }

    #[test]
    fn attribute_set_with_use_attribute_sets() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:attribute-set name="base">
                <xsl:attribute name="id">x</xsl:attribute>
            </xsl:attribute-set>
            <xsl:attribute-set name="derived" use-attribute-sets="base">
                <xsl:attribute name="class">y</xsl:attribute>
            </xsl:attribute-set>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.attribute_sets.len(), 2);
        assert_eq!(ast.attribute_sets[1].use_attribute_sets.len(), 1);
    }

    // ── xsl:output ──────────────────────────────────────────────

    #[test]
    fn output_with_cdata_section_elements() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:output method="xml" cdata-section-elements="raw script"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.outputs.len(), 1);
        assert_eq!(ast.outputs[0].cdata_section_elements.len(), 2);
    }

    #[test]
    fn output_full_options() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:output method="html"
                        encoding="UTF-8"
                        indent="yes"
                        omit-xml-declaration="yes"
                        standalone="no"
                        media-type="text/html"
                        doctype-public="-//W3C//DTD HTML 4.01//EN"
                        doctype-system="http://www.w3.org/TR/html4/strict.dtd"
                        version="4.0"/>
        </xsl:stylesheet>"#);
        let ast = compile(&doc).unwrap();
        let o = &ast.outputs[0];
        assert_eq!(o.method.as_deref(),                Some("html"));
        assert_eq!(o.encoding.as_deref(),              Some("UTF-8"));
        assert_eq!(o.indent,                           Some(true));
        assert_eq!(o.omit_xml_declaration,             Some(true));
        assert_eq!(o.standalone,                       Some(false));
        assert_eq!(o.media_type.as_deref(),            Some("text/html"));
        assert!(o.doctype_public.is_some());
        assert!(o.doctype_system.is_some());
        assert_eq!(o.version.as_deref(),               Some("4.0"));
    }

    // ── xsl:namespace-alias ─────────────────────────────────────

    #[test]
    fn namespace_alias_with_undeclared_prefix_errors() {
        let doc = parse(r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:namespace-alias stylesheet-prefix="nope" result-prefix="xsl"/>
        </xsl:stylesheet>"#);
        let r = compile(&doc);
        assert!(r.is_err());
    }

    #[test]
    fn namespace_alias_default_prefix_resolves_to_null_namespace() {
        // XSLT 1.0 §7.1.1 — `#default` with no default xmlns in
        // scope resolves to the null namespace ("").  The alias
        // entry is added with style_uri="" so literal result
        // elements in the null namespace are rewritten to the
        // result-side URI.
        let doc = parse(r##"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:namespace-alias stylesheet-prefix="#default" result-prefix="xsl"/>
        </xsl:stylesheet>"##);
        let ast = compile(&doc).unwrap();
        assert_eq!(ast.namespace_aliases.len(), 1);
        let (style_uri, result_uri, result_prefix) = &ast.namespace_aliases[0];
        assert_eq!(style_uri, "");
        assert_eq!(result_uri, "http://www.w3.org/1999/XSL/Transform");
        assert_eq!(result_prefix.as_deref(), Some("xsl"));
    }

    // ── compile_with_imports ────────────────────────────────────

    #[test]
    fn compile_with_imports_via_loader() {
        let main = r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:import href="imp.xsl"/>
            <xsl:template match="/"><x/></xsl:template>
        </xsl:stylesheet>"#;
        let imp = r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template match="foo"><y/></xsl:template>
        </xsl:stylesheet>"#;
        let loader = InMemoryLoader::new().with("imp.xsl", imp);
        let mut counter = 0;
        let ast = compile_with_imports(main, &loader, None,
            StylesheetAst::default(), &mut counter).unwrap();
        assert_eq!(ast.templates.len(), 2);
        // Imported template should have lower precedence than the local one.
        let local = ast.templates.iter().find(|t| t.import_precedence == 0).unwrap();
        let imported = ast.templates.iter().find(|t| t.import_precedence < 0).unwrap();
        assert!(local.match_pattern.is_some());
        assert!(imported.match_pattern.is_some());
    }

    #[test]
    fn compile_with_imports_via_include() {
        let main = r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:include href="inc.xsl"/>
            <xsl:template match="/"><x/></xsl:template>
        </xsl:stylesheet>"#;
        let inc = r#"<xsl:stylesheet
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform" version="1.0">
            <xsl:template match="foo"><y/></xsl:template>
        </xsl:stylesheet>"#;
        let loader = InMemoryLoader::new().with("inc.xsl", inc);
        let mut counter = 0;
        let ast = compile_with_imports(main, &loader, None,
            StylesheetAst::default(), &mut counter).unwrap();
        assert_eq!(ast.templates.len(), 2);
        // Includes share precedence with the including stylesheet — both 0.
        assert!(ast.templates.iter().all(|t| t.import_precedence == 0));
    }
}
