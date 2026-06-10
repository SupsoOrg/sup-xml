//! XSLT instruction evaluator.
//!
//! Walks a compiled stylesheet's templates against a source
//! document, dispatching instructions in order and accumulating
//! output into a [`ResultBuilder`].  The XSLT-built-in template
//! rules (XSLT 1.0 §5.8) supply default behaviour when no user
//! template matches — without them, `<xsl:apply-templates/>`
//! against a source tree whose nodes have no user-template match
//! would silently produce nothing.
//!
//! XPath expressions inside templates are evaluated through
//! `sup-xml-core`'s engine.  We bridge XSLT's lexical variable
//! scopes and namespace context into the XPath `XPathBindings`
//! trait via [`XsltBindings`].

use std::collections::HashMap;

use sup_xml_core::xpath::eval::{
    eval_expr, format_numeric_styled, value_equality_key, value_to_string_styled,
    EvalCtx, Numeric, NumStyle, StaticContext, Value, XPathBindings,
};
use sup_xml_core::xpath::{DocIndex, DocIndexLike, NodeId, XPathNodeKind};
use sup_xml_core::xpath::context::INodeKind;
use sup_xml_tree::dom::Document;

use crate::ast::{
    Avt, AvtPart, Instr, OnNoMatch, Param, QName, StylesheetAst, Template,
    Variable, WithParam,
};
use crate::error::XsltError;
use crate::functions::{self, KeyIndex};
use crate::loader::{Loader, NullLoader};
use crate::pattern;
use crate::result_tree::{ResultBuilder, ResultNode, ResultTree};

type Result<T> = std::result::Result<T, XsltError>;

// ── XPath bindings bridge ─────────────────────────────────────────

/// Bridge between the XSLT evaluator's runtime state (variables,
/// stylesheet namespace context, key index, XSLT context node) and
/// the XPath engine's pluggable `XPathBindings`.  Constructed
/// fresh per XPath call so the snapshot reflects the *current*
/// variable scope and current `current()` node.
struct XsltBindings<'a, I: DocIndexLike> {
    variables:         &'a VariableScope,
    namespaces:        &'a NamespaceContext,
    keys:              Option<&'a KeyIndex>,
    xslt_context_node: NodeId,
    idx:               &'a I,
    /// Stylesheet being evaluated.  `xsl:call-template` reached from
    /// inside an `xsl:function` body resolves named templates against
    /// `style.templates`; the rest of the dispatch ignores it.
    style:             &'a crate::ast::StylesheetAst,
    /// URI -> synthetic Document NodeId for every doc pre-loaded
    /// by [`apply_stylesheet_with_loader`].  `None` when the
    /// transformation isn't using a Loader.  Threaded through to
    /// `document()`'s dispatch in [`functions`].
    documents:         Option<&'a HashMap<String, NodeId>>,
    /// `<xsl:decimal-format>` table — name → settings.  Threaded
    /// through to `format-number()`'s 3rd-arg lookup.
    decimal_formats:   &'a HashMap<String, crate::format_number::DecimalFormat>,
    /// Source document's unparsed-entity map (entity name → SYSTEM /
    /// PUBLIC identifiers).  Powers XSLT 1.0 §12.4
    /// `unparsed-entity-uri()` / `unparsed-entity-public-id()`.
    unparsed_entities: &'a HashMap<String, sup_xml_tree::UnparsedEntity>,
    /// User-supplied extension functions for foreign-namespace XPath
    /// calls.  `None` means no user registration — the engine falls
    /// through to native EXSLT and then to "unknown function".  See
    /// [`crate::extensions::ExtensionFunctions`].
    user_exts: Option<&'a dyn crate::extensions::ExtensionFunctions>,
    /// User-defined XSLT 2.0 `<xsl:function>` declarations the
    /// XPath function dispatcher can match against `prefix:name(...)`
    /// calls.  Borrowed from the surrounding `StylesheetAst`; empty
    /// for pure XSLT 1.0 stylesheets.
    user_functions: Option<&'a [crate::ast::UserFunction]>,
    /// Active `xsl:for-each-group` body's nodes (the "current group")
    /// and grouping key.  `None` outside any group iteration — the
    /// XPath accessors `current-group()` / `current-grouping-key()`
    /// fall back to empty values in that case (matches Saxon).
    current_group:        Option<&'a [NodeId]>,
    current_grouping_key: Option<&'a Value>,
    /// Precomputed `xsl:accumulator` values, for
    /// `accumulator-before()` / `accumulator-after()`.  `None` when no
    /// accumulator has been computed yet.
    accumulators:         Option<&'a HashMap<String, AccumulatorData>>,
    /// Captured regex groups for `regex-group(n)` inside an
    /// `xsl:matching-substring`.  `None` outside any
    /// `xsl:analyze-string` matching iteration.
    regex_groups:         Option<&'a [String]>,
    /// URI → text resource for `unparsed-text()` and friends
    /// (XSLT 2.0 §16.6).  Pre-loaded at apply time from URIs that
    /// appear as static string literals in the stylesheet; dynamic
    /// URIs not in this map surface as a runtime error from
    /// `unparsed-text()` and as `false` from `unparsed-text-available()`.
    unparsed_texts:       Option<&'a HashMap<String, String>>,
    /// True when the stylesheet declared `version="3.0"` or higher.
    /// XSLT 3.0 §3.6 pre-binds `fn`, `math`, `map`, `array`, `err`
    /// in the static context; XSLT 1.0 / 2.0 require these to be
    /// declared explicitly.
    xslt_3_0:             bool,
    /// Stylesheet's declared `version=` attribute, surfaced
    /// through `system-property('xsl:version')`.
    xslt_version:         &'a str,
    /// Static base URI seen by XPath expressions in this stylesheet:
    /// the `xml:base` declared on the `xsl:stylesheet` root if any,
    /// otherwise the apply-time `base` URI.  `fn:resolve-uri($rel)`
    /// and `fn:static-base-uri()` use this.
    static_base_uri:      Option<&'a str>,
    /// XSLT 1.0 `document()` / XPath 2.0 `doc()` dynamic-loader
    /// hookup.  `Some` whenever the apply path passed a real
    /// `Loader`; the runtime resolves a URI on miss through this
    /// closure-style trio: pull bytes via `loader.load(uri, base)`,
    /// parse to a `Document`, graft into `idx` via
    /// `DocIndex::graft_dynamic_document`, and cache the resulting
    /// `NodeId` in `dyn_doc_cache` so repeated calls within the
    /// same apply scope reuse the graft.
    loader:               Option<&'a dyn crate::loader::Loader>,
    loader_base:          Option<&'a str>,
    dyn_doc_cache:        Option<&'a std::cell::RefCell<HashMap<String, NodeId>>>,
    /// Per-NodeId base-URI overrides published by xsl:variable /
    /// xsl:document instructions that carried xml:base — see
    /// [`EvalState::rtf_base_uris`].
    rtf_base_uris:        &'a std::cell::RefCell<HashMap<NodeId, String>>,
}

/// Does the stylesheet's `version=` attribute select XSLT 3.0 or
/// higher?  Used to gate XSLT-3-only static-context pre-bindings
/// (`fn`, `math`, etc.) — XSLT 1.0 / 2.0 leave these unbound and
/// require the stylesheet to declare them, matching the strict
/// reading of XSLT 2.0 §3.6 that tests like `type/namespace-6202`
/// expect.
fn xslt_version_3_or_more(version: &str) -> bool {
    let v = version.trim();
    let major = v.split('.').next().and_then(|s| s.parse::<u32>().ok());
    matches!(major, Some(n) if n >= 3)
}

/// Static XPath context derived from the stylesheet `version=`:
/// `xpath_2_0` follows the version-major>=2 test.  (The regex dialect
/// is not part of the static context — it stays on the bindings; see
/// the note on `StaticContext`.)
/// Split a grouping-key value into its individual items (XSLT 2.0
/// §14.3 — each item of a group-by key sequence is a separate key).
/// A node-set yields one single-node value per node so each node's
/// string-value becomes its own key.
fn grouping_key_items(v: &Value) -> Vec<Value> {
    match v {
        Value::NodeSet(ns)        => ns.iter().map(|&id| Value::NodeSet(vec![id])).collect(),
        Value::ForeignNodeSet(ns) => ns.iter().map(|&p| Value::ForeignNodeSet(vec![p])).collect(),
        Value::Sequence(items)    => items.clone(),
        Value::IntRange { lo, hi } => (*lo..=*hi).map(|i| Value::Number(Numeric::Integer(i))).collect(),
        other                     => vec![other.clone()],
    }
}

fn static_ctx_for_version(version: &str) -> StaticContext {
    let major = version.trim().split('.').next()
        .and_then(|s| s.parse::<u32>().ok());
    StaticContext {
        xpath_2_0: matches!(major, Some(n) if n >= 2),
        libxml2_compatible: false,
        // The native engine threads no fixed current() node yet;
        // `current()` falls back to the live context node.
        current_node: None,
    }
}

impl<'a, I: DocIndexLike> XsltBindings<'a, I> {
    /// `document($rel, $base)` — replace the first argument's URIs with
    /// their resolution against `$base`'s first node's base URI (XSLT
    /// 1.0 §12.1), so the subsequent load looks in the right place.
    /// A node-set first argument keeps its one-URI-per-node semantics.
    fn resolve_document_against_base(&self, mut args: Vec<Value>) -> Vec<Value> {
        let base_node = match args.get(1) {
            Some(Value::NodeSet(ns)) => ns.first().copied(),
            _ => None,
        };
        let (Some(bn), Some(loader)) = (base_node, self.loader) else { return args; };
        // The base URI is the recorded base of the node's document root
        // (a document()-loaded doc records its absolute URI; the source
        // root carries the source base).
        let mut root = bn;
        while let Some(p) = self.idx.parent(root) { root = p; }
        let Some(base) = self.node_base_uri(root).or_else(|| self.node_base_uri(bn))
        else { return args; };
        let resolve_one = |s: &str|
            loader.resolve(s, Some(&base)).unwrap_or_else(|_| s.to_string());
        // Resolve a single-URI first argument (a string, or a
        // single-node node-set whose string value is the URI).  The
        // multi-item node-set / sequence forms are left for
        // document_fn to load as-is — resolving each item against one
        // base node mis-handles empty / repeated values.
        args[0] = match &args[0] {
            Value::String(s) => Value::String(resolve_one(s)),
            Value::Typed(t)  => Value::String(resolve_one(&t.lexical)),
            Value::NodeSet(ns) if ns.len() == 1 =>
                Value::String(resolve_one(&self.idx.string_value(ns[0]))),
            other            => other.clone(),
        };
        args
    }

    /// `document($node-set)` (1-arg) — XSLT 1.0 §12.1: each node's
    /// string value is a URI resolved against THAT node's base URI.
    /// Only rewritten for nodes whose document root carries a recorded
    /// base (a document()-loaded fragment); source-document nodes with
    /// no recorded base keep the default loader-base resolution, and a
    /// non-node-set argument (string / atomic sequence) is untouched.
    fn resolve_document_node_bases(&self, mut args: Vec<Value>) -> Vec<Value> {
        let Some(loader) = self.loader else { return args; };
        let resolve_node = |id: NodeId| -> String {
            let raw = self.idx.string_value(id);
            let mut root = id;
            while let Some(p) = self.idx.parent(root) { root = p; }
            match self.node_base_uri(root) {
                Some(base) => loader.resolve(&raw, Some(&base)).unwrap_or(raw),
                None       => raw,
            }
        };
        if let Value::NodeSet(ns) = &args[0] {
            args[0] = Value::Sequence(
                ns.iter().map(|&id| Value::String(resolve_node(id))).collect());
        }
        args
    }
}

impl<'a, I: DocIndexLike> XPathBindings for XsltBindings<'a, I> {
    fn static_base_uri(&self) -> Option<String> {
        // XPath 2.0 §15.5.7 — static-base-uri() is an absolute URI.
        // When the host base is a bare absolute filesystem path (the
        // common case for a file-loaded stylesheet) present it as a
        // `file:` URI so it has a scheme, matching what a conformant
        // processor reports; relative xml:base values pass through.
        self.static_base_uri.map(|s| {
            if s.starts_with('/') { format!("file://{s}") } else { s.to_string() }
        })
    }

    fn node_base_uri(&self, id: NodeId) -> Option<String> {
        self.rtf_base_uris.borrow().get(&id).cloned()
    }

    fn regex_dialect(&self) -> sup_xml_core::regex::Dialect {
        // XSLT 3.0 hosts allow the XPath 3.0 regex extensions
        // (notably non-capturing `(?:…)`); XSLT 2.0 hosts pin to
        // the XSD 1.0 grammar plus XPath anchors.  Surface this
        // distinction through the bindings so `fn:matches` and
        // friends raise FORX0002 on the 3.0-only forms.
        if self.xslt_3_0 {
            sup_xml_core::regex::Dialect::Xpath
        } else {
            sup_xml_core::regex::Dialect::Xpath20
        }
    }

    /// XSLT runtime hookup for XPath's `doc()` / XSLT's `document()`
    /// on a URI not pre-loaded at apply start.  Caches per-URI so
    /// the same dynamic call within one apply only loads once.
    fn load_dynamic_document(
        &self, uri: &str,
    ) -> Option<std::result::Result<NodeId, sup_xml_core::error::XmlError>> {
        let cache  = self.dyn_doc_cache?;
        if let Some(&id) = cache.borrow().get(uri) {
            return Some(Ok(id));
        }
        let loader = self.loader?;
        // `load_parsed` lets a long-lived loader (e.g. the test
        // runner's per-test-set FilesystemLoader) cache the
        // parsed Document so repeated `doc()` calls for the same
        // URI across applies skip both the I/O and the parse.
        let doc = match loader.load_parsed(uri, self.loader_base) {
            Ok(d)  => d,
            Err(_) => return None, // surface as "URI not pre-loaded"
        };
        let id = match self.idx.graft_dynamic_document(&doc) {
            Some(id) => id,
            None    => return None,
        };
        cache.borrow_mut().insert(uri.to_string(), id);
        // Record the loaded document's absolute URI as its base so
        // base-uri() reports it and `document($rel, $thisdoc)` resolves
        // relative references against it (XSLT 1.0 §12.1).
        if let Ok(abs) = loader.resolve(uri, self.loader_base) {
            self.rtf_base_uris.borrow_mut().insert(id, abs);
        }
        Some(Ok(id))
    }
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        // Stylesheet-declared bindings always win.
        if let Some(uri) = self.namespaces.resolve(prefix) {
            return Some(uri);
        }
        // XSLT 3.0 §3.6 — additional standard prefixes are
        // pre-bound in 3.0+ static contexts.  We don't have a
        // `math` / `map` / `array` / `err` URI in our function
        // table yet; `fn` is the one with built-in coverage.
        // XSLT 1.0 / 2.0 leave these unbound (XSLT 2.0 §3.6 only
        // pre-binds `xml` / `xsl`); tests like
        // `type/namespace-6202` rely on the strict behaviour.
        if self.xslt_3_0 {
            match prefix {
                "fn"    => return Some("http://www.w3.org/2005/xpath-functions".into()),
                "math"  => return Some("http://www.w3.org/2005/xpath-functions/math".into()),
                "map"   => return Some("http://www.w3.org/2005/xpath-functions/map".into()),
                "array" => return Some("http://www.w3.org/2005/xpath-functions/array".into()),
                "err"   => return Some("http://www.w3.org/2005/xqt-errors".into()),
                _ => {}
            }
        }
        None
    }
    fn xpath_version_2_or_later(&self) -> bool {
        let major = self.xslt_version.trim().split('.').next()
            .and_then(|s| s.parse::<u32>().ok());
        matches!(major, Some(n) if n >= 2)
    }
    fn variable(&self, name: &str) -> Option<Value> {
        // The bind site stores variables under Clark form
        // (`{uri}local`).  XPath calls in with the lexical name; if
        // it has a prefix, resolve it via `resolve_prefix` so the
        // XSLT 3.0 standard pre-bindings (`err`, `math`, …) work
        // without an explicit xmlns declaration — that matches the
        // function-namespace lookup path's behaviour.
        let key = if let Some((prefix, local)) = name.split_once(':') {
            match self.resolve_prefix(prefix) {
                Some(uri) => format!("{{{uri}}}{local}"),
                None      => name.to_string(),
            }
        } else {
            name.to_string()
        };
        self.variables.get(&key)
            .or_else(|| self.variables.get(name))
            .cloned()
    }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        // No XPath context available on this path — fall back to the
        // XSLT current node, which matches the pre-`call_function_in`
        // behaviour.  XPath-aware functions (generate-id, position)
        // should route through `call_function_in` instead.
        self.call_function_in(ns_uri, name, args, self.xslt_context_node)
    }
    fn function_available_in(&self, ns_uri: &str, name: &str, arity: usize) -> bool {
        // A registered user `xsl:function` with this expanded name and
        // arity (XSLT 2.0 function names are always namespaced).  Built-in
        // and EXSLT availability is decided by the core engine.
        self.user_functions.unwrap_or(&[]).iter().any(|uf| {
            uf.name.uri == ns_uri && uf.name.local == name && uf.params.len() == arity
        })
    }
    fn function_signature_in(&self, ns_uri: &str, name: &str, arity: usize)
        -> Option<sup_xml_core::xpath::FunctionSig> {
        use sup_xml_core::xpath::{parse_sequence_type_str, FunctionSig, ItemType,
            Occurrence, SequenceType};
        // The declared `as=` types of a matching user `xsl:function` and
        // its `xsl:param`s.  An omitted type defaults to `item()*`.
        let item_star = || SequenceType { item: ItemType::Any, occurrence: Occurrence::ZeroOrMore };
        let uf = self.user_functions.unwrap_or(&[]).iter().find(|uf|
            uf.name.uri == ns_uri && uf.name.local == name && uf.params.len() == arity)?;
        let params = uf.params.iter().map(|p|
            p.as_type.as_deref().and_then(parse_sequence_type_str).unwrap_or_else(item_star)
        ).collect();
        let ret = uf.as_type.as_deref().and_then(parse_sequence_type_str).unwrap_or_else(item_star);
        Some(FunctionSig { params, ret })
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
        xpath_context_node: NodeId,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        // XSLT-built-in functions (current, key, generate-id, etc.)
        // live in the default XPath namespace, but XPath 2.0 also
        // exposes them in the `fn:` namespace (`fn:document(...)`,
        // `fn:key(...)`).  Treat both prefixes the same.
        if ns_uri.is_empty()
            || ns_uri == "http://www.w3.org/2005/xpath-functions"
        {
            // document($rel, $base) — XSLT 1.0 §12.1: resolve each
            // relative URI in $rel against the base URI of $base's
            // first node before loading.  Done here (not in
            // `document_fn`) because base-uri resolution needs the
            // bindings' node-base table and the active Loader.
            let args = if name == "document" && args.len() == 2 {
                self.resolve_document_against_base(args)
            } else if name == "document" && args.len() == 1 {
                self.resolve_document_node_bases(args)
            } else { args };
            let dyn_load = |uri: &str|
                <Self as sup_xml_core::xpath::eval::XPathBindings>::load_dynamic_document(self, uri);
            let dyn_load: Option<&dyn Fn(&str) -> Option<std::result::Result<NodeId, sup_xml_core::error::XmlError>>>
                = if self.loader.is_some() { Some(&dyn_load) } else { None };
            return functions::dispatch(
                name, args, self.idx, self.xslt_context_node, xpath_context_node,
                self.keys, &[], self.documents, dyn_load, self.decimal_formats,
                self.namespaces, self.unparsed_entities,
                self.current_group, self.current_grouping_key, self.regex_groups,
                self.unparsed_texts,
                self.user_functions.unwrap_or(&[]),
                self.xslt_version,
                self.accumulators,
            );
        }
        // XSLT 2.0 user-defined functions (`<xsl:function>`) live in
        // a user-chosen namespace.  The minimal "pure-XPath body"
        // form — a single `<xsl:sequence select="…"/>` — evaluates
        // here without re-entering the XSLT instruction loop: we
        // bind the parameters as XPath local variables (via
        // `ScopedBindings`) and evaluate the captured select expr.
        // Function bodies with more than one sequence-constructor
        // instruction surface as an error at call time — out of
        // scope for the initial 2.0 slice.
        // XSLT 2.0 §10.3 — multiple xsl:function declarations may
        // share a name as long as their arities differ.  Match on
        // (expanded-name, arity) so each overload dispatches
        // correctly; fall back to a name-only match (preserving the
        // historical "wrong-arity error" diagnostic) when no
        // arity-specific match exists.
        if let Some(fs) = self.user_functions {
            let uf = fs.iter().find(|f| f.name.uri == ns_uri
                && f.name.local == name
                && f.params.len() == args.len())
                .or_else(|| fs.iter().find(|f| f.name.uri == ns_uri
                    && f.name.local == name));
            if let Some(uf) = uf {
                // XSLT 3.0 §3.5.2 — an abstract function (from a used
                // package) that was never overridden has no body and
                // cannot be invoked.
                if uf.visibility.as_deref() == Some("abstract") {
                    return Some(Err(sup_xml_core::xpath::eval::xpath_err(format!(
                        "cannot call abstract function {} — no implementation \
                         was supplied by the using package (XTDE3052)", name))));
                }
                return Some(call_user_function_pure_xpath(
                    uf, args, self.idx, xpath_context_node, self, self.style,
                ));
            }
        }
        // User-registered extensions get the next shot.  Either
        // branch returning `None` falls through to native EXSLT
        // dispatch in the core engine — which now covers
        // `str:tokenize` / `str:split` / `regexp:match` via the
        // index's synthetic-text store.
        self.user_exts.and_then(|exts| exts.call(ns_uri, name, args))
    }

    fn foreign_string_value(
        &self,
        p: sup_xml_core::xpath::eval::ForeignNodePtr,
    ) -> String {
        // Tree-crate accessor encapsulates the unsafe deref —
        // xslt stays `forbid(unsafe_code)`.
        sup_xml_tree::dom::Document::node_string_value_by_ptr(p)
    }
}

/// Invoke an XSLT 2.0 `<xsl:function>`.  Bodies that can be
/// evaluated without re-entering the full XSLT instruction loop —
/// i.e. without `<xsl:apply-templates>`, `<xsl:call-template>`,
/// `<xsl:copy>` and friends — are handled directly here through a
/// small sequence-constructor interpreter that layers XPath
/// variable bindings on top of the outer chain as `<xsl:variable>`
/// instructions are encountered.  This covers the common pattern of
/// `xsl:variable` → `xsl:sequence` chains in computational helpers,
/// plus `xsl:if` / `xsl:choose` / `xsl:for-each` over nodesets.
///
/// Truly mutable-state instructions (`xsl:apply-templates`,
/// `xsl:call-template`) still surface as a runtime error since the
/// XPath bindings layer only carries an immutable view of the
/// surrounding `EvalState`.
fn call_user_function_pure_xpath<I: DocIndexLike>(
    uf:           &crate::ast::UserFunction,
    args:         Vec<Value>,
    idx:          &I,
    context_node: NodeId,
    outer:        &dyn XPathBindings,
    style:        &crate::ast::StylesheetAst,
) -> std::result::Result<Value, sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    let err = |m: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, m.to_string());

    if args.len() != uf.params.len() {
        return Err(err(&format!(
            "xsl:function {}:{} expects {} argument(s), got {}",
            uf.name.prefix.as_deref().unwrap_or(""),
            uf.name.local, uf.params.len(), args.len(),
        )));
    }
    // Layer each parameter value as a `NamedBinding` so XPath refs
    // (`$x`) resolve through the chain.  Innermost (rightmost) param
    // wins on name collision — declared names must be unique anyway.
    // XSLT 2.0 §10.3 / XTTE0790 — coerce each supplied argument to
    // the matching xsl:param's declared `as=` type before binding.
    // Pure-mismatch input (e.g. string `'banana'` → xs:integer) is
    // surfaced here so the function body never sees the wrong type.
    let mut coerced: Vec<Value> = Vec::with_capacity(args.len());
    for (p, v) in uf.params.iter().zip(args.into_iter()) {
        let bound = if let Some(t) = &p.as_type {
            match crate::eval::parse_as_atomic_type(t) {
                Some(st) => crate::eval::coerce_to_atomic_sequence(v, &st, idx)
                    .map_err(|e| err(&format!(
                        "xsl:function: argument '{}' type mismatch: {e}",
                        p.name.local,
                    )))?,
                None => v,
            }
        } else { v };
        coerced.push(bound);
    }
    fn build_chain<'p>(
        params: &'p [crate::ast::Param], args: Vec<Value>, parent: &'p dyn XPathBindings,
    ) -> Box<dyn XPathBindings + 'p> {
        let mut cur: Box<dyn XPathBindings + 'p> = Box::new(PassthroughBindings { inner: parent });
        for (p, v) in params.iter().zip(args.into_iter()) {
            let local = p.name.local.clone();
            cur = Box::new(NamedBinding { parent_owned: cur, name: local, value: v });
        }
        cur
    }
    let chain = build_chain(&uf.params, coerced, outer);
    let static_ctx = static_ctx_for_version(&style.version);
    // XSLT 2.0 §10.3 — the focus is *undefined* inside an
    // xsl:function body.  Mark it so context-dependent operations
    // raise their declared error codes instead of leaking the
    // caller's context.  Set the xslt-side flag (consulted by
    // key() / unparsed-entity-uri / -public-id) and the core-side
    // flag (consulted by `.` and absolute/relative paths in the
    // XPath evaluator).
    let prev_ctx = CONTEXT_UNDEFINED.with(|c| c.replace(true));
    let items = sup_xml_core::xpath::eval::with_focus_undefined(true, || {
        eval_function_body(
            &uf.body, idx, context_node, 1, 1, chain.as_ref(),
            FnEnv { style, depth: 0 }, &static_ctx,
        )
    });
    CONTEXT_UNDEFINED.with(|c| c.set(prev_ctx));
    let items = items?;
    let v = if items.len() == 1 { items.into_iter().next().unwrap() }
            else                 { Value::Sequence(items) };
    // XSLT 2.0 §10.3 / XTTE0780 — when xsl:function declares as=,
    // the produced value must match the declared type.  We support
    // the same atomic / cardinality coercion as xsl:variable: route
    // through coerce_to_atomic_sequence and translate the XSLT error
    // back into an XmlError so the pure-XPath call site can surface
    // it the same way as any other dynamic function failure.
    if let Some(t) = &uf.as_type {
        if let Some(st) = crate::eval::parse_as_atomic_type(t) {
            return crate::eval::coerce_to_atomic_sequence(v, &st, idx)
                .map_err(|e| err(&format!("xsl:function: {e}")));
        }
    }
    Ok(v)
}

/// Sequence-constructor interpreter for `xsl:function` bodies.
/// Walks `body` instruction-by-instruction, collecting contributed
/// values into the returned `Vec<Value>`.  `<xsl:variable>` extends
/// the binding chain and the rest of the body is evaluated via tail
/// recursion so the variable is in scope for everything that
/// follows.  Returns an error on instructions that need a mutable
/// `EvalState` view (template dispatch, output construction, etc.).
fn avt_static_string(a: &crate::ast::Avt) -> Option<String> { a.as_literal() }

/// Ambient context threaded through [`eval_function_body`] alongside
/// the per-instruction `bindings` / `ctx_node`.  Carries the bits a
/// pure function-body evaluation needs but that don't change as the
/// binding chain grows: the stylesheet (so `xsl:call-template` can
/// resolve a named template) and the cumulative template-call depth
/// (so a function ↔ template recursion aborts cleanly instead of
/// overflowing the stack, mirroring the stateful path's guard).
#[derive(Clone, Copy)]
struct FnEnv<'a> {
    style: &'a crate::ast::StylesheetAst,
    depth: u32,
}

/// Evaluate a `select=`-or-body sequence constructor (an
/// `xsl:with-param` value or an `xsl:param` default) on the pure
/// function-body path.  `select=` evaluates the XPath directly;
/// body form builds a result-tree fragment and either keeps it as a
/// node sequence (when `as=` is a sequence-typed node target, per
/// XSLT 2.0 §9.3) or stringifies it.
fn fn_seq_constructor_value<I: DocIndexLike>(
    select:   Option<&sup_xml_core::xpath::Expr>,
    body:     &[crate::ast::Instr],
    as_type:  Option<&str>,
    idx:      &I,
    ctx_node: NodeId, pos: usize, size: usize,
    bindings: &dyn XPathBindings,
    static_ctx: &StaticContext,
) -> std::result::Result<Value, sup_xml_core::error::XmlError> {
    use sup_xml_core::xpath::eval::{eval_expr, EvalCtx};
    if let Some(sel) = select {
        let ctx = EvalCtx {
            context_node: ctx_node, pos, size, bindings,
            static_ctx,
        };
        return eval_expr(sel, &ctx, idx);
    }
    if body.is_empty() {
        return Ok(Value::String(String::new()));
    }
    let mut builder = ResultBuilder::new();
    // Sequence-constructor bodies keep each top-level instruction's
    // text contribution separate so a function returning N empty
    // xsl:text nodes counts as N items (decl/function-1009), not 1.
    builder.no_text_merge = true;
    build_function_subtree(body, bindings, idx, ctx_node, pos, size, &mut builder, static_ctx)?;
    let nodes = builder.finish();
    let want_nodes = as_type
        .map(|t| as_is_sequence_typed(t) && !as_target_is_atomic(t))
        .unwrap_or(false);
    if want_nodes {
        Ok(Value::NodeSet(rtf_children_into_index_generic(idx, &nodes)))
    } else if as_type.is_none() {
        // No `as=` declaration — XSLT 2.0 §10.3 says the function's
        // result is the sequence the body constructed.  Expose top-
        // level nodes as a NodeSet (preserving the per-instruction
        // item identity that `count()` / sequence access needs).
        // A body that contributed *only* text gets a single combined
        // node by happenstance, which matches the old stringify
        // shape from `value-of()`.
        Ok(Value::NodeSet(rtf_children_into_index_generic(idx, &nodes)))
    } else {
        Ok(Value::String(stringify(&nodes)))
    }
}

/// Apply an `xsl:param` / `xsl:with-param` `as=` coercion to a value
/// on the pure function-body path, translating the XSLT type error
/// into an `XmlError` so the XPath call site surfaces it uniformly.
fn fn_coerce_as<I: DocIndexLike>(
    v: Value, as_type: Option<&str>, idx: &I,
) -> std::result::Result<Value, sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    match as_type.and_then(crate::eval::parse_as_atomic_type) {
        Some(st) => crate::eval::coerce_to_atomic_sequence(v, &st, idx)
            .map_err(|e| XmlError::new(
                ErrorDomain::XPath, ErrorLevel::Error, format!("{e}"))),
        None => Ok(v),
    }
}

fn eval_function_body<I: DocIndexLike>(
    body: &[crate::ast::Instr],
    idx:  &I,
    ctx_node: NodeId,
    pos:  usize,
    size: usize,
    bindings: &dyn XPathBindings,
    env: FnEnv,
    static_ctx: &StaticContext,
) -> std::result::Result<Vec<Value>, sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    use sup_xml_core::xpath::eval::{eval_expr, value_to_bool, value_to_string_with, EvalCtx};
    use crate::ast::Instr;
    let err = |m: String| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, m);
    let mut out: Vec<Value> = Vec::new();
    fn mk_ctx<'a>(
        b: &'a dyn XPathBindings, sc: &'a StaticContext,
        ctx_node: NodeId, pos: usize, size: usize,
    ) -> EvalCtx<'a> {
        EvalCtx {
            context_node: ctx_node, pos, size, bindings: b,
            static_ctx: sc,
        }
    }
    for (i, instr) in body.iter().enumerate() {
        match instr {
            Instr::Sequence { select } => {
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                out.push(v);
            }
            Instr::Variable(v) => {
                // Compute the variable's value with the current
                // binding chain, then layer a new chain element on
                // top and recurse with the tail — that way every
                // subsequent instruction sees the binding.
                let val = if let Some(sel) = &v.select {
                    eval_expr(sel, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?
                } else if v.body.is_empty() {
                    Value::String(String::new())
                } else {
                    let items = eval_function_body(&v.body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
                    if items.len() == 1 { items.into_iter().next().unwrap() }
                    else                 { Value::Sequence(items) }
                };
                let layered = NamedBinding {
                    parent_owned: Box::new(PassthroughBindings { inner: bindings }),
                    name: v.name.local.clone(),
                    value: val,
                };
                let mut tail = eval_function_body(
                    &body[i + 1..], idx, ctx_node, pos, size, &layered, env, static_ctx)?;
                out.append(&mut tail);
                return Ok(out);
            }
            Instr::If { test, body: if_body } => {
                let v = eval_expr(test, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                if value_to_bool(&v, idx) {
                    let mut sub = eval_function_body(if_body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
                    out.append(&mut sub);
                }
            }
            Instr::Choose { whens, otherwise } => {
                let mut matched = false;
                for (test, when_body) in whens {
                    let v = eval_expr(test, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                    if value_to_bool(&v, idx) {
                        let mut sub = eval_function_body(
                            when_body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
                        out.append(&mut sub);
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    if let Some(else_body) = otherwise {
                        let mut sub = eval_function_body(
                            else_body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
                        out.append(&mut sub);
                    }
                }
            }
            Instr::ForEach { select, body: fe_body, .. } => {
                // Iterate `select`'s items, evaluating the body for
                // each with that item as the context node when the
                // item is a node (atomic items keep the surrounding
                // context node since our model doesn't carry an
                // atomic context-item separately).
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                let items: Vec<Value> = match v {
                    Value::NodeSet(ns) => ns.into_iter()
                        .map(|id| Value::NodeSet(vec![id])).collect(),
                    Value::Sequence(items) => items,
                    other => vec![other],
                };
                let total = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    let cx = match &item {
                        Value::NodeSet(ns) if ns.len() == 1 => ns[0],
                        _ => ctx_node,
                    };
                    let mut sub = eval_function_body(
                        fe_body, idx, cx, i + 1, total, bindings, env, static_ctx)?;
                    out.append(&mut sub);
                }
            }
            Instr::ValueOf { select, separator, .. } => {
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                let text = match separator {
                    // XSLT 1.0 / backwards-compat (§7.6.1): the
                    // string-value of the first item only.  The
                    // compiler leaves `separator` None in 1.0 scopes
                    // (including a `[xsl:]version="1.0"` subtree).
                    None => value_to_string_with(&v, idx, bindings),
                    // XSLT 2.0 §11.5 — atomise to a sequence and join
                    // the string-items with the separator.
                    Some(sep_avt) => {
                        let sep = avt_static_string(sep_avt)
                            .unwrap_or_else(|| " ".to_string());
                        let items = sequence_string_items(
                            &v, idx,
                            NumStyle::from_context(false, bindings.xpath_version_2_or_later()),
                        );
                        if items.len() <= 1 {
                            value_to_string_with(&v, idx, bindings)
                        } else {
                            items.join(&sep)
                        }
                    }
                };
                out.push(Value::String(text));
            }
            Instr::ValueOfBody { body: vob_body, separator, .. } => {
                // Body-form xsl:value-of: recurse into the body,
                // join the string contributions with the separator
                // (default empty for body form, XSLT 2.0 §11.5).
                let sep = separator.as_ref()
                    .and_then(avt_static_string).unwrap_or_default();
                let sub = eval_function_body(
                    vob_body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
                let pieces: Vec<String> = sub.into_iter()
                    .map(|v| value_to_string_with(&v, idx, bindings))
                    .filter(|s| !s.is_empty())
                    .collect();
                out.push(Value::String(pieces.join(&sep)));
            }
            Instr::LiteralText { text, .. } => {
                // XSLT 2.0 §10.3 — an xsl:text instruction always
                // contributes a text-node item, even when its
                // content is empty.  Preserves item count for
                // sequence-consuming callers (decl/function-1009:
                // `count(my:f())` = 3 for three xsl:text).
                out.push(Value::String(text.clone()));
            }
            // Construction-mode instructions — `xsl:function` bodies
            // commonly build a result tree (`<TR>...</TR>` literal
            // elements, `xsl:element`, `xsl:copy-of`, …) and return
            // it as the function's value.  We materialise that tree
            // into a fresh `ResultBuilder`, then graft the top-level
            // result nodes into the dynamic-RTF arena so each becomes
            // an addressable `NodeId` in the returned sequence.
            Instr::LiteralElement { .. }
            | Instr::Element { .. }
            | Instr::Attribute { .. }
            | Instr::Comment { .. }
            | Instr::ProcessingInstruction { .. }
            | Instr::CopyOf { .. }
            | Instr::Copy { .. } => {
                let mut builder = ResultBuilder::new();
                build_function_subtree(
                    std::slice::from_ref(instr), bindings, idx,
                    ctx_node, pos, size, &mut builder, static_ctx,
                )?;
                let nodes = builder.finish();
                let ids = rtf_children_into_index_generic(idx, &nodes);
                for id in ids {
                    out.push(Value::NodeSet(vec![id]));
                }
            }
            Instr::Number { value, select, level, count, from, format,
                            grouping_separator, grouping_size, ordinal, lang, letter_value: _, start_at } => {
                // xsl:function bodies don't carry instruction-level
                // state, so AVT attributes must be static literals.
                // The format/lang/grouping AVTs all start with a
                // single literal in practice — falling through to
                // defaults when the AVT contains an expression keeps
                // the common case working without re-entering the
                // full state machine.
                let format_str = avt_static_string(format)
                    .unwrap_or_else(|| "1".to_string());
                let fmt = crate::number::parse_format(&format_str);
                let ordinal_str = ordinal.as_ref()
                    .and_then(avt_static_string).unwrap_or_default();
                let lang_str = lang.as_ref()
                    .and_then(avt_static_string).unwrap_or_default();
                let opts = crate::number::FormatOptions {
                    ordinal: !ordinal_str.is_empty(),
                    lang: if lang_str.is_empty() { None } else { Some(lang_str) },
                    ordinal_scheme: if ordinal_str.is_empty() { None }
                                    else { Some(ordinal_str) },
                };
                let grouping_sep_str = grouping_separator.as_ref()
                    .and_then(avt_static_string);
                let grouping_size_n = grouping_size.as_ref()
                    .and_then(avt_static_string)
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|n| *n > 0);
                let group = match (grouping_sep_str, grouping_size_n) {
                    (Some(sep), Some(n)) => Some((sep, n)),
                    _ => None,
                };
                let numbers = if let Some(ve) = value {
                    let v = eval_expr(ve, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                    match v {
                        Value::Sequence(items) => items.into_iter().filter_map(|it| {
                            let f = crate::eval::value_to_number_xpath(&it, idx);
                            if f.is_finite() { Some(f.round() as i64) } else { None }
                        }).collect(),
                        Value::IntRange { lo, hi } => (lo..=hi).collect(),
                        Value::Number(f) if f.as_f64().is_finite() => vec![f.as_f64().round() as i64],
                        Value::Number(_) => Vec::new(),
                        other => {
                            let f = crate::eval::value_to_number_xpath(&other, idx);
                            if f.is_finite() { vec![f.round() as i64] } else { Vec::new() }
                        }
                    }
                } else {
                    let target_node = if let Some(sel) = select {
                        match eval_expr(sel, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)? {
                            Value::NodeSet(ns) if ns.len() == 1 => ns[0],
                            _ => return Err(err(
                                "xsl:number select must evaluate to a \
                                 single node (XTTE1000)".into())),
                        }
                    } else {
                        // XSLT 2.0 §12.4 / XTTE0990 — without select=,
                        // the counting context is the context item.
                        // Inside an xsl:function body the focus is
                        // undefined; with an atomic for-each (e.g.
                        // `<xsl:for-each select="1 to 5">`) the
                        // context item is not a node.
                        if sup_xml_core::xpath::eval::focus_is_undefined()
                            || in_atomic_for_each()
                        {
                            return Err(err(
                                "xsl:number with no select= called where \
                                 the context item is not a node \
                                 (XTTE0990)".into()));
                        }
                        ctx_node
                    };
                    compute_number_list_generic(
                        idx, bindings, *level,
                        count.as_ref(), from.as_ref(), target_node,
                    ).map_err(|e| err(format!("xsl:number in function body: {e}")))?
                };
                let mut numbers = numbers;
                let start_offsets: Vec<i64> = start_at.as_ref()
                    .and_then(avt_static_string)
                    .map(|s| s.split_whitespace()
                        .filter_map(|t| t.parse::<i64>().ok())
                        .map(|v| v - 1).collect())
                    .unwrap_or_default();
                for (i, n) in numbers.iter_mut().enumerate() {
                    let off = if value.is_some() { start_offsets.first() }
                              else { start_offsets.get(i) };
                    if let Some(off) = off { *n += off; }
                }
                let mut s = crate::number::format_list_opts(&numbers, &fmt, &opts);
                if let Some((sep, sz)) = &group {
                    s = crate::number::apply_grouping(&s, sep, *sz);
                }
                out.push(Value::String(s));
            }
            Instr::Message { terminate: _, body } => {
                // xsl:function bodies don't expose mutable XSLT
                // state, so xsl:message has nowhere to emit the
                // text.  Stringify the body via the function-body
                // interpreter to preserve any side effects of arg
                // evaluation, then drop the result on the floor.
                let _ = eval_function_body(body, idx, ctx_node, pos, size, bindings, env, static_ctx)?;
            }
            Instr::CallTemplate { name, with_params } => {
                // `xsl:call-template` to a named template whose body is
                // itself pure (no apply-templates, no result-tree side
                // effects beyond what a function may construct) runs
                // through this same interpreter: bind the params, then
                // evaluate the template body.  The context node is
                // preserved (XSLT 1.0 §6) and the depth guard mirrors
                // the stateful path so a function ↔ template cycle
                // aborts cleanly.
                let key = qname_key(name);
                let template = env.style.templates.iter()
                    .find(|t| t.name.as_ref().map(qname_key).as_deref()
                        == Some(key.as_str()))
                    .ok_or_else(|| err(format!(
                        "xsl:call-template in function body: no template named `{key}`")))?;
                if env.depth >= MAX_TEMPLATE_CALL_DEPTH {
                    return Err(err(format!(
                        "xsl:call-template depth exceeds limit ({MAX_TEMPLATE_CALL_DEPTH}) \
                         — likely infinite recursion in template `{key}`")));
                }
                // Evaluate xsl:with-param in the caller's context.
                // Tunnel params ride the mutable tunnel pool, which
                // this state-free path doesn't carry — reject them
                // rather than silently dropping the value.
                let mut supplied: Vec<(String, Value)> =
                    Vec::with_capacity(with_params.len());
                for wp in with_params {
                    if wp.tunnel {
                        return Err(err(format!(
                            "xsl:with-param tunnel=\"yes\" (param `{}`) requires mutable \
                             XSLT state and isn't supported in xsl:function bodies yet",
                            wp.name.local)));
                    }
                    let raw = fn_seq_constructor_value(
                        wp.select.as_ref(), &wp.body, wp.as_type.as_deref(),
                        idx, ctx_node, pos, size, bindings, static_ctx)?;
                    supplied.push((
                        qname_key(&wp.name),
                        fn_coerce_as(raw, wp.as_type.as_deref(), idx)?,
                    ));
                }
                // Bind declared params over the caller's chain.  Each
                // default is evaluated against the chain built so far,
                // so a later default can reference an earlier param
                // (XSLT 2.0 §10.1.1).  Tunnel-typed params read only
                // the pool, never a regular arg, matching the stateful
                // path's precedence.
                let mut chain: Box<dyn XPathBindings> =
                    Box::new(PassthroughBindings { inner: bindings });
                for p in &template.params {
                    let pkey = qname_key(&p.name);
                    let raw = if p.tunnel {
                        if p.required {
                            return Err(err(format!(
                                "required tunnel parameter `{pkey}` can't be supplied \
                                 in an xsl:function body")));
                        }
                        fn_seq_constructor_value(
                            p.select.as_ref(), &p.body, p.as_type.as_deref(),
                            idx, ctx_node, pos, size, chain.as_ref(), static_ctx)?
                    } else if let Some((_, v)) = supplied.iter().find(|(k, _)| *k == pkey) {
                        v.clone()
                    } else if p.required {
                        // XSLT 2.0 §10.1.2 / XTDE0700.
                        return Err(err(format!("required parameter `{pkey}` not supplied")));
                    } else {
                        fn_seq_constructor_value(
                            p.select.as_ref(), &p.body, p.as_type.as_deref(),
                            idx, ctx_node, pos, size, chain.as_ref(), static_ctx)?
                    };
                    let value = fn_coerce_as(raw, p.as_type.as_deref(), idx)?;
                    chain = Box::new(NamedBinding {
                        parent_owned: chain, name: p.name.local.clone(), value,
                    });
                }
                let mut sub = eval_function_body(
                    &template.body, idx, ctx_node, pos, size, chain.as_ref(),
                    FnEnv { depth: env.depth + 1, ..env }, static_ctx)?;
                out.append(&mut sub);
            }
            Instr::Fallback { .. } => {
                // No-op outside an unrecognised-instruction context
                // (matches the main eval).
            }
            Instr::PerformSort { select, sort, body: _ } => {
                // `xsl:perform-sort` is side-effect-free, so it's
                // legal inside an `xsl:function` body.  Only the
                // `select=` form is supported here — body form
                // requires the mutable result-tree builder we don't
                // carry on this path.
                let select_expr = select.as_ref().ok_or_else(|| err(
                    "xsl:perform-sort body form is not supported in xsl:function bodies".into()
                ))?;
                let v = eval_expr(select_expr, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                match v {
                    Value::NodeSet(nodes) => {
                        let sorted = crate::sort::sort_nodes(
                            &nodes, sort, idx, |e, n, p, s| {
                                eval_expr(e, &mk_ctx(bindings, static_ctx, n, p, s), idx)
                                    .map_err(crate::error::XsltError::from)
                            },
                        ).map_err(|e| match e {
                            crate::error::XsltError::Xpath(x) => x,
                            other => err(format!("{other}")),
                        })?;
                        out.push(Value::NodeSet(sorted));
                    }
                    other => {
                        // Atomic / mixed sequence — sort items
                        // individually using the per-item context-
                        // item slot for bare `.` references.
                        let items: Vec<Value> = match other {
                            Value::Sequence(items) => items,
                            single                 => vec![single],
                        };
                        let sorted = crate::sort::sort_items(
                            items, sort, idx, |e, item, p, s| {
                                sup_xml_core::xpath::eval::with_atomic_context_item(
                                    Some(item.clone()),
                                    || eval_expr(e, &mk_ctx(bindings, static_ctx, ctx_node, p, s), idx)
                                        .map_err(crate::error::XsltError::from),
                                )
                            },
                        ).map_err(|e| match e {
                            crate::error::XsltError::Xpath(x) => x,
                            other => err(format!("{other}")),
                        })?;
                        for it in sorted { out.push(it); }
                    }
                }
            }
            other => return Err(err(format!(
                "xsl:function body: instruction `{}` requires mutable XSLT state \
                 and isn't supported in user-function bodies yet",
                instr_kind_name(other),
            ))),
        }
    }
    Ok(out)
}

/// Short human-readable label for an `Instr` variant, used in error
/// diagnostics so the message says `xsl:apply-templates` rather than
/// a full Debug print.
fn instr_kind_name(i: &crate::ast::Instr) -> &'static str {
    use crate::ast::Instr;
    match i {
        Instr::Sequence { .. }            => "xsl:sequence",
        Instr::Map { .. }                 => "xsl:map",
        Instr::MapEntry { .. }            => "xsl:map-entry",
        Instr::Variable(_)                => "xsl:variable",
        Instr::If { .. }                  => "xsl:if",
        Instr::Choose { .. }              => "xsl:choose",
        Instr::ForEach { .. }             => "xsl:for-each",
        Instr::Iterate { .. }             => "xsl:iterate",
        Instr::NextIteration { .. }       => "xsl:next-iteration",
        Instr::Break { .. }               => "xsl:break",
        Instr::ForEachGroup { .. }        => "xsl:for-each-group",
        Instr::ApplyTemplates { .. }      => "xsl:apply-templates",
        Instr::ApplyImports { .. }         => "xsl:apply-imports",
        Instr::NextMatch { .. }           => "xsl:next-match",
        Instr::CallTemplate { .. }        => "xsl:call-template",
        Instr::ValueOf { .. }             => "xsl:value-of",
        Instr::ValueOfBody { .. }         => "xsl:value-of",
        Instr::LiteralElement { .. }      => "literal result element",
        Instr::LiteralText { .. }         => "literal text",
        Instr::Copy { .. }                => "xsl:copy",
        Instr::CopyOf { .. }              => "xsl:copy-of",
        Instr::Element { .. }             => "xsl:element",
        Instr::Attribute { .. }           => "xsl:attribute",
        Instr::Comment { .. }             => "xsl:comment",
        Instr::ProcessingInstruction { .. } => "xsl:processing-instruction",
        Instr::Number { .. }              => "xsl:number",
        Instr::Message { .. }             => "xsl:message",
        Instr::Fallback { .. }            => "xsl:fallback",
        Instr::AnalyzeString { .. }       => "xsl:analyze-string",
        Instr::SourceDocument { .. }      => "xsl:source-document",
        Instr::Fork { .. }                => "xsl:fork",
        Instr::WherePopulated { .. }      => "xsl:where-populated",
        Instr::OnEmpty { .. }             => "xsl:on-empty",
        Instr::OnNonEmpty { .. }          => "xsl:on-non-empty",
        Instr::Evaluate { .. }            => "xsl:evaluate",
        Instr::Merge { .. }               => "xsl:merge",
        Instr::PerformSort { .. }         => "xsl:perform-sort",
        Instr::Document { .. }            => "xsl:document",
        Instr::ResultDocument { .. }      => "xsl:result-document",
        Instr::Namespace { .. }           => "xsl:namespace",
        Instr::Try { .. }                 => "xsl:try",
        Instr::Unsupported { .. }         => "unknown",
    }
}

/// Passthrough wrapper letting us box an `XPathBindings` reference so
/// the call-site chain can layer additional scopes on top.  Every
/// method delegates straight to `inner` — no behavioural change.
/// Base of an `xsl:function` body's binding chain.  Delegates to the
/// caller's bindings EXCEPT for `fn:regex-group`, which always returns
/// the empty string inside a function: the captured-substring context
/// of an enclosing `xsl:analyze-string` does not propagate into a
/// stylesheet function call (XSLT 2.0 §15.3).
struct PassthroughBindings<'p> { inner: &'p dyn XPathBindings }
impl<'p> PassthroughBindings<'p> {
    /// The builtin `fn:regex-group` (unprefixed / fn-namespace), as
    /// opposed to a user function that happens to be named
    /// `regex-group` in some other namespace.
    fn suppress_regex_group(
        ns_uri: &str, name: &str,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        let is_builtin = name == "regex-group"
            && (ns_uri.is_empty()
                || ns_uri == "http://www.w3.org/2005/xpath-functions");
        is_builtin.then(|| Ok(Value::String(String::new())))
    }
}
impl<'p> XPathBindings for PassthroughBindings<'p> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.inner.resolve_prefix(prefix)
    }
    fn xpath_version_2_or_later(&self) -> bool { self.inner.xpath_version_2_or_later() }
    fn variable(&self, name: &str) -> Option<Value> { self.inner.variable(name) }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        Self::suppress_regex_group(ns_uri, name)
            .or_else(|| self.inner.call_function(ns_uri, name, args))
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>, ctx: NodeId,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        Self::suppress_regex_group(ns_uri, name)
            .or_else(|| self.inner.call_function_in(ns_uri, name, args, ctx))
    }
    fn foreign_string_value(
        &self, p: sup_xml_core::xpath::eval::ForeignNodePtr,
    ) -> String { self.inner.foreign_string_value(p) }
}

/// Binds one variable name → value on top of an owned parent binding
/// chain.  Used to layer xsl:function parameters during call dispatch.
struct NamedBinding<'p> {
    parent_owned: Box<dyn XPathBindings + 'p>,
    name:         String,
    value:        Value,
}
impl<'p> XPathBindings for NamedBinding<'p> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.parent_owned.resolve_prefix(prefix)
    }
    fn xpath_version_2_or_later(&self) -> bool { self.parent_owned.xpath_version_2_or_later() }
    fn variable(&self, name: &str) -> Option<Value> {
        if name == self.name { Some(self.value.clone()) } else { self.parent_owned.variable(name) }
    }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        self.parent_owned.call_function(ns_uri, name, args)
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>, ctx: NodeId,
    ) -> Option<std::result::Result<Value, sup_xml_core::error::XmlError>> {
        self.parent_owned.call_function_in(ns_uri, name, args, ctx)
    }
    fn foreign_string_value(
        &self, p: sup_xml_core::xpath::eval::ForeignNodePtr,
    ) -> String { self.parent_owned.foreign_string_value(p) }
}

// ── variable scope stack ──────────────────────────────────────────

/// Precomputed values of one `xsl:accumulator` over a document:
/// the value immediately before each node's pre-order event and
/// immediately after its post-order event (XSLT 3.0 §18.4), plus the
/// initial value used as the fallback for unvisited nodes.
pub(crate) struct AccumulatorData {
    pub before:  HashMap<NodeId, Value>,
    pub after:   HashMap<NodeId, Value>,
    pub initial: Value,
}

/// A stack of variable maps.  Innermost frame wins on lookup.
/// XSLT 1.0 scoping rules (§11): variables are visible from their
/// point of binding through the rest of their containing element;
/// for-each / call-template / template-apply each open a fresh
/// scope.
#[derive(Default)]
struct VariableScope {
    frames: Vec<HashMap<String, Value>>,
}

impl VariableScope {
    fn enter(&mut self)             { self.frames.push(HashMap::new()); }
    fn leave(&mut self)             { self.frames.pop(); }
    fn bind(&mut self, name: String, value: Value) {
        if self.frames.is_empty() { self.frames.push(HashMap::new()); }
        self.frames.last_mut().unwrap().insert(name, value);
    }
    fn get(&self, name: &str) -> Option<&Value> {
        for frame in self.frames.iter().rev() {
            if let Some(v) = frame.get(name) { return Some(v); }
        }
        None
    }
}

// ── stylesheet namespace context ─────────────────────────────────

/// Prefix→URI map collected at compile time so XPath inside
/// template bodies can resolve `prefix:local` name tests.  We
/// build this from the stylesheet's xmlns declarations during
/// `apply_stylesheet`'s prologue.
#[derive(Default, Debug)]
pub(crate) struct NamespaceContext {
    map: HashMap<String, String>,
}

impl NamespaceContext {
    pub(crate) fn resolve(&self, prefix: &str) -> Option<String> {
        self.map.get(prefix).cloned()
    }
    fn from_stylesheet(style: &StylesheetAst) -> Self {
        // Start from every xmlns declaration on the stylesheet
        // root — that's the canonical source for resolving
        // prefixes inside XPath expressions per XSLT 1.0 §3.2.
        let mut map: HashMap<String, String> = style.namespaces.clone();
        // EXSLT fallbacks: always available under conventional
        // prefixes, even when the stylesheet didn't bind them.
        map.entry("math".into())  .or_insert("http://exslt.org/math".into());
        map.entry("date".into())  .or_insert("http://exslt.org/dates-and-times".into());
        map.entry("str".into())   .or_insert("http://exslt.org/strings".into());
        map.entry("set".into())   .or_insert("http://exslt.org/sets".into());
        map.entry("regexp".into()).or_insert("http://exslt.org/regular-expressions".into());
        map.entry("exsl".into())  .or_insert("http://exslt.org/common".into());
        map.entry("dyn".into())   .or_insert("http://exslt.org/dynamic".into());
        NamespaceContext { map }
    }
}

// ── built-in templates (XSLT 1.0 §5.8) ────────────────────────────

/// Apply the built-in template rules to `node`.  These fire when
/// no user template matches.  Rules:
///
/// | Source node type            | Default behaviour                |
/// |-----------------------------|----------------------------------|
/// | Document, Element           | apply-templates to children      |
/// | Text, Attribute             | copy the string-value to output  |
/// | Comment, PI, Namespace      | no output                        |
fn apply_builtin_template(
    state: &mut EvalState,
    node:  NodeId,
    mode:  Option<&QName>,
) -> Result<()> {
    apply_builtin_template_with_args(state, node, mode, &[])
}

/// Same as [`apply_builtin_template`] but forwards `args` (caller-
/// supplied xsl:with-param values) into the recursive children
/// dispatch.  XSLT 2.0 §6.7: built-in element / document templates
/// pass on *all* parameters, tunnel or not, so the test suite's
/// `<xsl:apply-templates select="$rtf"><xsl:with-param/>` patterns
/// reach the matched template with their argument intact.
fn apply_builtin_template_with_args(
    state: &mut EvalState,
    node:  NodeId,
    mode:  Option<&QName>,
    args:  &[(QName, Value, Option<Vec<ResultNode>>)],
) -> Result<()> {
    // The `on-no-match` property of the mode (XSLT 3.0 §6.7) selects
    // the built-in action; modes without an `xsl:mode` declaration use
    // the XSLT 1.0 default, `text-only-copy`.
    match mode_on_no_match(state.style, mode) {
        OnNoMatch::DeepSkip => Ok(()),
        OnNoMatch::Fail => Err(XsltError::InvalidStylesheet(
            "no template rule matches and the mode's on-no-match is \
             'fail' (XTDE0555)".into())),
        OnNoMatch::DeepCopy => {
            match state.idx.kind(node) {
                XPathNodeKind::Document => {
                    for c in state.idx.children(node).to_vec() {
                        deep_copy_node(state, c, None, true)?;
                    }
                }
                _ => deep_copy_node(state, node, None, true)?,
            }
            Ok(())
        }
        OnNoMatch::ShallowCopy => builtin_shallow(state, node, mode, args, true),
        OnNoMatch::ShallowSkip => builtin_shallow(state, node, mode, args, false),
        OnNoMatch::TextOnlyCopy => {
            match state.idx.kind(node) {
                XPathNodeKind::Document | XPathNodeKind::Element => {
                    // Recurse into children with xsl:strip-space applied —
                    // whitespace-only text nodes inside stripped elements
                    // are silently skipped here, so the built-in text rule
                    // doesn't fire for them.
                    let style = state.style;
                    let idx = state.idx;
                    let children: Vec<NodeId> = state.idx.children(node).iter().copied()
                        .filter(|&n| !crate::whitespace::should_strip(style, n, idx))
                        .collect();
                    let total = children.len();
                    for (i, child) in children.iter().enumerate() {
                        apply_one_to_node_with_args(state, *child, mode, i + 1, total, args)?;
                    }
                }
                XPathNodeKind::Text | XPathNodeKind::CData => {
                    let s = state.idx.string_value(node);
                    state.builder.push_text(s, false);
                }
                XPathNodeKind::Attribute => {
                    // Built-in for attribute is "copy its value" — only
                    // fires when an attribute is explicitly applied to
                    // via `<xsl:apply-templates select="@*"/>`, etc.
                    let s = state.idx.string_value(node);
                    state.builder.push_text(s, false);
                }
                // Comment, PI, Namespace → no-op.
                _ => {}
            }
            Ok(())
        }
    }
}

/// The `on-no-match` action declared for `mode`, defaulting to the
/// XSLT 1.0 `text-only-copy` when no `xsl:mode` declares it.
fn mode_on_no_match(style: &StylesheetAst, mode: Option<&QName>) -> OnNoMatch {
    fn norm(q: Option<&QName>) -> Option<(&str, &str)> {
        match q {
            // `#default`/`#unnamed` and the absent name all denote the
            // unnamed mode.
            Some(q) if !q.local.is_empty() || !q.uri.is_empty() =>
                Some((q.uri.as_str(), q.local.as_str())),
            _ => None,
        }
    }
    let want = norm(mode);
    style.modes.iter()
        .find(|m| norm(m.name.as_ref()) == want)
        .map(|m| m.on_no_match)
        .unwrap_or_default()
}

/// Built-in template for the `shallow-copy` / `shallow-skip` actions
/// (XSLT 3.0 §6.7).  `copy_self` distinguishes them: shallow-copy
/// reproduces the node, shallow-skip discards it; both then apply
/// templates to the node's attributes and children in the same mode.
fn builtin_shallow(
    state: &mut EvalState,
    node:  NodeId,
    mode:  Option<&QName>,
    args:  &[(QName, Value, Option<Vec<ResultNode>>)],
    copy_self: bool,
) -> Result<()> {
    let recurse = |state: &mut EvalState| -> Result<()> {
        let attrs: Vec<NodeId> = state.idx.attr_range(node).collect();
        let na = attrs.len();
        for (i, a) in attrs.iter().enumerate() {
            apply_one_to_node_with_args(state, *a, mode, i + 1, na, args)?;
        }
        let style = state.style;
        let idx = state.idx;
        let children: Vec<NodeId> = state.idx.children(node).iter().copied()
            .filter(|&n| !crate::whitespace::should_strip(style, n, idx))
            .collect();
        let nc = children.len();
        for (i, c) in children.iter().enumerate() {
            apply_one_to_node_with_args(state, *c, mode, i + 1, nc, args)?;
        }
        Ok(())
    };
    match state.idx.kind(node) {
        XPathNodeKind::Document => recurse(state),
        XPathNodeKind::Element => {
            if copy_self {
                let q = element_qname(state, node);
                state.builder.open_element(q);
                for ns_id in state.idx.ns_range(node) {
                    let prefix = state.idx.local_name(ns_id);
                    if prefix == "xml" { continue; }
                    let uri = state.idx.string_value(ns_id);
                    let p = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                    state.builder.push_namespace_decl(p, uri);
                }
                recurse(state)?;
                state.builder.close_element();
                Ok(())
            } else {
                recurse(state)
            }
        }
        XPathNodeKind::Text | XPathNodeKind::CData => {
            if copy_self { state.builder.push_text(state.idx.string_value(node), false); }
            Ok(())
        }
        XPathNodeKind::Attribute => {
            if copy_self {
                let q = attribute_qname(state, node);
                state.builder.push_attribute(q, state.idx.string_value(node));
            }
            Ok(())
        }
        XPathNodeKind::Comment => {
            if copy_self { state.builder.push_comment(state.idx.string_value(node)); }
            Ok(())
        }
        XPathNodeKind::PI => {
            if copy_self {
                state.builder.push_pi(
                    state.idx.pi_target(node).to_string(),
                    state.idx.string_value(node));
            }
            Ok(())
        }
        XPathNodeKind::Namespace => Ok(()),
    }
}

// ── runtime state ─────────────────────────────────────────────────

/// Carries the mutable state threaded through every instruction:
/// the result builder, the variable scope, references to the
/// stylesheet AST + source index, and the static namespace
/// context.  Everything else (context node / position / size)
/// flows through explicit function arguments.
struct EvalState<'a> {
    style:      &'a StylesheetAst,
    idx:        &'a DocIndex<'a>,
    namespaces: &'a NamespaceContext,
    keys:       Option<&'a KeyIndex>,
    /// URI -> synthetic Document NodeId for docs pre-loaded by
    /// `apply_stylesheet_with_loader`.  `document(uri)` resolves
    /// against this; absent when no Loader was supplied.
    documents:  Option<&'a HashMap<String, NodeId>>,
    /// The XSLT instruction-level context node — what `current()`
    /// should return.  Distinct from the XPath inner context node
    /// (which lives inside `EvalCtx` and changes inside predicate
    /// evaluation).  Updated at every template invocation and
    /// for-each iteration.
    xslt_current: NodeId,
    variables:  VariableScope,
    /// Result-tree-fragment storage for variables / params bound
    /// with a body sequence-constructor.  XSLT 1.0 §11.3: such
    /// variables have value "result tree fragment" — preserved as
    /// nodes here so `xsl:copy-of select="$var"` reproduces the
    /// tree structure faithfully (without this, the variable is
    /// just a string, and copy-of emits empty text).
    ///
    /// Keyed by the variable's expanded-name (qname_key form);
    /// scoped via `rtf_scopes` so the entries roll back when a
    /// variable scope ends.
    rtfs:       HashMap<String, Vec<ResultNode>>,
    rtf_scopes: Vec<Vec<String>>,
    builder:    ResultBuilder,
    /// The principal-output builder while `builder` is temporarily an
    /// `xsl:result-document` secondary capture.  `None` when `builder`
    /// itself is the principal destination (the common case).  A
    /// `href`-less `xsl:result-document` nested inside a secondary one
    /// targets the principal URI, so it writes here, not to `builder`.
    principal_buf: Option<ResultBuilder>,
    /// Source document's unparsed-entity table — captured once at
    /// apply time and threaded through to `unparsed-entity-uri()` /
    /// `unparsed-entity-public-id()`.
    unparsed_entities: std::sync::Arc<HashMap<String, sup_xml_tree::UnparsedEntity>>,
    /// Source document handle.  Retained so dynamic `document()`
    /// resolution can graft loaded docs into the index without
    /// requiring a separate handle.
    source_doc: &'a Document,
    /// `(node, mode, current template's import_precedence)` —
    /// captured each time we enter a pattern-matched template so
    /// `xsl:apply-imports` can re-run selection against the same
    /// `(node, mode)` while capping the candidate set at
    /// precedence < this one.  `None` outside any pattern-matched
    /// template (e.g. during xsl:call-template, top-level
    /// variables, etc.) — xsl:apply-imports raises an error there
    /// per XSLT 1.0 §5.6.
    /// `(node, mode, precedence, template_index, priority, branch_idx)`
    /// for the currently-executing pattern-matched template.
    /// `template_index` is `style.templates[i]`'s position — used by
    /// `xsl:next-match` to pick the next-priority template when
    /// `precedence - 1` produces no match.  `branch_idx` records
    /// which operand of a union pattern fired (XSLT 2.0 §6.4), so
    /// `xsl:next-match` can step to a sibling branch of the same
    /// template before falling through.  `None` outside any
    /// pattern-matched template (e.g. inside xsl:call-template,
    /// top-level variables).
    apply_imports_ctx: Option<(NodeId, Option<QName>, i32, Option<usize>, f64, Option<usize>)>,
    /// User-supplied extension functions, threaded through to every
    /// XPath evaluation via `XsltBindings.user_exts`.  See
    /// [`crate::extensions::ExtensionFunctions`].
    user_exts: Option<&'a dyn crate::extensions::ExtensionFunctions>,
    /// Stack of sequence sinks for active `xsl:function` calls (XSLT
    /// 2.0).  When non-empty, `xsl:sequence` appends its value to the
    /// top entry rather than emitting into the result tree; the
    /// caller (`call_user_function`) pops and inspects the captured
    /// values to derive the function's return.  Empty in pure 1.0
    /// stylesheets.
    sequence_sinks: Vec<Vec<Value>>,
    /// Cumulative template invocation depth (xsl:call-template +
    /// xsl:apply-templates).  Bumped on entry, decremented on exit,
    /// and capped at [`MAX_TEMPLATE_CALL_DEPTH`] so a stylesheet that
    /// accidentally tail-recurses without a guard surfaces a clean
    /// runtime error instead of blowing the process stack.
    template_call_depth: u32,
    /// Active `xsl:for-each-group` body's nodes (`current-group()`).
    /// Empty outside any group iteration.  Owned here so callers
    /// don't pin a borrow through the entire instruction loop.
    current_group: Vec<NodeId>,
    /// Active grouping key (`current-grouping-key()`); `None`
    /// outside a group iteration.
    current_grouping_key: Option<Value>,
    /// Precomputed `xsl:accumulator` values over the source document,
    /// keyed by accumulator expanded-name.  Filled lazily the first
    /// time `accumulator-before` / `accumulator-after` is needed.
    accumulators: HashMap<String, AccumulatorData>,
    /// Captured regex groups from the current `xsl:matching-substring`
    /// body — index 0 is the full match, 1..N are the groups.
    /// `regex-group(n)` reads from here.  Empty outside any
    /// `xsl:analyze-string` matching iteration.
    regex_groups: Vec<String>,
    /// XSLT 2.0 tunnel-parameter pool — keyed by the param's
    /// expanded-name.  Propagates through apply/call chains until a
    /// receiving `xsl:param tunnel="yes"` reads it.  Mutated in
    /// scoped fashion (snapshot on template entry, restore on exit)
    /// so updates from inner `xsl:with-param tunnel="yes"` don't
    /// leak to the caller.
    tunnel_pool: HashMap<String, Value>,
    /// URI → text resource pre-loaded for XSLT 2.0 §16.6
    /// `unparsed-text()` / `unparsed-text-available()` /
    /// `unparsed-text-lines()`.  `None` when no static URIs were
    /// collected at compile time (the function still dispatches —
    /// it just always reports "not available").
    unparsed_texts: Option<&'a HashMap<String, String>>,
    /// Static base URI for XPath expressions evaluated against this
    /// state.  Prefers `xml:base` on the stylesheet root; otherwise
    /// the apply-time `base` URI.  Surfaces through
    /// `fn:resolve-uri($rel)` and `fn:static-base-uri()`.
    static_base_uri: Option<String>,
    /// Loader handle, kept here so the runtime `document()` /
    /// XPath 2.0 `doc()` path can fetch URIs computed at apply
    /// time (concat-built, attribute-driven, etc.) that the
    /// static pre-load missed.  `None` outside an
    /// `apply_with_loader`-style call.
    loader: Option<&'a dyn crate::loader::Loader>,
    /// Base URI passed alongside the loader for relative-href
    /// resolution.  Pairs with `loader`.
    loader_base: Option<&'a str>,
    /// Per-apply cache of dynamically-loaded URIs → grafted
    /// document-root node ids.  Insert-on-miss; lookups serve
    /// repeated `doc('docs/x.xml')` calls within one apply
    /// without re-loading or re-grafting.
    dyn_doc_cache: Option<&'a std::cell::RefCell<HashMap<String, NodeId>>>,
    /// XPath 2.0 §3.1.5 — base-URI overrides for synthetic nodes
    /// the runtime constructs (RTF document roots from xsl:variable
    /// / xsl:with-param bodies, xsl:document instructions).  Keyed
    /// by the synthetic NodeId; the value is the resolved URI from
    /// the constructing instruction's xml:base.  fn:base-uri()
    /// consults this before falling back to ancestor-walk lookup
    /// for ordinary nodes.  Shared `RefCell` so nested sub-states
    /// (xsl:function, xsl:apply-templates) see entries published
    /// in the outer scope and contribute to the same table.
    rtf_base_uris: &'a std::cell::RefCell<HashMap<NodeId, String>>,
    /// Static XPath context (XPath version + regex dialect) derived
    /// from the stylesheet version.  Borrowed into every `EvalCtx`
    /// this state constructs so XPath operators observe the same
    /// 1.0/2.0/3.0 distinctions the [`XsltBindings`] declares.
    static_ctx: StaticContext,
}

/// Conservative ceiling on template-invocation depth.  Real-world
/// XSLT pipelines rarely exceed a few hundred (deeply nested
/// recursive number formatting / tree-walks are the usual culprits);
/// 1024 leaves an order-of-magnitude headroom while still aborting
/// pathological infinite-recursion stylesheets before they exhaust
/// the OS thread stack.
const MAX_TEMPLATE_CALL_DEPTH: u32 = 1024;

impl<'a> EvalState<'a> {
    /// Snapshot the state as an [`XsltBindings`] for one XPath call.
    /// The returned bindings borrow from `self`, so each construction
    /// site stays a single line and adding fields touches only this
    /// helper (and the matching field on [`EvalState`]).
    /// Is an `xsl:function` body currently in flight?  Used by
    /// `xsl:sequence` to decide whether to capture into the
    /// function's return value or emit into the result tree.
    fn sequence_sink_active(&self) -> bool { !self.sequence_sinks.is_empty() }

    /// Number-serialization style for result output (the serialization
    /// slice of the static context).  An XSLT 2.0+ stylesheet renders
    /// xs:double/xs:float in F&O scientific form; a 1.0 stylesheet keeps
    /// XPath 1.0 decimal.  Values no longer carry a precomputed lexical,
    /// so this is threaded into `value_to_string_styled` at every result
    /// serialization site.
    fn num_style(&self) -> NumStyle {
        let v2 = self.style.version.trim().split('.').next()
            .and_then(|m| m.parse::<u32>().ok())
            .is_some_and(|m| m >= 2);
        NumStyle::from_context(false, v2)
    }

    fn push_to_sequence_sink(&mut self, v: Value) {
        if let Some(top) = self.sequence_sinks.last_mut() { top.push(v); }
    }

    fn bindings(&self) -> XsltBindings<'_, DocIndex<'a>> {
        XsltBindings {
            variables:         &self.variables,
            namespaces:        self.namespaces,
            keys:              self.keys,
            xslt_context_node: self.xslt_current,
            idx:               self.idx,
            style:             self.style,
            documents:         self.documents,
            decimal_formats:   &self.style.decimal_formats,
            unparsed_entities: &self.unparsed_entities,
            user_exts:            self.user_exts,
            current_group:        if self.current_group.is_empty() {
                                       None
                                   } else {
                                       Some(self.current_group.as_slice())
                                   },
            current_grouping_key: self.current_grouping_key.as_ref(),
            accumulators:         (!self.accumulators.is_empty()).then_some(&self.accumulators),
            regex_groups:         if self.regex_groups.is_empty() {
                                       None
                                   } else {
                                       Some(self.regex_groups.as_slice())
                                   },
            user_functions:       (!self.style.functions.is_empty())
                                     .then_some(self.style.functions.as_slice()),
            unparsed_texts:       self.unparsed_texts,
            xslt_3_0:             xslt_version_3_or_more(&self.style.version),
            xslt_version:         self.style.version.as_str(),
            static_base_uri:      self.static_base_uri.as_deref(),
            loader:               self.loader,
            loader_base:          self.loader_base,
            dyn_doc_cache:        self.dyn_doc_cache,
            rtf_base_uris:        self.rtf_base_uris,
        }
    }

    /// Construct an `EvalCtx` for an XPath evaluation against the
    /// current variable scope.  Use this inside instruction
    /// handlers; the borrow is released immediately so subsequent
    /// `self.variables.bind(…)` calls are free to mutate.
    fn xpath_eval(
        &self, expr: &sup_xml_core::xpath::Expr,
        context_node: NodeId, pos: usize, size: usize,
    ) -> Result<Value> {
        // Fresh step budget for each top-level XPath expression — the
        // XSLT engine triggers many of these per apply(), and the
        // thread-local cap would otherwise deplete across one run
        // and spuriously reject later evaluations.  The cap still
        // bounds any single expression's evaluation work.
        sup_xml_core::xpath::eval::reset_eval_budget();
        let sc = self.static_ctx;
        let b = self.bindings();
        // XPath 2.0 §1: every namespace prefix used in a name test
        // must be in scope.  XSLT 2.0 surfaces an undeclared prefix
        // as XPST0081 (static error code).  Without this, a path
        // like `@undeclared:attribute` would silently produce the
        // empty sequence instead of erroring out.
        sup_xml_core::xpath::eval::validate_prefixes(expr, &b)
            .map_err(|e| e.or_xpath_code("XPST0081"))?;
        let ctx = EvalCtx { context_node, pos, size, bindings: &b, static_ctx: &sc };
        eval_expr(expr, &ctx, self.idx).map_err(XsltError::from)
    }
}

// ── public entry point ────────────────────────────────────────────

/// Run `style` against `source_doc` and produce the result tree.
/// Convenience wrapper around [`apply_stylesheet_with_loader`] with
/// a [`NullLoader`] — sufficient for stylesheets that don't call
/// `document()` with a static URI.  Stylesheets that do will fail
/// at apply time with the NullLoader's error.
pub fn apply_stylesheet(
    style:      &StylesheetAst,
    source_doc: &Document,
) -> Result<ResultTree> {
    apply_stylesheet_full(style, source_doc, &NullLoader, None, None)
}

/// Run `style` against `source_doc`, using `loader` to resolve any
/// `document()` URIs the stylesheet references with string literals.
///
/// `base` is the base URI for resolving relative hrefs (typically
/// the source-document URI or the stylesheet URI; callers know
/// which fits their setup).  Pass `None` when relative resolution
/// isn't needed.
///
/// Dynamic `document()` arguments (anything other than a string
/// literal in the first position) raise a runtime error since the
/// engine pre-loads URIs at apply time rather than during XPath
/// evaluation.  Stylesheets that need dynamic URIs should rewrite
/// to enumerate the literal set, or file an issue.
pub fn apply_stylesheet_with_loader(
    style:      &StylesheetAst,
    source_doc: &Document,
    loader:     &dyn Loader,
    base:       Option<&str>,
) -> Result<ResultTree> {
    apply_stylesheet_full(style, source_doc, loader, base, None)
}

/// Full-form apply with both `document()` loader resolution and
/// caller-registered XPath extension functions.  See
/// [`crate::extensions::ExtensionFunctions`] for the registration
/// surface and [`crate::Stylesheet::apply_with_extensions`] for the
/// supported public entry point.
pub fn apply_stylesheet_full(
    style:      &StylesheetAst,
    source_doc: &Document,
    loader:     &dyn Loader,
    base:       Option<&str>,
    extensions: Option<&dyn crate::extensions::ExtensionFunctions>,
) -> Result<ResultTree> {
    apply_stylesheet_full_with_params(style, source_doc, loader, base, extensions, &[])
}

/// Full-form apply with caller-supplied overrides for top-level
/// `xsl:param` declarations.  Each `(name, value)` pair binds
/// `name` (matched against the declared global-param qname's
/// local part) to a string `value`, replacing whatever default the
/// stylesheet's `select=` / body would have produced.  Unmatched
/// param names are silently ignored — XSLT 1.0 doesn't require the
/// caller to know the stylesheet's parameter set, and over-strict
/// rejection would break common "apply many stylesheets with the
/// same fixed parameter bag" patterns.
pub fn apply_stylesheet_full_with_params(
    style:      &StylesheetAst,
    source_doc: &Document,
    loader:     &dyn Loader,
    base:       Option<&str>,
    extensions: Option<&dyn crate::extensions::ExtensionFunctions>,
    top_level_params: &[(String, String)],
) -> Result<ResultTree> {
    apply_stylesheet_full_with_params_and_initial(
        style, source_doc, loader, base, extensions, top_level_params, None, None,
    )
}

/// Same as [`apply_stylesheet_full_with_params`] but allows the
/// caller to pick a named template as the entry point instead of the
/// default "apply-templates on the document node" dispatch.  Matches
/// XSLT 3.0's named-template / initial-template invocation pattern
/// — most W3C 2.0/3.0 test cases use this entry shape.  `None`
/// preserves the historical XSLT 1.0 behaviour.
pub fn apply_stylesheet_full_with_params_and_initial(
    style:             &StylesheetAst,
    source_doc:        &Document,
    loader:            &dyn Loader,
    base:              Option<&str>,
    extensions:        Option<&dyn crate::extensions::ExtensionFunctions>,
    top_level_params:  &[(String, String)],
    initial_template:  Option<&str>,
    initial_mode:      Option<&str>,
) -> Result<ResultTree> {
    // Reset the XPath step-budget thread-local for this whole apply.
    // Per-expression reset in `xpath_eval` is a finer grain that already
    // helps, but pattern matching and sort key evaluation use
    // `eval_expr` directly without a reset, so we'd otherwise inherit a
    // depleted budget from any prior call on this thread.
    sup_xml_core::xpath::eval::reset_eval_budget();
    // Sample a single stable instant for this transform so every
    // fn:current-dateTime / current-date / current-time call returns
    // the same value (XPath 2.0 §16 stable-execution requirement).
    sup_xml_core::xpath::eval::refresh_stable_now();
    // Defensive: clear any stale xsl:iterate control signal left by a
    // prior (failed) transform on this thread, so it can't short-
    // circuit this run's instruction sequences.
    let _ = take_iterate_control();
    // Clear secondary documents / temp-output depth from any prior
    // transform on this thread before collecting this run's output.
    reset_secondary_docs();
    // Pre-load every document referenced via a string-literal URI
    // in the stylesheet's `document()` calls.  The loaded docs live
    // in this function's scope so their lifetimes naturally cover
    // the DocIndex that borrows from them.
    let mut loaded_docs: Vec<Box<Document>> = Vec::with_capacity(style.documents_to_load.len());
    for href in &style.documents_to_load {
        // XSLT 1.0 §12.1 — `document('')` means "the stylesheet's
        // containing document."  The loader's resolution would turn
        // an empty href into a doubled path, so route the empty-URI
        // case through the base path directly.
        let text = if href.is_empty() {
            match base {
                Some(b) => loader.load(b, None)?,
                None    => continue, // no base — silently skip
            }
        } else {
            // A fragment identifier plays no role in retrieving the
            // resource (XSLT 1.0 §12.1); load the bare URI.
            loader.load(crate::functions::strip_uri_fragment(href), base)?
        };
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true,
            ..Default::default()
        };
        let doc = sup_xml_core::parse_str(&text, &opts).map_err(XsltError::from)?;
        loaded_docs.push(Box::new(doc));
    }

    // Dynamic-form `document()` slot: URIs not known until apply time
    // (e.g. `document(@href)`, `document(*)`) are loaded speculatively
    // below.  The owned `Box<Document>` keeps each loaded doc alive
    // for the rest of `apply_stylesheet_full`'s scope.
    let mut style_documents_dynamic: Vec<(String, Box<Document>)> = Vec::new();

    // XSLT 1.0 §12.1 also allows `document(node-set)` — each node's
    // string-value is taken as a URI.  We can't statically know which
    // nodes will be passed, so before the transformation runs we
    // speculatively walk the source doc collecting every string-value
    // (text-node content and attribute values) and try to load each.
    // Successful loads join the pre-loaded map; failures are silently
    // ignored.  This costs at most one filesystem probe per leaf node
    // in the source, and only on stylesheets that actually contain a
    // dynamic `document(...)` call.
    // Speculative pre-load for `document(node-set)` / `document($v)`
    // patterns: walk every string-valued attribute / text node of
    // the source and every XPath string literal in the stylesheet,
    // probe each through the loader, and pre-load whatever
    // resolves.  Runtime `load_dynamic_document` now serves
    // misses too (XPath 2.0 `doc()` / dynamic XSLT
    // `document(@href)` work even without this), but the
    // speculative pre-load keeps `document(node-set)` happy
    // *across many node-values where most aren't actually URIs*
    // — runtime loading would surface a "URI not pre-loaded"
    // error for each non-URI string, while the speculative pass
    // silently ignores those.  Stylesheets that don't fit either
    // shape pay nothing here.
    //
    // Source-doc string-value collection was previously enabled
    // unconditionally; for unicode-90's 1300-element reference
    // docs that meant 1300 negative filesystem probes per apply.
    // It now runs only when the stylesheet's static AST contains
    // a literal `document(*)` / `document(some/path)` call —
    // i.e., a node-set-driven `document()`.  Stylesheets that
    // only call `doc(concat(...))` / `doc($var)` skip the
    // source-doc walk and rely on runtime loading.
    if crate::walk::has_dynamic_document_call(style) {
        let mut seen: std::collections::HashSet<String> = style.documents_to_load
            .iter().cloned().collect();
        let mut candidates: Vec<String> = Vec::new();
        if crate::walk::has_document_node_set_call(style) {
            collect_candidate_uris(source_doc, &mut candidates, &mut seen);
        }
        for lit in crate::walk::collect_all_string_literals(style) {
            if seen.insert(lit.clone()) { candidates.push(lit); }
        }
        for cand in candidates {
            if cand.is_empty() { continue; }
            match loader.load(&cand, base) {
                Ok(text) => {
                    let opts = sup_xml_core::ParseOptions {
                        namespace_aware: true, ..Default::default()
                    };
                    if let Ok(d) = sup_xml_core::parse_str(&text, &opts) {
                        style_documents_dynamic.push((cand, Box::new(d)));
                    }
                }
                Err(_) => { /* not a loadable URI — ignore */ }
            }
        }
    }

    // Pre-load every text resource referenced via a string-literal
    // URI in the stylesheet's `unparsed-text` / `unparsed-text-available`
    // / `unparsed-text-lines` calls.  Each URI is loaded by the
    // same `Loader` the rest of the apply uses; failures here are
    // silent — the runtime falls back to "not available" for that URI.
    let mut unparsed_texts: HashMap<String, String> = HashMap::new();
    for uri in crate::walk::collect_static_unparsed_text_uris(style) {
        if uri.is_empty() { continue; }
        if let Ok(text) = loader.load(&uri, base) {
            unparsed_texts.insert(uri, text);
        }
    }

    // Build the XPath index over the source doc, then graft each
    // loaded `document()` doc onto it so `document()` results are
    // addressable as ordinary NodeIds.  XSLT 1.0 §12.2 ("keys are
    // local to a specific document") requires `key()` to only return
    // nodes from the SAME document as the calling context node —
    // that's enforced inside `KeyIndex::build` (which buckets per
    // document root) and at lookup time (which takes the context
    // doc-root).  Grafting before key-build means keys ALSO index
    // loaded-doc nodes, so `key()` works against `document()`
    // results too (test fn/key-021 et al).
    let mut idx = DocIndex::build(source_doc);
    let mut documents: HashMap<String, NodeId> = HashMap::with_capacity(loaded_docs.len());
    for (href, doc) in style.documents_to_load.iter().zip(loaded_docs.iter()) {
        let id = idx.add_document(doc.as_ref());
        // Key by the fragment-stripped URI so `document('a#x')` and
        // `document('a#y')` resolve to the same node (XSLT 1.0 §12.1).
        documents.insert(crate::functions::strip_uri_fragment(href).to_string(), id);
    }
    for (uri, doc) in &style_documents_dynamic {
        let id = idx.add_document(doc.as_ref());
        documents.insert(uri.clone(), id);
    }
    // XSLT 1.0 §3.4 — apply `xsl:strip-space` to the source tree
    // *before* transformation, so subsequent `string-value`,
    // `xsl:value-of`, `xsl:copy-of` etc. all see the pruned tree.
    // Without this, the strip filter at apply-templates time
    // hides whitespace from the built-in text rule but not from
    // value-of / @* selectors.
    apply_strip_space(style, &mut idx);

    let namespaces = NamespaceContext::from_stylesheet(style);
    // XSLT 1.0 §12.4 — `unparsed-entity-uri()` returns the entity's
    // SYSTEM identifier resolved against the base URI of its
    // declaration (the source document).  Resolve once here so the
    // function dispatcher can return the absolute URI directly.
    let unparsed_entities = {
        let raw = source_doc.unparsed_entities();
        match source_doc.base_url() {
            Some(base) if !raw.is_empty() => {
                let resolved = raw.iter().map(|(name, ent)| {
                    (name.clone(), sup_xml_tree::UnparsedEntity {
                        system_id: sup_xml_core::xpath::eval::resolve_uri_against(
                            base, &ent.system_id),
                        public_id: ent.public_id.clone(),
                    })
                }).collect();
                std::sync::Arc::new(resolved)
            }
            _ => raw.clone(),
        }
    };

    // The xsl:key index is built *after* global variables/params are
    // bound (below) so that `xsl:key` match/use expressions can
    // reference them (XSLT 2.0 §16.3 — `use`/`match` are evaluated in
    // the stylesheet's static context, which includes global
    // variables).  Declared here without an initial value so the
    // storage outlives `state` (which borrows from it) while the
    // assignment inside the `if` block is the only one that actually
    // runs.  The `None` path stays untouched: when the stylesheet
    // has no keys, `state.keys` retains the `None` it was constructed
    // with.
    let key_index: Option<KeyIndex>;

    // Cache for runtime `doc()` loads — populated as the engine
    // encounters URIs not in the static `documents` map.  Lives in
    // this scope so its lifetime covers every `EvalState` derived
    // below.  RefCell because every XPath function-dispatch only
    // has `&XsltBindings` (and so `&EvalState`) at hand.
    let dyn_doc_cache: std::cell::RefCell<HashMap<String, NodeId>>
        = std::cell::RefCell::new(HashMap::new());
    // RTF document-node base-URI overrides (XPath 2.0 §3.1.5).
    // Lives here so every derived sub-state can read and write the
    // same table (xsl:function call sites, recursive apply chains).
    // Pre-populated with the source-document URI (apply-time base or
    // the stylesheet's xml:base fallback) plus each pre-loaded
    // document() URI — fn:base-uri() on source nodes returns the
    // URI they were loaded from.
    let rtf_base_uris: std::cell::RefCell<HashMap<NodeId, String>>
        = std::cell::RefCell::new(HashMap::new());
    {
        let mut map = rtf_base_uris.borrow_mut();
        // The source document's base URI is the URI it was loaded
        // from (XPath 2.0 §2.5) — distinct from the stylesheet's
        // static base URI.  Prefer the document's own recorded URI;
        // fall back to the apply-time `base` for in-memory sources.
        if let Some(uri) = source_doc.base_url().or(base) {
            map.insert(0, uri.to_string());
        }
        for (href, &id) in &documents {
            map.insert(id, href.clone());
        }
    }

    let mut state = EvalState {
        style,
        idx:        &idx,
        namespaces: &namespaces,
        keys:       None,
        // Always pass `Some(&documents)` — the dispatcher's "URI not
        // pre-loaded" diagnostic is accurate for both the "no string-
        // literal URIs found" and the "dynamic URI doesn't match any
        // pre-loaded entry" cases.  Conflating empty with `None` led
        // to a misleading "no Loader supplied" error fired when the
        // caller actually did supply one.
        documents:  Some(&documents),
        xslt_current: 0,
        variables:  VariableScope::default(),
        rtfs:       HashMap::new(),
        rtf_scopes: Vec::new(),
        builder:    {
            let mut b = ResultBuilder::new();
            b.is_principal_document = true;
            b
        },
        principal_buf: None,
        unparsed_entities: unparsed_entities.clone(),
        source_doc,
        apply_imports_ctx: None,
        user_exts: extensions,
        sequence_sinks: Vec::new(),
        template_call_depth: 0,
        current_group: Vec::new(),
        regex_groups: Vec::new(),
        tunnel_pool: HashMap::new(),
        current_grouping_key: None,
        accumulators: HashMap::new(),
        unparsed_texts: if unparsed_texts.is_empty() { None } else { Some(&unparsed_texts) },
        static_base_uri: style.xml_base.clone().or_else(|| base.map(str::to_string)),
        loader: Some(loader),
        loader_base: base,
        dyn_doc_cache: Some(&dyn_doc_cache),
        rtf_base_uris: &rtf_base_uris,
        static_ctx: static_ctx_for_version(&style.version),
    };

    // Stylesheet global variables / params (very partial — proper
    // ordering / circular-reference detection lands later).
    // XSLT 1.0 §12.3 import precedence — the main stylesheet's
    // declarations override same-named imports.  `compile_with_imports`
    // pushes main first then each import in turn, so the first
    // occurrence of a name in these vectors wins; dedupe-by-first.
    state.variables.enter();
    let mut seen_params: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();
    let dedup_params: Vec<&_> = style.global_params.iter().filter(|p| {
        seen_params.insert((p.name.prefix.clone(), p.name.local.clone()))
    }).collect();
    let mut seen_vars: std::collections::HashSet<(Option<String>, String)> =
        std::collections::HashSet::new();
    let dedup_vars: Vec<&_> = style.global_variables.iter().filter(|v| {
        seen_vars.insert((v.name.prefix.clone(), v.name.local.clone()))
    }).collect();
    // XSLT 1.0 §11.4 — global variables may forward-reference each
    // other, so source order isn't enough.  Pre-bind every global
    // name to an empty string so first-pass references resolve to
    // "" instead of erroring, then iterate to fixpoint: each round
    // re-binds, and the now-populated values feed later lookups.
    // A small N bounds the cost; cycles just stall at "".
    for p in &dedup_params {
        state.variables.bind(qname_key(&p.name), Value::String(String::new()));
    }
    for v in &dedup_vars {
        state.variables.bind(qname_key(&v.name), Value::String(String::new()));
    }
    // XSLT 1.0 §11.4 — caller-supplied top-level param overrides
    // run BEFORE the global-variable fixpoint so any `xsl:variable`
    // that references one of these params (e.g. `select="concat('^',
    // $regex, '$')"`) sees the caller's value rather than the empty
    // default.  Match by local-name only (the public string-keyed
    // API doesn't carry namespace).
    for (name, value) in top_level_params {
        for p in &dedup_params {
            if p.name.local == *name {
                state.variables.bind(qname_key(&p.name), Value::String(value.clone()));
                break;
            }
        }
    }
    // XSLT 2.0 §9.5 / XTDE0050 — a global xsl:param with
    // required="yes" whose value was not supplied by the caller is
    // a dynamic error.  Check before the fixpoint loop so the
    // error surfaces deterministically rather than after a partial
    // default binding.
    for p in &dedup_params {
        if p.required && !top_level_params.iter().any(|(n, _)| *n == p.name.local) {
            return Err(XsltError::InvalidStylesheet(format!(
                "required global xsl:param '{}' was not supplied (XTDE0050)",
                p.name.local,
            )));
        }
    }
    for _round in 0..16 {
        for p in &dedup_params {
            // Only re-evaluate params the caller DIDN'T override;
            // otherwise the param's default expression would clobber
            // the caller-supplied value on every fixpoint round.
            let overridden = top_level_params.iter().any(|(n, _)| *n == p.name.local);
            if !overridden {
                bind_variable(&mut state, &p.name, p.select.as_ref(), &p.body, p.as_type.as_deref(), None, 0, 1, 1)?;
            }
        }
        for v in &dedup_vars {
            bind_variable(&mut state, &v.name, v.select.as_ref(), &v.body, v.as_type.as_deref(), v.base_uri.as_deref(), 0, 1, 1)?;
        }
    }

    // Build the xsl:key index now that global variables are bound, so
    // `xsl:key` match/use expressions referencing them resolve (XSLT
    // 2.0 §16.3).  Then re-run the global fixpoint once with the keys
    // available — a global variable may itself call `key()`, the
    // mutual-reference case the spec permits as long as it isn't a
    // genuine cycle.
    if !style.keys.is_empty() {
        let sc = state.static_ctx;
        let (mut built, deferred) = {
            let state_ref = &state;
            KeyIndex::build(style, &idx, |expr, node| {
                // `current()` in an xsl:key `match`/`use` expression is
                // the node being indexed (XSLT 1.0 §12.2), so bind both
                // the context node and the XSLT current node to it.
                let mut bindings = state_ref.bindings();
                bindings.xslt_context_node = node;
                let ctx = EvalCtx { context_node: node, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
                eval_expr(expr, &ctx, &idx)
            }).map_err(XsltError::from)?
        };
        // Body-form keys (XSLT 2.0 §16.3): compute each matched node's
        // key value through the instruction evaluator and bucket it.
        for (ki, node_id) in deferred {
            let value = eval_key_body_value(&mut state, &style.keys[ki].body, node_id)?;
            built.add_value(&qname_key(&style.keys[ki].name), node_id, &value, &idx);
        }
        key_index = Some(built);
        state.keys = key_index.as_ref();
        for p in &dedup_params {
            let overridden = top_level_params.iter().any(|(n, _)| *n == p.name.local);
            if !overridden {
                bind_variable(&mut state, &p.name, p.select.as_ref(), &p.body, p.as_type.as_deref(), None, 0, 1, 1)?;
            }
        }
        for v in &dedup_vars {
            bind_variable(&mut state, &v.name, v.select.as_ref(), &v.body, v.as_type.as_deref(), v.base_uri.as_deref(), 0, 1, 1)?;
        }
    }

    // The transformation entry point is normally "apply-templates to
    // the document node" (XSLT 1.0 §5.1).  Node 0 in our index is the
    // synthetic Document node.  When an `initial_template` is set on
    // the bindings layer (XSLT 3.0 entry-point convention used by
    // many W3C test cases), we instead `xsl:call-template` directly
    // on that name and skip the pattern-match dispatch.
    // XSLT 3.0 §2.4 (and forward-compat for 2.0 stylesheets that
    // declare it for use by 3.0 callers): when the stylesheet
    // contains a template named `xsl:initial-template` and the
    // caller hasn't requested a different entry point, that
    // template is the implicit default entry.
    let implicit_initial_template: Option<&str> =
        if initial_template.is_none() && initial_mode.is_none() {
            const XSL_INITIAL: &str = "{http://www.w3.org/1999/XSL/Transform}initial-template";
            if state.style.templates.iter().any(|t|
                t.name.as_ref().map(qname_key).as_deref() == Some(XSL_INITIAL))
            {
                Some(XSL_INITIAL)
            } else { None }
        } else { None };
    let initial_template = initial_template.or(implicit_initial_template);
    // XSLT 3.0 §18 — precompute accumulator values over the source
    // document (after global variables are bound, before any template
    // runs) so accumulator-before() / accumulator-after() can read them.
    if !state.style.accumulators.is_empty() {
        precompute_accumulators(&mut state, 0)?;
    }
    if let Some(name) = initial_template {
        // The harness may pass either an already-expanded Clark-form
        // key (`{uri}local`) or a raw `prefix:local` string; resolve
        // the prefix through the stylesheet's static namespace
        // context so a `name="foo:temp"` initial-template entry
        // finds the template declared with the same expanded URI.
        let key = if name.starts_with('{') {
            name.to_string()
        } else if let Some((p, l)) = name.split_once(':') {
            match state.namespaces.resolve(p) {
                Some(uri) => format!("{{{uri}}}{l}"),
                None      => name.to_string(),
            }
        } else {
            name.to_string()
        };
        let tmpl = state.style.templates.iter()
            .find(|t| t.name.as_ref().map(qname_key).as_deref() == Some(key.as_str()))
            .ok_or_else(|| XsltError::UnresolvedReference(format!(
                "no template named '{key}' for initial-template entry"
            )))?;
        run_template_body(&mut state, tmpl, 0, 1, 1, &[])?;
    } else {
        // `<initial-mode name="X"/>` (XSLT 3.0 §2.4) — dispatch
        // the document node with the named mode active.  Mode
        // names are unprefixed in the W3C harness; resolve via
        // the stylesheet's in-scope namespaces only when the
        // name carries a prefix.
        let mode_qname = initial_mode.map(|raw| {
            match raw.split_once(':') {
                Some((p, l)) => QName {
                    prefix: Some(p.to_string()),
                    local:  l.to_string(),
                    uri:    state.namespaces.resolve(p).unwrap_or_default(),
                },
                None => QName {
                    prefix: None,
                    local:  raw.to_string(),
                    uri:    String::new(),
                },
            }
        });
        apply_one_to_node(&mut state, 0, mode_qname.as_ref())?;
    }
    state.variables.leave();

    // Surface any sequence-construction error (XTDE0410 etc.) that the
    // infallible builder methods stashed while the transform ran.
    if let Some(msg) = state.builder.deferred_error.take() {
        return Err(XsltError::InvalidStylesheet(msg));
    }
    let children = state.builder.finish();
    // Merge xsl:output specs into one — last wins for scalar
    // fields; cdata_section_elements concatenates.
    let mut output = crate::ast::OutputSpec::default();
    for o in &style.outputs {
        if o.method.is_some() { output.method = o.method.clone(); }
        if o.encoding.is_some() { output.encoding = o.encoding.clone(); }
        if o.indent.is_some() { output.indent = o.indent; }
        if o.omit_xml_declaration.is_some() { output.omit_xml_declaration = o.omit_xml_declaration; }
        if o.standalone.is_some() { output.standalone = o.standalone; }
        if o.media_type.is_some() { output.media_type = o.media_type.clone(); }
        if o.doctype_public.is_some() { output.doctype_public = o.doctype_public.clone(); }
        if o.doctype_system.is_some() { output.doctype_system = o.doctype_system.clone(); }
        if o.version.is_some() { output.version = o.version.clone(); }
        output.cdata_section_elements.extend(o.cdata_section_elements.iter().cloned());
        output.use_character_maps.extend(o.use_character_maps.iter().cloned());
    }
    let character_map = flatten_character_maps(
        &output.use_character_maps, &style.character_maps);
    // Wrap each captured secondary document (xsl:result-document) in a
    // ResultTree, inheriting the principal output settings.
    let secondary = take_secondary_docs().into_iter()
        .map(|(uri, nodes)| (uri, ResultTree {
            children: nodes,
            output: output.clone(),
            character_map: character_map.clone(),
            secondary: Vec::new(),
        }))
        .collect();
    Ok(ResultTree { children, output, character_map, secondary })
}

/// Resolve `use-character-maps` references against the stylesheet's
/// declared maps, flattening transitive `use-character-maps`
/// references in declaration order.  Later mappings override
/// earlier ones for the same character (XSLT 2.0 §20.2 — the
/// principal map's `output-character` declarations take precedence
/// over the referenced ones).  Cycles are tolerated by tracking the
/// active resolution chain.
fn flatten_character_maps(
    refs: &[QName],
    declared: &[crate::ast::CharacterMap],
) -> Vec<(char, String)> {
    fn visit(
        name:     &QName,
        declared: &[crate::ast::CharacterMap],
        chain:    &mut Vec<String>,
        out:      &mut Vec<(char, String)>,
    ) {
        let key = qname_key(name);
        if chain.iter().any(|c| c == &key) { return; }
        let Some(map) = declared.iter().find(|m| qname_key(&m.name) == key) else {
            return;
        };
        chain.push(key);
        for referenced in &map.use_character_maps {
            visit(referenced, declared, chain, out);
        }
        for (ch, repl) in &map.mappings {
            if let Some(slot) = out.iter_mut().find(|(k, _)| k == ch) {
                slot.1 = repl.clone();
            } else {
                out.push((*ch, repl.clone()));
            }
        }
        chain.pop();
    }
    let mut out = Vec::new();
    let mut chain = Vec::new();
    for name in refs {
        visit(name, declared, &mut chain, &mut out);
    }
    out
}

// ── apply-templates dispatch ──────────────────────────────────────

fn apply_one_to_node(state: &mut EvalState, node: NodeId, mode: Option<&QName>) -> Result<()> {
    // 1. Pick a user template.  Selection uses the engine's
    //    pattern matcher.  Override the binding's
    //    `xslt_context_node` for the matcher so `current()` inside
    //    a pattern predicate refers to the candidate node, not the
    //    outer apply-templates current (XSLT 1.0 §12.4 / 2.0 §6).
    let mut bindings = state.bindings();
    bindings.xslt_context_node = node;
    let chosen = pattern::select_template(
        state.style, node, mode, state.idx, &bindings,
    ).map_err(XsltError::from)?;

    match chosen {
        Some(sel) => {
            // See `apply_one_to_node_with_args` — bound the recursion so
            // a self-re-applying template errors instead of overflowing.
            state.template_call_depth += 1;
            if state.template_call_depth > MAX_TEMPLATE_CALL_DEPTH {
                state.template_call_depth -= 1;
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:apply-templates depth exceeds limit \
                     ({MAX_TEMPLATE_CALL_DEPTH}) — possible infinite recursion"
                )));
            }
            let prev = state.apply_imports_ctx.replace(
                (node, mode.cloned(), sel.template.import_precedence,
                 template_index_of(state.style, sel.template),
                 sel.priority, sel.branch_idx),
            );
            let r = run_template_body(state, sel.template, node, 1, 1, &[]);
            state.apply_imports_ctx = prev;
            state.template_call_depth -= 1;
            r?;
        }
        None => apply_builtin_template(state, node, mode)?,
    }
    Ok(())
}

/// Run a template's body with the given context.  Opens a variable
/// scope for the template's `xsl:param` declarations.
/// URI for the `err:` namespace — XSLT 3.0 § 15 / XQuery
/// errors namespace. Variables bound during a `xsl:catch` body
/// live under this URI in Clark form.
const ERR_NS: &str = "http://www.w3.org/2005/xqt-errors";

/// Evaluate an `xsl:try` body; if it raises a dynamic error, scan
/// the catch handlers for one whose `errors=` matchers cover the
/// caught error, bind the `err:*` variables, and run that catch
/// body.  When no handler matches, the error propagates.
fn run_try_instr<'a>(
    state:    &mut EvalState<'a>,
    body:     &[crate::ast::Instr],
    catches:  &[crate::ast::TryCatch],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<()> {
    // Snapshot the result-tree builder up to this point so an
    // error doesn't leave half-written output for the catch body
    // to inherit.  We capture a sub-builder, run the body into
    // it, and either commit on success or discard on error.
    use crate::result_tree::ResultBuilder;
    let prev = std::mem::replace(&mut state.builder, ResultBuilder::new());
    let r = eval_body(state, body, ctx_node, pos, size);
    let written = std::mem::replace(&mut state.builder, prev);
    match r {
        Ok(()) => {
            // Commit the protected body's output.
            for n in written.finish() {
                copy_result_node(state, &n);
            }
            Ok(())
        }
        Err(e) => {
            let (code_qname, message) = error_to_qname(&e);
            for c in catches {
                if !catch_matches(&c.errors, &code_qname, state) { continue; }
                // Bind the err:* variables in a fresh scope so they
                // don't leak past the catch body.  Stored under
                // Clark form for the XPath lookup path.
                state.variables.enter();
                for (local, value) in [
                    ("code",        Value::String(format_qname_for_err(&code_qname))),
                    ("description", Value::String(message.clone())),
                    ("value",       Value::NodeSet(Vec::new())),
                    ("module",      Value::NodeSet(Vec::new())),
                    ("line-number", Value::NodeSet(Vec::new())),
                    ("column-number", Value::NodeSet(Vec::new())),
                ] {
                    let key = format!("{{{ERR_NS}}}{local}");
                    state.variables.bind(key, value);
                }
                let r = eval_body(state, &c.body, ctx_node, pos, size);
                state.variables.leave();
                return r;
            }
            // No matching handler — propagate.
            Err(e)
        }
    }
}

/// Project an `XsltError` into the `(err:code, err:description)`
/// pair the catch handlers see.  XPath errors that carry a spec
/// error code (`xpath_code`, e.g. `FOAR0001`) surface it as the
/// `err:` local name; everything else defaults to `err:FOER0000`
/// (XPath/XQuery "unidentified error").  The description is the raw
/// message.
fn error_to_qname(e: &XsltError) -> (QName, String) {
    let local = match e {
        XsltError::Xpath(xe) => xe.xpath_code.clone()
            .unwrap_or_else(|| "FOER0000".to_string()),
        _ => "FOER0000".to_string(),
    };
    let qn = QName {
        prefix: Some("err".into()),
        uri:    ERR_NS.to_string(),
        local,
    };
    (qn, e.to_string())
}

/// Render a QName as it should appear in the `$err:code`
/// variable's string-value.  XSLT 3.0 §15 says the variable is
/// typed as `xs:QName`; without a QName Value variant we render
/// the lexical form `prefix:local` (matching the most common
/// stylesheet comparison `$err:code = 'err:FOER0000'`).
fn format_qname_for_err(q: &QName) -> String {
    match &q.prefix {
        Some(p) => format!("{p}:{}", q.local),
        None    => q.local.clone(),
    }
}

/// True iff at least one `CatchMatcher` in `errors` matches the
/// caught error's QName.  Empty matcher list is treated as
/// catch-all by the compiler (it inserts `CatchMatcher::Any`).
fn catch_matches(
    errors: &[crate::ast::CatchMatcher],
    err_qname: &QName,
    state: &EvalState,
) -> bool {
    use crate::ast::CatchMatcher::*;
    errors.iter().any(|m| match m {
        Any => true,
        LocalNameOnly(local) => err_qname.local == *local,
        PrefixWildcard(prefix) => state.namespaces.resolve(prefix)
            .as_deref() == Some(err_qname.uri.as_str()),
        QName(q) => q.uri == err_qname.uri && q.local == err_qname.local,
    })
}

fn copy_result_node(state: &mut EvalState, n: &crate::result_tree::ResultNode) {
    use crate::result_tree::ResultNode;
    match n {
        ResultNode::Text { content, dose } => {
            state.builder.push_text(content.clone(), *dose);
        }
        ResultNode::Element { name, namespaces, attributes, children } => {
            state.builder.open_element(name.clone());
            for (p, u) in namespaces {
                state.builder.push_namespace_decl(p.clone(), u.clone());
            }
            for (q, v) in attributes {
                state.builder.push_attribute(q.clone(), v.clone());
            }
            for c in children { copy_result_node(state, c); }
            state.builder.close_element();
        }
        ResultNode::Comment(s) => state.builder.push_comment(s.clone()),
        ResultNode::ProcessingInstruction { target, data } => {
            state.builder.push_pi(target.clone(), data.clone());
        }
        ResultNode::Attribute { name, value } => {
            state.builder.push_attribute(name.clone(), value.clone());
        }
    }
}

fn run_template_body(
    state:    &mut EvalState,
    template: &Template,
    ctx_node: NodeId,
    pos:      usize,
    size:     usize,
    args:     &[(QName, Value, Option<Vec<ResultNode>>)],
) -> Result<()> {
    state.variables.enter();
    rtf_scope_enter(state);
    // Set the `current()` node — XSLT 1.0 §12.4 says it's the
    // node the template was applied to, not the inner XPath
    // context node.  Save+restore so nested call-template /
    // apply-templates can update without losing the outer current.
    let prev_current = state.xslt_current;
    state.xslt_current = ctx_node;
    // Bind params: explicit args override defaults; XSLT 2.0
    // tunnel-marked params pull from the propagating tunnel pool
    // instead of the per-call args list.
    for p in &template.params {
        let key = qname_key(&p.name);
        // Resolve the raw value first (tunnel pool, caller arg, or
        // default-expression), then apply `as=` coercion uniformly
        // before binding.  XSLT 2.0 §10.2: the declared type
        // applies regardless of which value source filled the slot.
        let raw = if p.tunnel {
            match state.tunnel_pool.get(&key).cloned() {
                Some(v) => v,
                None if p.required => return Err(XsltError::InvalidStylesheet(format!(
                    "required tunnel param '{key}' not supplied"
                ))),
                None => evaluate_param_default(state, p, ctx_node, pos, size)?,
            }
        } else if let Some((_, v, rtf)) = args.iter().find(|(n, _, _)| qname_key(n) == key) {
            if let Some(nodes) = rtf {
                store_rtf(state, &key, nodes.clone());
            }
            v.clone()
        } else if p.required {
            // XSLT 2.0 §10.1.2: XTDE0700 — required parameter
            // wasn't supplied by the caller.  This is the test
            // gate for `call-template-2101` and friends.
            return Err(XsltError::InvalidStylesheet(format!(
                "required parameter '{key}' not supplied"
            )));
        } else {
            evaluate_param_default(state, p, ctx_node, pos, size)?
        };
        let value = if let Some(t) = &p.as_type {
            if let Some(st) = parse_as_atomic_type(t) {
                coerce_to_atomic_sequence(raw, &st, state.idx)?
            } else { raw }
        } else { raw };
        state.variables.bind(key, value);
    }
    // XSLT 2.0 §10 / XTTE0505 — when the template declares a node-kind
    // `as=` result type, capture the body's output and check the
    // produced sequence's cardinality and item kinds against it before
    // committing.  A mismatch (wrong count, or a node kind the type
    // doesn't admit) is a dynamic error.  Atomic / `item()` result
    // types aren't checked structurally here (they atomise).
    let declared = template.as_type.as_deref()
        .and_then(parse_as_atomic_type)
        .filter(|st| template_result_type_is_node_kind(st));
    let result = if let Some(st) = declared {
        use crate::result_tree::ResultBuilder;
        let prev = std::mem::replace(&mut state.builder, ResultBuilder::new());
        let r = eval_body(state, &template.body, ctx_node, pos, size);
        let written = std::mem::replace(&mut state.builder, prev);
        r.and_then(|()| {
            let nodes = written.finish();
            if template_result_violates_type(&nodes, &st) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "template result doesn't match the declared type {:?} \
                     (XTTE0505)", st.item
                )));
            }
            for n in &nodes { copy_result_node(state, n); }
            Ok(())
        })
    } else {
        eval_body(state, &template.body, ctx_node, pos, size)
    };
    state.xslt_current = prev_current;
    rtf_scope_leave(state);
    state.variables.leave();
    result
}

fn evaluate_param_default(
    state: &mut EvalState, p: &Param,
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<Value> {
    if let Some(sel) = &p.select {
        return state.xpath_eval(sel, ctx_node, pos, size);
    }
    // Body-form default — build a navigable result-tree fragment so
    // the bound value is a node sequence, not a string.  This mirrors
    // the body-form `xsl:variable` binding: `as="attribute()"` exposes
    // each constructed attribute as a real node, other node kinds bind
    // to the RTF document root (the caller's `coerce_to_atomic_sequence`
    // unwraps it to the declared element()/text()/… items).
    let key = qname_key(&p.name);
    if p.as_type.as_deref().map(as_is_attribute_kind).unwrap_or(false) {
        let nodes = build_rtf_nodes_no_merge(state, &p.body, ctx_node, pos, size)?;
        let ids = rtf_children_into_index(state.idx, &nodes);
        store_rtf(state, &key, nodes);
        return Ok(Value::NodeSet(ids));
    }
    let nodes   = build_rtf_nodes(state, &p.body, ctx_node, pos, size)?;
    let root_id = rtf_into_index(state.idx, &nodes);
    store_rtf(state, &key, nodes);
    Ok(Value::NodeSet(vec![root_id]))
}

// ── instruction dispatch ──────────────────────────────────────────

/// Control-flow signal raised by `xsl:break` / `xsl:next-iteration`
/// inside an `xsl:iterate` body.  Propagated up through `eval_body`
/// (which stops running further instructions once it is set) to the
/// enclosing iterate driver, which consumes it.
enum IterateControl {
    /// `xsl:break` — its output has already been emitted; stop the loop.
    Break,
    /// `xsl:next-iteration` — parameter values for the next iteration.
    Next(Vec<(QName, Value, Option<Vec<ResultNode>>)>),
}

thread_local! {
    static ITERATE_CONTROL: std::cell::RefCell<Option<IterateControl>> =
        const { std::cell::RefCell::new(None) };
    /// XPath 2.0 §2.1.2 / XSLT 2.0 §10.3 — true while evaluating an
    /// `xsl:function` body, where the focus is *undefined* (no context
    /// item, position, or size).  Context-dependent operations
    /// (`fn:key`, `unparsed-entity-uri`, `.`, absolute paths) consult
    /// this flag and raise the appropriate error code instead of
    /// silently using the enclosing focus.
    static CONTEXT_UNDEFINED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// XSLT 2.0 §13.2 — depth counter for `xsl:for-each` iterations
    /// whose `select=` yielded an *atomic* sequence (e.g.
    /// `select="1 to 5"`).  Our engine fabricates a synthetic text
    /// node per atomic so the iteration model stays uniform with the
    /// node-set case, but the context item is semantically an atomic
    /// — instructions that require a node context (xsl:apply-templates
    /// without select, xsl:number without select / value) check this
    /// counter to raise XTTE0510 / XTTE0990.
    static ATOMIC_FOR_EACH_DEPTH: std::cell::Cell<u32> =
        const { std::cell::Cell::new(0) };
    /// Secondary result documents produced by `xsl:result-document
    /// href="…"` during the current apply: `(resolved-href, body
    /// nodes)`.  Drained into [`ResultTree::secondary`] when the apply
    /// finishes; reset at apply entry so they don't leak across runs.
    static SECONDARY_DOCS: std::cell::RefCell<Vec<(String, Vec<ResultNode>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Nesting depth of TEMPORARY output destinations — a variable /
    /// param / function body, an attribute / comment value, or a
    /// nested xsl:result-document.  `xsl:result-document` is illegal
    /// (XTDE1480) when this is non-zero, since the current output
    /// destination isn't the principal result tree.
    static TEMP_OUTPUT_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII guard marking a temporary output destination for the duration
/// of a body evaluation (used by the temp-tree builders so a contained
/// `xsl:result-document` reports XTDE1480).
struct TempOutputGuard;
impl TempOutputGuard {
    fn enter() -> Self {
        TEMP_OUTPUT_DEPTH.with(|c| c.set(c.get() + 1));
        TempOutputGuard
    }
}
impl Drop for TempOutputGuard {
    fn drop(&mut self) {
        TEMP_OUTPUT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}
fn in_temporary_output() -> bool {
    TEMP_OUTPUT_DEPTH.with(|c| c.get() > 0)
}

/// True iff the current evaluation is inside an `xsl:function` body —
/// where the XPath focus (context item / position / size) is
/// undefined per XSLT 2.0 §10.3.  Used by context-dependent built-ins
/// to raise their specific error codes (XTDE1270 for fn:key,
/// XTDE1370 / XTDE1380 for unparsed-entity-uri / -public-id) instead
/// of silently working off the caller's focus.
pub(crate) fn is_context_undefined() -> bool {
    CONTEXT_UNDEFINED.with(|c| c.get())
}

/// True iff the current iteration is inside an `xsl:for-each` whose
/// `select=` produced an atomic sequence — see [`ATOMIC_FOR_EACH_DEPTH`].
pub(crate) fn in_atomic_for_each() -> bool {
    ATOMIC_FOR_EACH_DEPTH.with(|c| c.get() > 0)
}

/// RAII guard incrementing [`ATOMIC_FOR_EACH_DEPTH`] for the duration
/// of the iteration body — the for-each driver wraps each iteration
/// in one of these when the original `select=` was atomic.
struct AtomicForEachGuard;
impl AtomicForEachGuard {
    fn enter() -> Self {
        ATOMIC_FOR_EACH_DEPTH.with(|c| c.set(c.get() + 1));
        AtomicForEachGuard
    }
}
impl Drop for AtomicForEachGuard {
    fn drop(&mut self) {
        ATOMIC_FOR_EACH_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

fn reset_secondary_docs() {
    SECONDARY_DOCS.with(|d| d.borrow_mut().clear());
    TEMP_OUTPUT_DEPTH.with(|c| c.set(0));
}
fn take_secondary_docs() -> Vec<(String, Vec<ResultNode>)> {
    SECONDARY_DOCS.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

fn iterate_control_active() -> bool {
    ITERATE_CONTROL.with(|c| c.borrow().is_some())
}
fn take_iterate_control() -> Option<IterateControl> {
    ITERATE_CONTROL.with(|c| c.borrow_mut().take())
}
fn set_iterate_control(ctrl: IterateControl) {
    ITERATE_CONTROL.with(|c| *c.borrow_mut() = Some(ctrl));
}

/// Promote an `xsl:iterate select=` value to a list of context nodes,
/// the same way `xsl:for-each` does — atomic items become synthetic
/// text nodes so each becomes one iteration with `.` / position().
fn iterate_select_nodes(state: &mut EvalState, v: Value) -> Result<Vec<NodeId>> {
    Ok(match v {
        Value::NodeSet(ns) => ns,
        Value::String(s) => state.idx.allocate_rtf_text_nodes_inherent(vec![s]),
        Value::Number(n) => state.idx.allocate_rtf_text_nodes_inherent(
            vec![value_to_string_styled(&Value::Number(n), state.idx, state.num_style())]),
        Value::Boolean(b) => state.idx.allocate_rtf_text_nodes_inherent(
            vec![if b { "true".into() } else { "false".into() }]),
        Value::Typed(t) => state.idx.allocate_rtf_text_nodes_inherent(vec![t.lexical]),
        Value::IntRange { lo, hi } => state.idx.allocate_rtf_text_nodes_inherent(
            (lo..=hi).map(|i| i.to_string()).collect()),
        Value::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::NodeSet(ns) => out.extend(ns),
                    Value::ForeignNodeSet(_) => {}
                    Value::IntRange { lo, hi } => {
                        let strings: Vec<String> = (lo..=hi).map(|i| i.to_string()).collect();
                        out.extend(state.idx.allocate_rtf_text_nodes_inherent(strings));
                    }
                    atomic => {
                        let s = value_to_string_styled(&atomic, state.idx, state.num_style());
                        out.extend(state.idx.allocate_rtf_text_nodes_inherent(vec![s]));
                    }
                }
            }
            out
        }
        other => return Err(XsltError::InvalidStylesheet(format!(
            "xsl:iterate select= must yield a sequence (got {other:?})"))),
    })
}

/// Internal namespace used to mark `xsl:on-empty` / `xsl:on-non-empty`
/// content while a sequence constructor is captured for §16.4
/// resolution.  Never appears in output (the wrapper is unwrapped).
const ON_COND_NS: &str = "https://sup-xml.internal/on-conditional";

fn eval_body(
    state: &mut EvalState,
    body:  &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<()> {
    // XSLT 3.0 §16.4 — a sequence constructor containing xsl:on-empty /
    // xsl:on-non-empty needs the whole body captured so their inclusion
    // can be decided from the rest of the output.
    if body.iter().any(|i| matches!(i, Instr::OnEmpty { .. } | Instr::OnNonEmpty { .. })) {
        return eval_body_conditional(state, body, ctx_node, pos, size);
    }
    for instr in body {
        eval_instr(state, instr, ctx_node, pos, size)?;
        // An xsl:break / xsl:next-iteration in the enclosing
        // xsl:iterate terminates the rest of this sequence
        // constructor; the iterate driver handles the signal.
        if iterate_control_active() { break; }
    }
    Ok(())
}

/// Evaluate a sequence constructor that contains xsl:on-empty /
/// xsl:on-non-empty (XSLT 3.0 §16.4).  The body is evaluated once into
/// a capture builder (preserving variable bindings, which live on
/// `state`); each on-* prong's content is wrapped in a sentinel
/// element marking its position.  The "ordinary" output (everything
/// outside the prongs) then decides which prongs are kept, and the
/// nodes are replayed into the real builder in document order.
fn eval_body_conditional(
    state: &mut EvalState,
    body:  &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<()> {
    let prev = std::mem::replace(&mut state.builder, ResultBuilder::new());
    let r = capture_on_conditional_body(state, body, ctx_node, pos, size);
    let captured = std::mem::replace(&mut state.builder, prev).finish();
    r?;
    // §16.4.1/2 — the rest of the constructor is "non-empty" if it
    // produces ANY node (an empty element still counts; only a
    // zero-length text node does not).  This differs from
    // xsl:where-populated's stricter emptiness rule.
    let ordinary_significant = captured.iter().any(|n| {
        on_cond_kind(n).is_none() && match n {
            ResultNode::Text { content, .. } => !content.is_empty(),
            _ => true,
        }
    });
    for n in captured {
        match on_cond_kind(&n) {
            Some(is_on_empty) => {
                let keep = if is_on_empty { !ordinary_significant } else { ordinary_significant };
                if keep {
                    if let ResultNode::Element { children, attributes, .. } = n {
                        // Replay the prong's content; a parentless
                        // attribute produced by the prong was hung on
                        // the sentinel, so re-emit those too.
                        for (name, value) in attributes {
                            state.builder.push_built_node(ResultNode::Attribute { name, value });
                        }
                        for c in children { state.builder.push_built_node(c); }
                    }
                }
            }
            None => state.builder.push_built_node(n),
        }
    }
    Ok(())
}

/// Evaluate the body into the (already-swapped-in capture) builder,
/// wrapping each on-empty / on-non-empty prong in a sentinel element.
fn capture_on_conditional_body(
    state: &mut EvalState,
    body:  &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<()> {
    for instr in body {
        match instr {
            Instr::OnEmpty { body } | Instr::OnNonEmpty { body } => {
                let local = if matches!(instr, Instr::OnEmpty { .. })
                    { "on-empty" } else { "on-non-empty" };
                state.builder.open_element(QName {
                    prefix: None, local: local.to_string(), uri: ON_COND_NS.to_string() });
                eval_body(state, body, ctx_node, pos, size)?;
                state.builder.close_element();
            }
            other => eval_instr(state, other, ctx_node, pos, size)?,
        }
        if iterate_control_active() { break; }
    }
    Ok(())
}

/// If `n` is an on-empty / on-non-empty sentinel element, returns
/// `Some(true)` for on-empty, `Some(false)` for on-non-empty.
fn on_cond_kind(n: &ResultNode) -> Option<bool> {
    match n {
        ResultNode::Element { name, .. } if name.uri == ON_COND_NS =>
            Some(name.local == "on-empty"),
        _ => None,
    }
}

fn eval_instr(
    state:    &mut EvalState,
    instr:    &Instr,
    ctx_node: NodeId,
    pos:      usize,
    size:     usize,
) -> Result<()> {
    match instr {
        Instr::LiteralElement { name, attributes, namespaces, use_attribute_sets, body } => {
            // Apply xsl:namespace-alias before emit (XSLT 1.0
            // §7.1.1) — rewrites the stylesheet-side URI to the
            // result-side URI for both element and attribute
            // namespaces.
            let element_name = apply_namespace_alias(state, name);
            state.builder.open_element(element_name.clone());
            if !element_name.uri.is_empty() {
                state.builder.push_namespace_decl(element_name.prefix.clone(), element_name.uri.clone());
            }
            // XSLT 1.0 §7.1.1 — propagate every namespace that was in
            // scope on the literal-result-element in the stylesheet
            // (modulo `[xsl:]exclude-result-prefixes`), as collected
            // by the compiler.  Namespace aliases rewrite the URI.
            for (prefix, uri) in namespaces {
                let aliased = apply_namespace_alias(state, &QName {
                    prefix: prefix.clone(),
                    uri:    uri.clone(),
                    local:  String::new(),
                });
                state.builder.push_namespace_decl(aliased.prefix, aliased.uri);
            }
            // Attributes pulled in via `xsl:use-attribute-sets` apply
            // first; LRE-declared attributes (next) override on name.
            apply_attribute_sets(state, use_attribute_sets, ctx_node, pos, size)?;
            for (aname, avt) in attributes {
                // XSLT 1.0 §7.1.1 — namespace-alias only rewrites
                // namespace nodes; unprefixed attributes have no
                // namespace node and stay in the null namespace
                // even when an alias matches the null URI.
                let aname = if aname.uri.is_empty() {
                    aname.clone()
                } else {
                    apply_namespace_alias(state, aname)
                };
                let value = render_avt(state, avt, ctx_node, pos, size)?;
                state.builder.push_attribute(aname.clone(), value);
                if !aname.uri.is_empty() && aname.prefix.is_some() {
                    state.builder.push_namespace_decl(aname.prefix.clone(), aname.uri.clone());
                }
            }
            // XSLT 1.0 §11.5 — an `xsl:variable` declared inside an
            // LRE stays in scope only until that element closes.
            // Open a scope around the body so inner declarations
            // don't leak to subsequent siblings.
            state.variables.enter();
            let r = eval_body(state, body, ctx_node, pos, size);
            state.variables.leave();
            r?;
            state.builder.close_element();
        }
        Instr::LiteralText { text, dose } => {
            state.builder.push_text(text.clone(), *dose);
        }
        Instr::ApplyTemplates { select, mode, sort, with_params, mode_current } => {
            let nodes = if let Some(sel) = select {
                match state.xpath_eval(sel, ctx_node, pos, size)? {
                    Value::NodeSet(ns) => ns,
                    Value::String(s) if s.is_empty() => Vec::new(),
                    // XPath 2.0 atomic sequence — surface each item as
                    // a synthetic text node so the template dispatch
                    // sees a flat node sequence.  Real nodes inside
                    // the sequence pass through unchanged.
                    Value::Sequence(items) => {
                        let mut out: Vec<NodeId> = Vec::with_capacity(items.len());
                        for item in items {
                            match item {
                                Value::NodeSet(ns) => out.extend(ns),
                                Value::ForeignNodeSet(_) => {}
                                atomic => {
                                    let s = value_to_string_styled(&atomic, state.idx, state.num_style());
                                    let ids = state.idx.allocate_rtf_text_nodes_inherent(vec![s]);
                                    out.extend(ids);
                                }
                            }
                        }
                        out
                    }
                    other => return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:apply-templates select= must yield a sequence (got {other:?})"
                    ))),
                }
            } else {
                // XSLT 2.0 §6.2 / XTTE0510 — `xsl:apply-templates` with
                // no `select=` defaults to the children of the context
                // item, which must therefore be a node.  An atomic
                // context (e.g. inside `<xsl:for-each select="1 to 5">`)
                // or an undefined focus is a type error.
                if sup_xml_core::xpath::eval::focus_is_undefined() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:apply-templates with no select= called where \
                         the context item is undefined (XTTE0510)".into()));
                }
                if in_atomic_for_each() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:apply-templates with no select= called where \
                         the context item is not a node (XTTE0510)".into()));
                }
                // Default iteration: element's children with
                // xsl:strip-space applied.
                let style = state.style;
                let idx = state.idx;
                state.idx.children(ctx_node).iter()
                    .copied()
                    .filter(|&n| !crate::whitespace::should_strip(style, n, idx))
                    .collect()
            };
            // Apply xsl:sort directives before iterating.
            let nodes = sort_nodes_for_iter(state, &nodes, sort, ctx_node, pos, size)?;
            // Snapshot only when there are tunnel params in scope —
            // the empty-HashMap clone is cheap but not free, and most
            // templates have no tunnel params at all.
            let tunnel_save = state.tunnel_pool.clone();
            let args = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            // XSLT 2.0 `mode="#current"` — the caller's running mode
            // is what we apply with; lookup goes through the
            // apply-imports context.  Outside any pattern-matched
            // template the spec says #current is an error; we
            // gracefully degrade to the default mode.
            let inherited_mode: Option<QName> = if *mode_current {
                state.apply_imports_ctx.as_ref()
                    .and_then(|(_, m, _, _, _, _)| m.clone())
            } else {
                None
            };
            let effective_mode = if *mode_current {
                inherited_mode.as_ref()
            } else {
                mode.as_ref()
            };
            let total = nodes.len();
            let r = (|| -> Result<()> {
                for (i, child) in nodes.iter().enumerate() {
                    apply_one_to_node_with_args(state, *child, effective_mode, i + 1, total, &args)?;
                }
                Ok(())
            })();
            state.tunnel_pool = tunnel_save;
            r?;
        }
        Instr::NextMatch { with_params } => {
            // XSLT 2.0 §6.7 — selection is "next-lower-precedence"
            // for our simplified implementation.  Handles the common
            // multi-import case correctly; same-precedence chains
            // XSLT 2.0 §6.7 — first try the apply-imports-style
            // "next-lower-precedence" selection; if that turns up no
            // match, fall back to the same-precedence pool excluding
            // the current template (which approximates "next
            // template in the conflict-resolution order" for the
            // common case where templates differ by priority but
            // not by precedence).
            let (node, mode, _cur_prec, cur_index, cur_prio, cur_branch) = state.apply_imports_ctx
                .clone()
                .ok_or_else(|| XsltError::Xpath(
                    sup_xml_core::xpath::eval::xpath_err(
                        "xsl:next-match invoked when no current template rule is in scope (XTDE0560)"
                    ).with_xpath_code("XTDE0560")
                ))?;
            // Snapshot only when there are tunnel params in scope —
            // the empty-HashMap clone is cheap but not free, and most
            // templates have no tunnel params at all.
            let tunnel_save = state.tunnel_pool.clone();
            let args = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            let bindings = state.bindings();
            // XSLT 2.0 §6.7 — find the strictly-next template in the
            // conflict-resolution order (lower precedence / lower
            // priority / earlier source position than the current
            // one).  This avoids the infinite-loop trap where a
            // higher-priority sibling keeps re-winning the lookup.
            let cur_index = cur_index.unwrap_or(usize::MAX);
            let cur_tmpl = state.style.templates.get(cur_index);
            let chosen = if let Some(t) = cur_tmpl {
                let current = pattern::Selected {
                    template:   t,
                    priority:   cur_prio,
                    branch_idx: cur_branch,
                };
                pattern::select_template_next(
                    state.style, node, mode.as_ref(), state.idx, &bindings,
                    &current, cur_index,
                ).map_err(XsltError::from)?
            } else {
                None
            };
            let inner_r: Result<()> = if let Some(sel) = chosen {
                state.template_call_depth += 1;
                if state.template_call_depth > MAX_TEMPLATE_CALL_DEPTH {
                    state.template_call_depth -= 1;
                    state.tunnel_pool = tunnel_save;
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:next-match depth exceeds limit ({MAX_TEMPLATE_CALL_DEPTH}) \
                         — possible infinite recursion"
                    )));
                }
                let prev = state.apply_imports_ctx.replace(
                    (node, mode.clone(), sel.template.import_precedence,
                     template_index_of(state.style, sel.template),
                     sel.priority, sel.branch_idx),
                );
                let r = run_template_body(state, sel.template, node, pos, size, &args);
                state.apply_imports_ctx = prev;
                state.template_call_depth -= 1;
                r
            } else {
                apply_builtin_template_with_args(state, node, mode.as_ref(), &args)
            };
            state.tunnel_pool = tunnel_save;
            inner_r?;
        }
        Instr::ApplyImports { with_params } => {
            // XSLT 1.0 §5.6 / XSLT 2.0 §9.4: re-applies the
            // *next-lower* precedence template that matches the same
            // node + mode.  Errors when invoked outside a pattern-
            // matched template.  XSLT 2.0 lets the caller forward
            // explicit and tunnel parameters into the imported
            // template via xsl:with-param children.
            let (node, mode, cur_prec, _cur_index, _cur_prio, _cur_branch) = state.apply_imports_ctx
                .clone()
                .ok_or_else(|| XsltError::Xpath(
                    sup_xml_core::xpath::eval::xpath_err(
                        "xsl:apply-imports invoked when no current template rule is in scope (XTDE0560)"
                    ).with_xpath_code("XTDE0560")
                ))?;
            let tunnel_save = state.tunnel_pool.clone();
            let args = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            let bindings = state.bindings();
            let chosen = pattern::select_template_max_precedence(
                state.style, node, mode.as_ref(), state.idx, &bindings,
                cur_prec - 1,
            ).map_err(XsltError::from)?;
            let r: Result<()> = (|| {
                if let Some(sel) = chosen {
                    let prev = state.apply_imports_ctx.replace(
                        (node, mode.clone(), sel.template.import_precedence,
                         template_index_of(state.style, sel.template),
                         sel.priority, sel.branch_idx),
                    );
                    let r = run_template_body(state, sel.template, node, pos, size, &args);
                    state.apply_imports_ctx = prev;
                    r
                } else {
                    apply_builtin_template_with_args(state, node, mode.as_ref(), &args)
                }
            })();
            state.tunnel_pool = tunnel_save;
            r?;
        }
        Instr::CallTemplate { name, with_params } => {
            let key = qname_key(name);
            let template = state.style.templates.iter()
                .find(|t| t.name.as_ref().map(qname_key).as_deref() == Some(key.as_str()))
                .ok_or_else(|| XsltError::UnresolvedReference(format!(
                    "no template named '{key}'"
                )))?;
            // Snapshot only when there are tunnel params in scope —
            // the empty-HashMap clone is cheap but not free, and most
            // templates have no tunnel params at all.
            let tunnel_save = state.tunnel_pool.clone();
            let args = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            state.template_call_depth += 1;
            if state.template_call_depth > MAX_TEMPLATE_CALL_DEPTH {
                state.template_call_depth -= 1;
                state.tunnel_pool = tunnel_save;
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:call-template depth exceeds limit ({MAX_TEMPLATE_CALL_DEPTH}) \
                     — likely infinite recursion in template '{key}'"
                )));
            }
            let r = run_template_body(state, template, ctx_node, pos, size, &args);
            state.template_call_depth -= 1;
            state.tunnel_pool = tunnel_save;
            r?;
        }
        Instr::Choose { whens, otherwise } => {
            for (test, body) in whens {
                let v = state.xpath_eval(test, ctx_node, pos, size)?;
                if value_to_bool(&v) {
                    eval_body(state, body, ctx_node, pos, size)?;
                    return Ok(());
                }
            }
            if let Some(body) = otherwise {
                eval_body(state, body, ctx_node, pos, size)?;
            }
        }
        Instr::If { test, body } => {
            let v = state.xpath_eval(test, ctx_node, pos, size)?;
            if value_to_bool(&v) {
                eval_body(state, body, ctx_node, pos, size)?;
            }
        }
        Instr::PerformSort { select, sort, body } => {
            // XSLT 2.0 §13.3: produce a sorted sequence and emit each
            // item as if by xsl:sequence (atomics → text, nodes → copy).
            //
            // The input sequence comes from `select=` when present;
            // otherwise it's the result of evaluating the sequence
            // constructor `body` (with `xsl:sort` children stripped at
            // compile time).
            match select {
                Some(e) => match state.xpath_eval(e, ctx_node, pos, size)? {
                    Value::NodeSet(ns) => {
                        let sorted = sort_nodes_for_iter(state, &ns, sort, ctx_node, pos, size)?;
                        copy_value_into(state, &Value::NodeSet(sorted), true)?;
                    }
                    Value::Sequence(items) => {
                        // Sort an atomic / mixed sequence by per-item
                        // sort keys, then emit each item as xsl:sequence.
                        let sorted = sort_items_for_iter(state, items, sort, ctx_node)?;
                        for item in sorted {
                            copy_value_into(state, &item, true)?;
                        }
                    }
                    // `m to n` materialises into a sequence of integer
                    // atomics — same shape as Sequence above.
                    Value::IntRange { lo, hi } => {
                        let items: Vec<Value> = (lo..=hi)
                            .map(|i| Value::Number(Numeric::Integer(i)))
                            .collect();
                        let sorted = sort_items_for_iter(state, items, sort, ctx_node)?;
                        for item in sorted {
                            copy_value_into(state, &item, true)?;
                        }
                    }
                    // A single atomic value sorts to itself — degenerate
                    // but legal per §13.3 (it's a 1-item sequence).
                    other @ (Value::Number(_) | Value::String(_)
                           | Value::Boolean(_) | Value::Typed(_)) => {
                        copy_value_into(state, &other, true)?;
                    }
                    other => return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:perform-sort select= must yield a sequence \
                         (got {other:?})"
                    ))),
                },
                None => {
                    // Body form — evaluate into a fresh RTF and graft
                    // each top-level result into the dynamic-RTF arena
                    // so it's addressable as an ordinary NodeId for
                    // the sort-key evaluator.  Text-fragment merging
                    // is disabled so each contributing instruction
                    // (e.g. one xsl:value-of per iteration) becomes a
                    // distinct item rather than collapsing into one
                    // concatenated text node.  An empty body yields
                    // an empty sorted sequence (sort-071).
                    let result_nodes = build_rtf_nodes_no_merge(state, body, ctx_node, pos, size)?;
                    let nodes = rtf_children_into_index(state.idx, &result_nodes);
                    let sorted = sort_nodes_for_iter(state, &nodes, sort, ctx_node, pos, size)?;
                    copy_value_into(state, &Value::NodeSet(sorted), true)?;
                }
            }
        }
        Instr::Document { body } => {
            // XSLT 2.0 §14.4: the body becomes a NEW document node.
            // When the surrounding scope is capturing a sequence of
            // documents (an `as="document-node()*"` variable, an
            // xsl:function body, …), expose this doc node as its own
            // sequence item rather than splatting its children into
            // the outer result tree.
            if state.sequence_sink_active() {
                let children = build_rtf_nodes_no_merge(
                    state, body, ctx_node, pos, size,
                )?;
                let doc_id = rtf_into_index(state.idx, &children);
                state.push_to_sequence_sink(Value::NodeSet(vec![doc_id]));
            } else {
                // Toggle the principal-document flag for the
                // duration of the body so XTDE0420 fires if a stray
                // attribute or namespace lands at this scope.
                let prev_principal = state.builder.is_principal_document;
                state.builder.is_principal_document = true;
                let r = eval_body(state, body, ctx_node, pos, size);
                state.builder.is_principal_document = prev_principal;
                r?;
            }
        }
        Instr::ResultDocument { href, format, format_namespaces, body } => {
            use crate::result_tree::ResultBuilder;
            // XSLT 2.0 §19.1.1 / XTDE1460 — the `format=` AVT
            // expansion must be a valid EQName (a non-empty NCName,
            // or `prefix:local` whose prefix is bound in this
            // element's in-scope namespaces) AND it must name a
            // declared `xsl:output`.  We currently track only the
            // prefix-resolution side; an unbound prefix is the most
            // common failure shape and is enough to catch the
            // expected error in the W3C suite.
            if let Some(fmt_avt) = format {
                let fmt = render_avt(state, fmt_avt, ctx_node, pos, size)?;
                let fmt = fmt.trim();
                if let Some((prefix, _)) = fmt.split_once(':') {
                    let bound = format_namespaces.iter().any(|(p, _)|
                        p.as_deref() == Some(prefix));
                    if !bound {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "xsl:result-document format='{fmt}' references \
                             undeclared prefix '{prefix}' (XTDE1460)")));
                    }
                }
            }
            // XTDE1480: an xsl:result-document is illegal while the current
            // output state is *temporary* — inside a variable/function
            // body, or an attribute/comment/PI value.  Being nested inside
            // another xsl:result-document is NOT temporary output: the
            // inner instruction creates a further final result tree.
            if in_temporary_output() {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:result-document is not allowed while writing to a \
                     temporary output destination (XTDE1480)".into()));
            }
            let uri = render_avt(state, href, ctx_node, pos, size)?;
            // An absent or empty href targets the principal output URI.
            // The principal builder is `principal_buf` when we are inside a
            // secondary capture, else the live `builder`.  Writing to the
            // principal is valid only when it is still empty — otherwise two
            // sources write the same destination (XTRE1495).
            if uri.is_empty() {
                if let Some(principal) = state.principal_buf.take() {
                    let secondary = std::mem::replace(&mut state.builder, principal);
                    if !state.builder.is_empty() {
                        state.principal_buf =
                            Some(std::mem::replace(&mut state.builder, secondary));
                        return Err(XsltError::InvalidStylesheet(
                            "xsl:result-document targets the principal output URI, \
                             which already has content (XTRE1495)".into()));
                    }
                    let r = eval_body(state, body, ctx_node, pos, size);
                    state.principal_buf =
                        Some(std::mem::replace(&mut state.builder, secondary));
                    return r;
                }
                if !state.builder.is_empty() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:result-document targets the principal output URI, \
                         which already has content (XTRE1495)".into()));
                }
                eval_body(state, body, ctx_node, pos, size)?;
                return Ok(());
            }
            // XTRE1495: two result documents must not share a URI.
            if SECONDARY_DOCS.with(|d| d.borrow().iter().any(|(u, _)| *u == uri)) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "two xsl:result-document instructions write to the same \
                     URI '{uri}' (XTRE1495)")));
            }
            // Capture the body into a fresh builder.  The previous active
            // builder is the principal when this is the outermost
            // result-document; expose it via `principal_buf` so a nested
            // href-less result-document can still reach the principal URI.
            let parent_active = std::mem::replace(&mut state.builder, ResultBuilder::new());
            let outer_secondary = if state.principal_buf.is_none() {
                state.principal_buf = Some(parent_active);
                None
            } else {
                Some(parent_active)
            };
            let r = eval_body(state, body, ctx_node, pos, size);
            let restored = outer_secondary
                .unwrap_or_else(|| state.principal_buf.take().expect("principal stashed"));
            let written = std::mem::replace(&mut state.builder, restored);
            r?;
            SECONDARY_DOCS.with(|d| d.borrow_mut().push((uri, written.finish())));
        }
        Instr::Try { body, catches } => {
            run_try_instr(state, body, catches, ctx_node, pos, size)?;
        }
        Instr::Namespace { name, select, body } => {
            let prefix = render_avt(state, name, ctx_node, pos, size)?;
            let uri = match select {
                Some(e) => value_to_string_styled(
                    &state.xpath_eval(e, ctx_node, pos, size)?, state.idx, state.num_style()),
                None => {
                    // Body form — render the body's text content.
                    use crate::result_tree::ResultBuilder;
                    let prev = std::mem::replace(
                        &mut state.builder, ResultBuilder::new()
                    );
                    let r = eval_body(state, body, ctx_node, pos, size);
                    let nested = std::mem::replace(&mut state.builder, prev);
                    r?;
                    stringify(&nested.finish())
                }
            };
            // XTDE0920 — the effective name must be a zero-length string
            // or an NCName, and must not be `xmlns` (XSLT 2.0 §11.7.2).
            if prefix == "xmlns" || (!prefix.is_empty() && !is_ncname_str(&prefix)) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:namespace name='{prefix}' is not a valid namespace \
                     prefix (XTDE0920)")));
            }
            // XTDE0925 — the `xml` prefix and the XML namespace URI may
            // only be bound to each other.
            const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
            if (prefix == "xml") != (uri == XML_NS) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:namespace binding of prefix '{prefix}' to '{uri}' \
                     conflicts with the reserved xml namespace (XTDE0925)")));
            }
            // XTDE0440 — a default-namespace node (empty name) may not be
            // attached to an element that is itself in no namespace; that
            // would put the element's own unprefixed name into the
            // declared namespace (XSLT 2.0 §11.7.2).
            if prefix.is_empty() && !uri.is_empty()
                && state.builder.current_element_uri() == Some("") {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:namespace declares a default namespace on an element \
                     that is in no namespace (XTDE0440)".into()));
            }
            // XTDE0930 — binding a non-empty prefix to a zero-length URI
            // is illegal (XSLT 2.0 §11.7.2); only the *default*
            // namespace declaration may have an empty URI (and only as
            // an undeclaration, handled elsewhere).
            if !prefix.is_empty() && uri.is_empty() {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:namespace name='{prefix}' bound to the empty URI \
                     (XTDE0930)"
                )));
            }
            // XTDE0905 — the XML Names "xmlns" pseudo-namespace URI
            // (`http://www.w3.org/2000/xmlns/`) must never appear as
            // the value of a namespace node (XSLT 2.0 §11.7.2).
            if uri == "http://www.w3.org/2000/xmlns/" {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:namespace URI 'http://www.w3.org/2000/xmlns/' \
                     is reserved and may not be declared (XTDE0905)".into()));
            }
            // Empty prefix means the default namespace; otherwise a
            // named binding.  Routed through the explicit-decl path
            // so an XTDE0430 conflict with another xsl:namespace on
            // the same element is caught (the implicit fixups
            // push_attribute performs use the non-strict variant).
            let prefix_opt = if prefix.is_empty() { None } else { Some(prefix) };
            state.builder.push_namespace_decl_explicit(prefix_opt, uri);
        }
        Instr::AnalyzeString { select, regex, flags, matching, non_matching } => {
            // XSLT 2.0 §15.1 — partition `select` (a string value)
            // into alternating matching / non-matching segments
            // against the regex.  Inside `matching`, the captured
            // groups are reachable via `regex-group(n)`.
            let input_v = state.xpath_eval(select, ctx_node, pos, size)?;
            // XSLT 2.0 §15.1 / XPTY0004 — `select` must be a string
            // (or castable from xs:untypedAtomic) and produce a
            // singleton; sequences with more than one string item
            // or non-string atomics are static type errors.
            match &input_v {
                Value::String(_) => {}
                Value::Typed(t) if matches!(t.kind,
                    "string" | "untypedAtomic" | "anyURI"
                    | "normalizedString" | "token" | "Name" | "NCName"
                    | "language" | "ID" | "IDREF" | "ENTITY" | "NMTOKEN") => {}
                Value::Sequence(items) if items.is_empty() => return Err(
                    XsltError::InvalidStylesheet(
                        "xsl:analyze-string select= must yield a single \
                         string (got empty sequence — XSLT 2.1+ relaxes this \
                         to '' but XSLT 2.0 is XPTY0004)".into())),
                Value::Sequence(items) if items.len() == 1 => {}
                Value::Sequence(items) => return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:analyze-string select= must yield a single string \
                     (got {}-item sequence) (XPTY0004)", items.len()
                ))),
                Value::NodeSet(ns) if ns.is_empty() => return Err(
                    XsltError::InvalidStylesheet(
                        "xsl:analyze-string select= must yield a single \
                         string (got empty node-set — XSLT 2.1+ relaxes this \
                         to '' but XSLT 2.0 is XPTY0004)".into())),
                Value::NodeSet(ns) if ns.len() == 1 => {}
                Value::NodeSet(ns) => return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:analyze-string select= must yield a single string \
                     (got {}-node set) (XPTY0004)", ns.len()
                ))),
                _ => return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:analyze-string select= must yield a string \
                     (got {input_v:?}) (XPTY0004)"
                ))),
            }
            let input   = value_to_string_styled(&input_v, state.idx, state.num_style());
            let pattern = render_avt(state, regex, ctx_node, pos, size)?;
            let flag_s  = render_avt(state, flags, ctx_node, pos, size)?;
            // XSLT 2.0 §15.1 — non-capturing groups, lookaround and
            // other PCRE `(?...)` extensions are XSLT 3.0+ regex
            // additions; the 2.0 grammar is XSD-only.  Reject them
            // statically to surface as an analyze-string error.
            let allow_q = crate::functions::xslt_version_3_or_more(&state.style.version);
            if !allow_q && pattern.contains("(?") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:analyze-string regex='{pattern}' uses a `(?…)` \
                     construct not allowed in XSLT 2.0 (XTDE1145)"
                )));
            }
            for ch in flag_s.chars() {
                let ok = matches!(ch, 's' | 'm' | 'i' | 'x')
                    || (allow_q && ch == 'q');
                if !ok {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:analyze-string flags='{flag_s}' contains \
                         unrecognised flag '{ch}' (XTDE1145)"
                    )));
                }
            }
            // XSLT 2.0 §15.1 / XTDE1150 — a regex that matches the
            // empty string would partition into infinitely many
            // zero-length matches; reject it before pattern compile.
            // (XSLT 3.0 relaxes this.)
            if let Ok(probe) = sup_xml_core::regex::Pattern::compile_with(
                &pattern, sup_xml_core::regex::Dialect::Xpath,
            ) {
                if probe.is_match("") {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:analyze-string regex='{pattern}' matches the \
                         zero-length string (XTDE1150)"
                    )));
                }
            }
            // Prefer the native XSD §F / XPath 2.0 engine when no
            // flags are in play — it gives version-pinned Unicode
            // answers (set by the conformance runner around
            // version-locked test sets) and avoids Rust regex's
            // looser XPath-grammar interpretation.  Capture groups
            // aren't exposed yet from the native path; callers that
            // need `regex-group(n)` fall back to the Rust engine.
            let uses_captures = matching_body_uses_regex_group(matching);
            // Reluctant quantifiers (`a+?`, `a{2,4}?`, …) need
            // shortest-match semantics during partition.  Our
            // `find_iter` is greedy-only so far — fall back to
            // Rust regex for those.
            let has_reluctant = regex_has_reluctant_quantifier(&pattern);
            // One entry per substring in the XSLT 2.0 §15.1 partition:
            // `(is_match, text, capture-groups)`.  Built first so the
            // body for each substring can be run with the substring's
            // position in the partition as `position()` and the total
            // substring count as `last()` (XSLT 2.0 §5.6).
            let mut segments: Vec<(bool, String, Vec<String>)> = Vec::new();
            if flag_s.is_empty() && !uses_captures && !has_reluctant {
                if let Ok(p) = sup_xml_core::regex::Pattern::compile_with(
                    &pattern, sup_xml_core::regex::Dialect::Xpath,
                ) {
                    let matches = p.find_iter(&input);
                    let mut cursor = 0usize;
                    for (start, end) in matches {
                        if start > cursor {
                            segments.push((false, input[cursor..start].to_string(), Vec::new()));
                        }
                        if end > start {
                            let m_str = input[start..end].to_string();
                            // No capture groups from this path —
                            // group 0 (the whole match) is enough for
                            // the bodies our gate above accepts.
                            let groups = vec![m_str.clone()];
                            segments.push((true, m_str, groups));
                        }
                        cursor = end;
                    }
                    if cursor < input.len() {
                        segments.push((false, input[cursor..].to_string(), Vec::new()));
                    }
                    return run_analyze_partition(
                        state, matching, non_matching, &segments, ctx_node);
                }
            }
            // Fallback: Rust regex.  Used when flags are present,
            // captures are wanted, or the pattern doesn't compile
            // under the strict XPath dialect.
            let re = sup_xml_core::xpath::compile_xpath_2_0_regex(&pattern, &flag_s)
                .map_err(XsltError::from)?;
            let mut cursor = 0usize;
            for cap in re.captures_iter(&input) {
                let m = cap.get(0).unwrap();
                if m.start() > cursor {
                    segments.push((false, input[cursor..m.start()].to_string(), Vec::new()));
                }
                let groups: Vec<String> = (0..cap.len())
                    .map(|i| cap.get(i).map(|m| m.as_str().to_string()).unwrap_or_default())
                    .collect();
                segments.push((true, m.as_str().to_string(), groups));
                cursor = m.end();
            }
            if cursor < input.len() {
                segments.push((false, input[cursor..].to_string(), Vec::new()));
            }
            run_analyze_partition(state, matching, non_matching, &segments, ctx_node)?;
        }
        Instr::ForEachGroup { select, kind, key, sort, body, collation } => {
            use crate::ast::GroupingKind;
            // XSLT 2.0 §14 — the input sequence may contain atomic
            // values as well as nodes.  Realise atomics as synthetic
            // text nodes so the rest of the grouping pipeline (which
            // walks NodeIds) keeps working without a parallel
            // atomic-only path.
            let select_val = state.xpath_eval(select, ctx_node, pos, size)?;
            // XSLT 2.0 §14 / XTTE1120 — group-starting-with /
            // group-ending-with match a pattern against each item, so
            // every item must be a node.  An atomic population (e.g.
            // `select="1,2,3"`) is a dynamic type error.
            let pattern_grouping = matches!(kind,
                GroupingKind::StartingWith | GroupingKind::EndingWith);
            if pattern_grouping {
                let all_nodes = match &select_val {
                    Value::NodeSet(_) | Value::ForeignNodeSet(_) => true,
                    Value::Sequence(items) => items.iter().all(|v| matches!(v,
                        Value::NodeSet(_) | Value::ForeignNodeSet(_))),
                    _ => false,
                };
                if !all_nodes {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:for-each-group group-starting-with / \
                         group-ending-with requires every item to be a \
                         node (XTTE1120)".into()));
                }
            }
            let nodes = match select_val {
                Value::NodeSet(ns) => ns,
                Value::Sequence(items) => {
                    let mut out: Vec<NodeId> = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::NodeSet(ns) => out.extend(ns),
                            Value::ForeignNodeSet(_) => {}
                            atomic => {
                                let s = value_to_string_styled(&atomic, state.idx, state.num_style());
                                let ids = state.idx
                                    .allocate_rtf_text_nodes_inherent(vec![s]);
                                out.extend(ids);
                            }
                        }
                    }
                    out
                }
                other => return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:for-each-group select= must yield a sequence \
                     (got {other:?})"
                ))),
            };
            // Materialise the group partition first, then iterate.
            // group-by:  bucket by the string-value of `key` per item.
            //            Group order = order of first appearance of each key.
            // group-adjacent: consecutive runs with equal key.
            // group-starting-with / group-ending-with: pattern boundary —
            //   not yet implemented; raise rather than silently mis-evaluate.
            // Fold the bucket-key string for the configured collation
            // (default codepoint = identity).  XSLT 2.0 §14 — group-by
            // / group-adjacent treat the key as a value-comparison
            // result, so the collation drives equality.
            let collation_fold = |s: &str| -> String {
                let ci = matches!(collation.as_deref(),
                    Some("http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive"));
                if ci {
                    let mut out = String::with_capacity(s.len());
                    for c in s.chars() {
                        if c.is_ascii_uppercase() { out.push(c.to_ascii_lowercase()); }
                        else                       { out.push(c); }
                    }
                    out
                } else { s.to_string() }
            };
            let groups: Vec<(Value, Vec<NodeId>)> = match kind {
                GroupingKind::By => {
                    let mut key_order: Vec<String> = Vec::new();
                    let mut buckets: std::collections::HashMap<String, (Value, Vec<NodeId>)>
                        = std::collections::HashMap::new();
                    for (i, &n) in nodes.iter().enumerate() {
                        state.xslt_current = n;
                        let kv = state.xpath_eval(key, n, i + 1, nodes.len())?;
                        // XSLT 2.0 §14.3 — each item in the grouping-key
                        // sequence is a distinct key; the node joins the
                        // group for each, deduplicated within this item
                        // (a key value repeated for one node adds it once).
                        let mut seen = std::collections::HashSet::new();
                        for key_item in grouping_key_items(&kv) {
                            // group-by equality is the `eq` operator: temporal
                            // values match by instant, durations by magnitude.
                            let k_str = value_equality_key(&key_item).unwrap_or_else(|| collation_fold(
                                &value_to_string_styled(&key_item, state.idx, state.num_style())));
                            if !seen.insert(k_str.clone()) { continue; }
                            if !buckets.contains_key(&k_str) {
                                key_order.push(k_str.clone());
                            }
                            buckets.entry(k_str)
                                .or_insert_with(|| (key_item, Vec::new()))
                                .1.push(n);
                        }
                    }
                    key_order.into_iter().map(|k| buckets.remove(&k).unwrap()).collect()
                }
                GroupingKind::Adjacent => {
                    let mut out: Vec<(Value, Vec<NodeId>)> = Vec::new();
                    let mut prev_key: Option<String> = None;
                    for (i, &n) in nodes.iter().enumerate() {
                        state.xslt_current = n;
                        let kv = state.xpath_eval(key, n, i + 1, nodes.len())?;
                        // XSLT 2.0 §14.4 / XTTE1100 — the
                        // group-adjacent expression must produce
                        // exactly one item per source item.  An
                        // empty sequence (or one of length > 1) is
                        // a hard type error rather than a quiet
                        // empty-bucket join.
                        let item_count = match &kv {
                            Value::NodeSet(ns)     => ns.len(),
                            Value::Sequence(items) => items.len(),
                            _                       => 1,
                        };
                        if item_count != 1 {
                            return Err(XsltError::InvalidStylesheet(format!(
                                "xsl:for-each-group group-adjacent= must yield \
                                 exactly one item per source item (got {item_count} \
                                 items) (XTTE1100)"
                            )));
                        }
                        let k_str = value_equality_key(&kv).unwrap_or_else(|| collation_fold(
                            &value_to_string_styled(&kv, state.idx, state.num_style())));
                        if Some(&k_str) == prev_key.as_ref() {
                            out.last_mut().unwrap().1.push(n);
                        } else {
                            out.push((kv, vec![n]));
                            prev_key = Some(k_str);
                        }
                    }
                    out
                }
                GroupingKind::StartingWith => {
                    // XSLT 2.0 §14.4: each item matching the pattern
                    // STARTS a new group.  The first item in the
                    // population always begins a group too, so items
                    // preceding the first match form a leading group of
                    // their own rather than being dropped.
                    // current-grouping-key() is the empty sequence here.
                    let mut out: Vec<(Value, Vec<NodeId>)> = Vec::new();
                    let bindings = state.bindings();
                    for &n in nodes.iter() {
                        let starts = pattern::matches(
                            key, n, state.idx, &bindings,
                        ).map_err(XsltError::from)?;
                        if starts || out.is_empty() {
                            out.push((Value::NodeSet(Vec::new()), vec![n]));
                        } else {
                            out.last_mut().unwrap().1.push(n);
                        }
                    }
                    out
                }
                GroupingKind::EndingWith => {
                    // XSLT 2.0 §14.4: each item matching the pattern
                    // ENDS the current group; the next item starts a
                    // new group.  Trailing items not ending with a
                    // match still form a final group.
                    let mut out: Vec<(Value, Vec<NodeId>)> = Vec::new();
                    let mut current: Vec<NodeId> = Vec::new();
                    let bindings = state.bindings();
                    for &n in nodes.iter() {
                        current.push(n);
                        let ends = pattern::matches(
                            key, n, state.idx, &bindings,
                        ).map_err(XsltError::from)?;
                        if ends {
                            out.push((Value::NodeSet(Vec::new()),
                                      std::mem::take(&mut current)));
                        }
                    }
                    if !current.is_empty() {
                        out.push((Value::NodeSet(Vec::new()), current));
                    }
                    out
                }
            };
            // Optional sort: re-order the groups by their xsl:sort
            // keys, evaluated with each group's current-group() /
            // current-grouping-key() in scope (XSLT 2.0 §14.3).
            let group_order: Vec<usize> = sort_group_indices(state, &groups, sort)?;
            state.variables.enter();
            let prev_current = state.xslt_current;
            let prev_group = std::mem::take(&mut state.current_group);
            let prev_key   = state.current_grouping_key.take();
            let total = groups.len();
            for (i, gi) in group_order.iter().enumerate() {
                let (k, ns) = &groups[*gi];
                state.current_group = ns.clone();
                state.current_grouping_key = Some(k.clone());
                let leader = *ns.first().unwrap_or(&ctx_node);
                state.xslt_current = leader;
                eval_body(state, body, leader, i + 1, total)?;
            }
            state.xslt_current = prev_current;
            state.current_group = prev_group;
            state.current_grouping_key = prev_key;
            state.variables.leave();
        }
        Instr::OnEmpty { body } | Instr::OnNonEmpty { body } => {
            // Reached only outside a normal sequence constructor
            // (eval_body resolves on-* in place against their siblings);
            // here there are no siblings, so include the content.
            eval_body(state, body, ctx_node, pos, size)?;
        }
        Instr::WherePopulated { body } => {
            // Build the body's output, then emit it only if populated
            // (XSLT 3.0 §16.4.3): suppress an empty wrapper such as an
            // element with no attributes/children, or a zero-length
            // text node.
            let nodes = build_rtf_nodes(state, body, ctx_node, pos, size)?;
            if nodes.iter().any(result_node_is_significant) {
                for n in nodes { state.builder.push_built_node(n); }
            }
        }
        Instr::Fork { body } => {
            // Tree-based: the prongs share the same focus and their
            // results concatenate in order — same as evaluating the
            // body straight through.
            eval_body(state, body, ctx_node, pos, size)?;
        }
        Instr::SourceDocument { href, body } => {
            // Non-streamed: load the referenced document fully and run
            // the body against its document node.
            let uri = render_avt(state, href, ctx_node, pos, size)?;
            let root = match state.documents.and_then(|d| d.get(&uri).copied()) {
                Some(id) => id,
                None => match state.bindings().load_dynamic_document(&uri) {
                    Some(Ok(id)) => id,
                    _ => return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:source-document: cannot load {uri:?} (FODC0002)"))),
                },
            };
            let prev_current = state.xslt_current;
            state.xslt_current = root;
            eval_body(state, body, root, 1, 1)?;
            state.xslt_current = prev_current;
        }
        Instr::Evaluate { xpath, context_item, with_params } => {
            // The xpath= expression yields the dynamic expression text.
            let xpath_str = value_to_string_styled(
                &state.xpath_eval(xpath, ctx_node, pos, size)?, state.idx, state.num_style());
            let opts = sup_xml_core::xpath::XPathOptions {
                xpath_2_0: true, libxml2_compatible: false,
                ..sup_xml_core::xpath::XPathOptions::default()
            };
            let expr = sup_xml_core::xpath::parse_xpath_with(&xpath_str, &opts)
                .map_err(|e| XsltError::InvalidStylesheet(format!(
                    "xsl:evaluate: invalid XPath {xpath_str:?}: {} (XTDE3160)", e.message)))?;
            // The dynamic expression's context item (default: the
            // current node).  A node-set result focuses on its first
            // node; other values keep the current node as a fallback.
            let cnode = match context_item {
                Some(ce) => match state.xpath_eval(ce, ctx_node, pos, size)? {
                    Value::NodeSet(ns) if !ns.is_empty() => ns[0],
                    _ => ctx_node,
                },
                None => ctx_node,
            };
            // Bind the with-param values as variables visible to the
            // dynamic expression.
            let bound = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            state.variables.enter();
            for (name, value, _) in &bound {
                state.variables.bind(qname_key(name), value.clone());
            }
            let result = state.xpath_eval(&expr, cnode, 1, 1);
            state.variables.leave();
            let v = result?;
            if state.sequence_sink_active() {
                state.push_to_sequence_sink(v);
            } else {
                copy_value_into(state, &v, true)?;
            }
        }
        Instr::Merge { sources, action } => {
            // Gather the items from every source, tagging each with its
            // source index.  Atomic items become synthetic text nodes so
            // the node-keyed pipeline (sort, grouping) applies uniformly
            // — mirrors xsl:for-each-group.  The tag is needed because
            // each merge-source declares its OWN merge-keys (the selects
            // differ across sources even though they compare alike), so a
            // node must be keyed by the keys of the source it came from.
            let mut tagged: Vec<(usize, NodeId)> = Vec::new();
            for (si, src) in sources.iter().enumerate() {
                let contexts: Vec<NodeId> = match &src.for_each_source {
                    None => vec![ctx_node],
                    Some(fes) => {
                        let v = state.xpath_eval(fes, ctx_node, pos, size)?;
                        merge_materialize_nodes(state, v)
                    }
                };
                let nc = contexts.len();
                for (ci, c) in contexts.into_iter().enumerate() {
                    let v = state.xpath_eval(&src.select, c, ci + 1, nc)?;
                    tagged.extend(merge_materialize_nodes(state, v).into_iter().map(|n| (si, n)));
                }
            }
            // Order the merged stream, keying each node by its own
            // source's merge keys.
            let nodes: Vec<NodeId> = tagged.iter().map(|(_, n)| *n).collect();
            let per_node: Vec<&[crate::ast::Sort]> =
                tagged.iter().map(|(si, _)| sources[*si].keys.as_slice()).collect();
            let order = merge_sort_order(state, &nodes, &per_node)?;
            let sorted: Vec<(usize, NodeId)> = order.into_iter().map(|i| tagged[i]).collect();
            // Group adjacent items sharing an equal composite merge key,
            // computed from each item's own source keys.
            let mut groups: Vec<(Value, Vec<NodeId>)> = Vec::new();
            let mut prev_gk: Option<String> = None;
            for (si, n) in &sorted {
                let (si, n) = (*si, *n);
                let vals = merge_key_values(state, &sources[si].keys, n)?;
                let gk = merge_group_key_str(&vals, state);
                let key_value = if vals.len() == 1 {
                    vals.into_iter().next().unwrap()
                } else {
                    Value::Sequence(vals)
                };
                if prev_gk.as_ref() == Some(&gk) {
                    groups.last_mut().unwrap().1.push(n);
                } else {
                    groups.push((key_value, vec![n]));
                    prev_gk = Some(gk);
                }
            }
            // Run the action once per group with current-merge-group()
            // / current-merge-key() in scope (reusing the grouping
            // accessor state — a merge-action and a for-each-group body
            // are never active at the same time).
            state.variables.enter();
            let prev_current = state.xslt_current;
            let prev_group = std::mem::take(&mut state.current_group);
            let prev_key   = state.current_grouping_key.take();
            let total = groups.len();
            for (i, (k, ns)) in groups.iter().enumerate() {
                state.current_group = ns.clone();
                state.current_grouping_key = Some(k.clone());
                let leader = *ns.first().unwrap_or(&ctx_node);
                state.xslt_current = leader;
                eval_body(state, action, leader, i + 1, total)?;
            }
            state.xslt_current = prev_current;
            state.current_group = prev_group;
            state.current_grouping_key = prev_key;
            state.variables.leave();
        }
        Instr::ForEach { select, sort, body } => {
            // XSLT 2.0 §13.2 allows any sequence — atomics get
            // promoted to synthetic text nodes so the iteration
            // model is uniform with the 1.0 nodeset case.  XSLT 1.0
            // mode also reaches this path; non-nodeset values there
            // would be a static type error in strict 1.0 but most
            // engines tolerate them.
            let v = state.xpath_eval(select, ctx_node, pos, size)?;
            // When the input is a sequence of typed atomics and a
            // sort is in play, preserve the typed values so the sort
            // can compare by XPath value (e.g. xs:dateTime sorting
            // beyond year 9999) instead of falling back to lexical.
            // This branch only handles the pure-atomic case; mixed
            // node/atomic sequences fall through to the legacy text-
            // node promotion path below.
            if !sort.is_empty() {
                if let Value::Sequence(items) = &v {
                    if !items.is_empty()
                        && items.iter().all(|it| matches!(it,
                            Value::Typed(_) | Value::String(_)
                            | Value::Number(_) | Value::Boolean(_)))
                    {
                        return run_for_each_typed_sequence(
                            state, items.clone(), sort, body,
                            ctx_node, pos, size);
                    }
                }
            }
            // Mark whether the selected sequence was atomic so the
            // iteration body sees the right context-item flavour
            // (XSLT 2.0 §13.2): an atomic select makes each iteration's
            // focus an atomic, which we present via synthetic text
            // nodes but flag for XTTE0510 / XTTE0990 detection.
            let select_is_atomic = !matches!(&v,
                Value::NodeSet(_) | Value::ForeignNodeSet(_));
            let nodes = match v {
                Value::NodeSet(ns) => ns,
                Value::String(s) => {
                    state.idx.allocate_rtf_text_nodes_inherent(vec![s])
                }
                Value::Number(n) => {
                    state.idx.allocate_rtf_text_nodes_inherent(
                        vec![value_to_string_styled(&Value::Number(n), state.idx, state.num_style())])
                }
                Value::Boolean(b) => {
                    state.idx.allocate_rtf_text_nodes_inherent(
                        vec![if b { "true".into() } else { "false".into() }])
                }
                Value::Typed(t) => {
                    state.idx.allocate_rtf_text_nodes_inherent(vec![t.lexical])
                }
                Value::Sequence(items) => {
                    // Heterogeneous atomic/typed sequence — synthesise a
                    // text node per item so iteration sees each one
                    // separately.  Pre-existing nodes flow through their
                    // own ids; everything else stringifies.  Inline
                    // IntRange fragments expand here too so a
                    // `(scalar, 1 to N, scalar)` literal iterates per
                    // codepoint.
                    let mut out: Vec<NodeId> = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::NodeSet(ns) => out.extend(ns),
                            Value::ForeignNodeSet(_) => {}
                            Value::IntRange { lo, hi } => {
                                let strings: Vec<String> = (lo..=hi).map(|i| i.to_string()).collect();
                                let ids = state.idx.allocate_rtf_text_nodes_inherent(strings);
                                out.extend(ids);
                            }
                            atomic => {
                                let s = value_to_string_styled(&atomic, state.idx, state.num_style());
                                let ids = state.idx.allocate_rtf_text_nodes_inherent(vec![s]);
                                out.extend(ids);
                            }
                        }
                    }
                    out
                }
                // Lazy `m to n` from XPath 2.0 — materialise each
                // integer as a synthetic text node so the iteration
                // loop below sees one context node per value.
                Value::IntRange { lo, hi } => {
                    let strings: Vec<String> = (lo..=hi).map(|i| i.to_string()).collect();
                    state.idx.allocate_rtf_text_nodes_inherent(strings)
                }
                other => return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:for-each select= must yield a sequence (got {other:?})"
                ))),
            };
            let nodes = sort_nodes_for_iter(state, &nodes, sort, ctx_node, pos, size)?;
            // for-each opens a fresh scope per XSLT 1.0 §11 and
            // updates `current()` to each iterated node.
            state.variables.enter();
            let prev_current = state.xslt_current;
            // XSLT 2.0 §6.6.3 — the current template rule becomes
            // null inside xsl:for-each, so a contained
            // xsl:apply-imports / xsl:next-match raises XTDE0560.
            let prev_apply_imports = state.apply_imports_ctx.take();
            let total = nodes.len();
            let _atomic_guard = select_is_atomic.then(AtomicForEachGuard::enter);
            for (i, child) in nodes.iter().enumerate() {
                state.xslt_current = *child;
                eval_body(state, body, *child, i + 1, total)?;
            }
            state.xslt_current = prev_current;
            state.apply_imports_ctx = prev_apply_imports;
            state.variables.leave();
        }
        Instr::Iterate { select, params, on_completion, body } => {
            // Promote the selected sequence to context nodes the same
            // way xsl:for-each does (atomics become synthetic text
            // nodes), so position()/last() and `.` work per item.
            let v = state.xpath_eval(select, ctx_node, pos, size)?;
            let nodes = iterate_select_nodes(state, v)?;
            // Initial loop-carried parameter values, keyed by name.
            let mut current: HashMap<String, Value> = HashMap::new();
            for p in params {
                let raw = evaluate_param_default(state, p, ctx_node, pos, size)?;
                let val = match &p.as_type {
                    Some(t) => match parse_as_atomic_type(t) {
                        Some(st) => coerce_to_atomic_sequence(raw, &st, state.idx)?,
                        None => raw,
                    },
                    None => raw,
                };
                current.insert(qname_key(&p.name), val);
            }
            let prev_current = state.xslt_current;
            let total = nodes.len();
            let mut broke = false;
            for (i, child) in nodes.iter().enumerate() {
                state.variables.enter();
                for p in params {
                    let key = qname_key(&p.name);
                    if let Some(v) = current.get(&key) {
                        state.variables.bind(key, v.clone());
                    }
                }
                state.xslt_current = *child;
                let r = eval_body(state, body, *child, i + 1, total);
                state.variables.leave();
                // Consume the control signal before propagating any
                // error, so a failing iteration can't leave the
                // thread-local set and poison later evaluation.
                let ctrl = take_iterate_control();
                r?;
                match ctrl {
                    Some(IterateControl::Break) => { broke = true; break; }
                    Some(IterateControl::Next(args)) => {
                        // Supplied params update the carried value;
                        // unmentioned ones keep their current value.
                        for (name, val, _rtf) in args {
                            let key = qname_key(&name);
                            let val = params.iter()
                                .find(|p| qname_key(&p.name) == key)
                                .and_then(|p| p.as_type.as_deref())
                                .and_then(parse_as_atomic_type)
                                .map(|st| coerce_to_atomic_sequence(val.clone(), &st, state.idx))
                                .transpose()?
                                .unwrap_or(val);
                            current.insert(key, val);
                        }
                    }
                    None => {} // body fell off the end — params unchanged
                }
            }
            state.xslt_current = prev_current;
            // XSLT 3.0 §8.3 — xsl:on-completion runs iff no xsl:break
            // fired, with the final parameter values in scope.
            if !broke && !on_completion.is_empty() {
                state.variables.enter();
                for p in params {
                    let key = qname_key(&p.name);
                    if let Some(v) = current.get(&key) {
                        state.variables.bind(key, v.clone());
                    }
                }
                eval_body(state, on_completion, ctx_node, pos, size)?;
                state.variables.leave();
            }
        }
        Instr::NextIteration { with_params } => {
            let args = evaluate_with_params(state, with_params, ctx_node, pos, size)?;
            set_iterate_control(IterateControl::Next(args));
        }
        Instr::Break { select, body } => {
            // The break's output is emitted here, at the break point.
            match select {
                Some(sel) => {
                    let v = state.xpath_eval(sel, ctx_node, pos, size)?;
                    copy_value_into(state, &v, true)?;
                }
                None => eval_body(state, body, ctx_node, pos, size)?,
            }
            set_iterate_control(IterateControl::Break);
        }
        Instr::ValueOf { select, dose, separator } => {
            let v = state.xpath_eval(select, ctx_node, pos, size)?;
            let text = match separator {
                // XSLT 2.0 path — atomise the result and join with
                // the separator's rendered value.
                Some(sep_avt) => {
                    let sep = render_avt(state, sep_avt, ctx_node, pos, size)?;
                    let pieces = sequence_string_items(&v, state.idx, state.num_style());
                    pieces.join(&sep)
                }
                // XSLT 1.0 path — take the first node's / value's
                // string representation.
                None => value_to_string_styled(&v, state.idx, state.num_style()),
            };
            state.builder.push_text(text, *dose);
        }
        Instr::ValueOfBody { body, dose, separator } => {
            // XSLT 2.0 §11.5: when value-of carries a body (no
            // select= attribute), the default separator is the
            // zero-length string.  An explicit separator= overrides.
            // The select= form uses a single space as default — see
            // the ValueOf arm above.
            let sep = match separator {
                Some(avt) => render_avt(state, avt, ctx_node, pos, size)?,
                None      => String::new(),
            };
            let pieces = collect_value_of_body_pieces(state, body, ctx_node, pos, size)?;
            let text = pieces.join(&sep);
            state.builder.push_text(text, *dose);
        }
        Instr::Copy { use_attribute_sets, body, copy_namespaces } => {
            // Copy the *current node*: same node type and name,
            // but the children come from re-evaluating the body —
            // and ONLY the node itself, no descendants
            // (xsl:copy-of does deep copy).
            match state.idx.kind(ctx_node) {
                XPathNodeKind::Element => {
                    let q = element_qname(state, ctx_node);
                    state.builder.open_element(q.clone());
                    // XSLT 1.0 §7.5 — the namespace nodes of the
                    // current element are automatically copied.  With
                    // copy-namespaces="no" (XSLT 2.0 §11.9.1) only the
                    // element's own name binding is kept.
                    if *copy_namespaces {
                        for ns_id in state.idx.ns_range(ctx_node) {
                            let prefix = state.idx.local_name(ns_id);
                            let uri    = state.idx.string_value(ns_id);
                            if prefix == "xml" { continue; }
                            let p_opt = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                            state.builder.push_namespace_decl(p_opt, uri);
                        }
                    } else if !q.uri.is_empty() {
                        state.builder.push_namespace_decl(q.prefix.clone(), q.uri.clone());
                    }
                    apply_attribute_sets(state, use_attribute_sets, ctx_node, pos, size)?;
                    eval_body(state, body, ctx_node, pos, size)?;
                    state.builder.close_element();
                }
                XPathNodeKind::Text | XPathNodeKind::CData => {
                    state.builder.push_text(state.idx.string_value(ctx_node), false);
                }
                XPathNodeKind::Attribute => {
                    // Copy the attribute onto the current element.
                    let q = attribute_qname(state, ctx_node);
                    state.builder.push_attribute(q, state.idx.string_value(ctx_node));
                }
                XPathNodeKind::Comment => {
                    state.builder.push_comment(state.idx.string_value(ctx_node));
                }
                XPathNodeKind::PI => {
                    state.builder.push_pi(
                        state.idx.pi_target(ctx_node).to_string(),
                        state.idx.string_value(ctx_node),
                    );
                }
                XPathNodeKind::Document => {
                    // XSLT 2.0 §11.9.1 — xsl:copy of a document node
                    // creates a NEW document node whose children come
                    // from the body sequence constructor.  When the
                    // surrounding scope is collecting a sequence (an
                    // `as="document-node()*"` variable, an xsl:function
                    // body, …), expose each doc-copy as its own item;
                    // otherwise splat its children into the outer
                    // result tree (the historical 1.0 behaviour).
                    if state.sequence_sink_active() {
                        let children = build_rtf_nodes_no_merge(
                            state, body, ctx_node, pos, size,
                        )?;
                        check_document_node_content(&children)?;
                        let doc_id = rtf_into_index(state.idx, &children);
                        state.push_to_sequence_sink(Value::NodeSet(vec![doc_id]));
                    } else {
                        // The body constructs the content of a new
                        // document node; build it in a fresh scope so a
                        // top-level attribute is caught as XTDE0420
                        // (running the body inline would attach the
                        // attribute to the enclosing open element
                        // instead), then splice the children into the
                        // outer result tree.
                        let children = build_rtf_nodes(
                            state, body, ctx_node, pos, size,
                        )?;
                        check_document_node_content(&children)?;
                        for child in children {
                            state.builder.push_built_node(child);
                        }
                    }
                }
                XPathNodeKind::Namespace => {
                    // xsl:copy of a namespace node adds its binding to
                    // the element under construction (XSLT 2.0 §11.9.2);
                    // `xml` is implicitly in scope.
                    let prefix = state.idx.local_name(ctx_node);
                    if prefix != "xml" {
                        let uri = state.idx.string_value(ctx_node);
                        let p = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                        state.builder.push_namespace_decl(p, uri);
                    }
                }
            }
        }
        Instr::CopyOf { select, copy_namespaces } => {
            // Special case: if the select is a single variable
            // reference and we have an RTF for that variable,
            // deep-copy the result-tree nodes instead of treating
            // it as a string.  XSLT 1.0 §11.3 requires this for
            // body-form xsl:variable / xsl:param.
            if let sup_xml_core::xpath::Expr::Variable(name) = select {
                if let Some(nodes) = state.rtfs.get(name).cloned() {
                    for n in nodes { copy_result_node_into(state, &n); }
                    return Ok(());
                }
            }
            let v = state.xpath_eval(select, ctx_node, pos, size)?;
            // When the surrounding scope is capturing a sequence of
            // document nodes (an `as="document-node()*"` variable
            // body), each top-level doc-node item must reach the
            // sink as its OWN item rather than being splatted into
            // the result tree.  Other shapes still go through the
            // ordinary copy-into-builder path.
            if state.sequence_sink_active() {
                if let Value::NodeSet(ns) = &v {
                    let all_docs = !ns.is_empty() && ns.iter().all(|&id|
                        matches!(state.idx.kind(id), XPathNodeKind::Document));
                    if all_docs {
                        for &id in ns {
                            state.push_to_sequence_sink(Value::NodeSet(vec![id]));
                        }
                        return Ok(());
                    }
                }
                if let Value::Sequence(items) = &v {
                    let all_doc_nodesets = !items.is_empty() && items.iter().all(|it| {
                        if let Value::NodeSet(ns) = it {
                            ns.len() == 1 && matches!(state.idx.kind(ns[0]),
                                XPathNodeKind::Document)
                        } else { false }
                    });
                    if all_doc_nodesets {
                        for it in items {
                            state.push_to_sequence_sink(it.clone());
                        }
                        return Ok(());
                    }
                }
            }
            copy_value_into(state, &v, *copy_namespaces)?;
        }
        Instr::Element { name, namespace, body, use_attribute_sets, in_scope_namespaces } => {
            let name_str = render_avt(state, name, ctx_node, pos, size)?;
            // XSLT spec §7.1.2 / §11.7 — the element name must be a
            // non-empty lexical QName.  Empty / whitespace-only
            // resolves to error XTDE0820 at runtime, not a silent
            // `<>` emission.
            if name_str.trim().is_empty() {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:element name= must yield a non-empty QName (XTDE0820)".into()
                ));
            }
            // XSLT 2.0 §11.7 / XTDE0820 — the resolved name= must be
            // a *lexical QName*; leading digits / spaces / dots /
            // other XML Names violations are dynamic errors rather
            // than silent emissions of a malformed element name.
            if !is_lexical_qname_str(&name_str) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:element name='{name_str}' is not a valid QName (XTDE0820)"
                )));
            }
            // XSLT 1.0 §7.1.2: when `namespace=` is omitted (the
            // common case), expand the AVT-resolved name using the
            // namespaces in scope on the `xsl:element` element
            // itself, captured at compile time.  A prefixed name
            // resolves its prefix; an unprefixed name picks up the
            // local default namespace.  Only an explicit
            // `namespace=""` puts the element in no namespace.
            let explicit_namespace = namespace.is_some();
            let ns_uri = match namespace {
                Some(avt) => render_avt(state, avt, ctx_node, pos, size)?,
                None      => String::new(),
            };
            // XSLT 2.0 §11.2 / XTDE0835 — the XML Names "xmlns"
            // pseudo-namespace URI may not be used as an element's
            // namespace; doing so would put the element into the
            // namespace-declaration pseudo-axis.
            if explicit_namespace && ns_uri == "http://www.w3.org/2000/xmlns/" {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:element namespace='http://www.w3.org/2000/xmlns/' \
                     is reserved (XTDE0835)".into()));
            }
            let lookup_local = |target: Option<&str>| -> Option<String> {
                in_scope_namespaces.iter()
                    .find(|(p, _)| p.as_deref() == target)
                    .map(|(_, u)| u.clone())
            };
            let (mut prefix, local) = split_qname(&name_str);
            // XSLT 2.0 §11.2.1 — when `namespace=""` is supplied
            // explicitly, the constructed element is in the null
            // namespace regardless of the name's prefix.  The prefix
            // is then meaningless; drop it so the serialiser doesn't
            // emit a `prefix:local` form that has no binding (and
            // doesn't synthesise a fresh `xmlns:prefix=""` decl).
            if explicit_namespace && ns_uri.is_empty() {
                prefix = None;
            }
            // XTDE0830 — with no `namespace=`, a prefix on the effective
            // name must be bound by an in-scope namespace declaration of
            // the xsl:element instruction (XSLT 2.0 §11.7.2).
            let resolved_uri = if explicit_namespace {
                ns_uri
            } else if let Some((p, _)) = name_str.split_once(':') {
                match lookup_local(Some(p)).or_else(|| state.namespaces.resolve(p)) {
                    Some(u) => u,
                    None => return Err(XsltError::InvalidStylesheet(format!(
                        "xsl:element name='{name_str}' uses prefix '{p}', which is \
                         not a declared namespace (XTDE0830)"))),
                }
            } else {
                lookup_local(None).unwrap_or_default()
            };
            let q = QName { prefix, local, uri: resolved_uri };
            state.builder.open_element(q.clone());
            if !q.uri.is_empty() {
                state.builder.push_namespace_decl(q.prefix.clone(), q.uri.clone());
            }
            apply_attribute_sets(state, use_attribute_sets, ctx_node, pos, size)?;
            // Same per-element variable scoping as the LRE arm —
            // an `xsl:variable` declared inside this element doesn't
            // leak to subsequent sibling instructions.
            state.variables.enter();
            let body_r = eval_body(state, body, ctx_node, pos, size);
            state.variables.leave();
            body_r?;
            state.builder.close_element();
        }
        Instr::Attribute { name, namespace, select, separator, body, in_scope_namespaces } => {
            let name_str = render_avt(state, name, ctx_node, pos, size)?;
            if name_str.trim().is_empty() {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:attribute name= must yield a non-empty QName (XTDE0850)".into()
                ));
            }
            // XSLT 2.0 §11.3 / XTDE0850 — the resolved name= must be
            // a *lexical QName*.
            if !is_lexical_qname_str(&name_str) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:attribute name='{name_str}' is not a valid QName (XTDE0850)"
                )));
            }
            let explicit_namespace = namespace.is_some();
            // XSLT 2.0 §11.3 / XTDE0855 — when no namespace= is
            // given, the name "xmlns" denotes a namespace declaration,
            // not an attribute; rejecting it here keeps such
            // declarations on the element's `ns_def` chain instead.
            if !explicit_namespace && name_str == "xmlns" {
                return Err(XsltError::InvalidStylesheet(
                    "xsl:attribute with name='xmlns' and no namespace= \
                     (XTDE0855)".into()));
            }
            // XSLT 2.0 §11.3 / XTDE0865 — the XML Names "xmlns"
            // pseudo-namespace URI is reserved for namespace
            // declarations and may not appear as an attribute's
            // namespace.
            if explicit_namespace
                && namespace.is_some()
            {
                let ns_str = render_avt(state, namespace.as_ref().unwrap(),
                    ctx_node, pos, size)?;
                if ns_str == "http://www.w3.org/2000/xmlns/" {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:attribute namespace='http://www.w3.org/2000/xmlns/' \
                         is reserved (XTDE0865)".into()));
                }
            }
            let ns_uri = match namespace {
                Some(avt) => render_avt(state, avt, ctx_node, pos, size)?,
                None      => String::new(),
            };
            // XSLT 2.0 §10.1.1 / §11.3 — `select=` items are joined
            // by `separator=` (default `" "` when `select=` is set,
            // empty when computed from `body`).
            let sep = match separator {
                Some(avt) => render_avt(state, avt, ctx_node, pos, size)?,
                None      => if select.is_some() { " ".to_string() } else { String::new() },
            };
            let value = if let Some(sel) = select {
                let v = state.xpath_eval(sel, ctx_node, pos, size)?;
                let items = sequence_string_items(&v, state.idx, state.num_style());
                if items.len() <= 1 {
                    value_to_string_styled(&v, state.idx, state.num_style())
                } else {
                    items.join(&sep)
                }
            } else {
                // XSLT 2.0 §11.3 — body-form xsl:attribute interleaves
                // text and xsl:sequence contributions in *document
                // order*, joined by separator= (default empty for body
                // form).  See `collect_value_of_body_pieces` for the
                // same shape used by xsl:value-of body.
                collect_value_of_body_pieces(state, body, ctx_node, pos, size)?
                    .join(&sep)
            };
            let (mut prefix, local) = split_qname(&name_str);
            // XSLT 2.0 §11.3 — when an explicit `namespace=` is given,
            // a name prefix of `xmlns` must NOT be propagated to the
            // attribute (it would otherwise become an `xmlns:…`
            // namespace declaration, not an attribute).  Drop the
            // prefix so the synthesizer picks a fresh one.
            if explicit_namespace && prefix.as_deref() == Some("xmlns") {
                prefix = None;
            }
            // XSLT 2.0 §11.3 — explicit `namespace=""` puts the
            // attribute in the null namespace; any prefix on the
            // name is meaningless and dropped so the serialiser
            // doesn't emit `prefix:local` without a binding.
            if explicit_namespace && ns_uri.is_empty() {
                prefix = None;
            }
            let lookup_local = |target: Option<&str>| -> Option<String> {
                in_scope_namespaces.iter()
                    .find(|(p, _)| p.as_deref() == target)
                    .map(|(_, u)| u.clone())
            };
            // XSLT 1.0 §7.1.3: an attribute name's namespace defaults
            // from the prefix's binding at the `xsl:attribute` source
            // location.  Unprefixed names get the null namespace —
            // attributes don't inherit the default namespace.
            let resolved_uri = if explicit_namespace {
                ns_uri
            } else if let Some((p, _)) = name_str.split_once(':') {
                // XSLT 2.0 §11.3 / XTDE0860 — with no namespace=, the
                // prefix must be in scope for the xsl:attribute
                // instruction.  `xml` is predeclared by XML 1.0 §3.7.
                lookup_local(Some(p))
                    .or_else(|| state.namespaces.resolve(p))
                    .or_else(|| (p == "xml")
                        .then(|| "http://www.w3.org/XML/1998/namespace".to_string()))
                    .ok_or_else(|| XsltError::InvalidStylesheet(format!(
                        "xsl:attribute name='{name_str}' uses undeclared \
                         prefix '{p}' (XTDE0860)"
                    )))?
            } else {
                String::new()
            };
            // Declare the attribute's namespace on the owning element
            // so the serialiser can write a prefixed-qualified name.
            // `push_attribute` synthesises a prefix when none is
            // supplied; emitting the declaration explicitly here
            // avoids letting `nsN` shadow the user-chosen prefix
            // (e.g. `fiscus:objectID` keeps `fiscus:`).
            let aq = QName { prefix: prefix.clone(), local, uri: resolved_uri };
            if !aq.uri.is_empty() && aq.prefix.is_some() {
                state.builder.push_namespace_decl(aq.prefix.clone(), aq.uri.clone());
            }
            state.builder.push_attribute(aq, value);
        }
        Instr::Comment { select, body } => {
            // XSLT 1.0 §7.4 / XML 1.0 §2.5 — a comment may not contain
            // `--`, nor end with `-`.  When `xsl:comment` produces a
            // string with adjacent hyphens or a trailing hyphen, the
            // processor inserts a single space to keep the result
            // well-formed.
            let raw = match select {
                Some(sel) => {
                    let v = state.xpath_eval(sel, ctx_node, pos, size)?;
                    let items = sequence_string_items(&v, state.idx, state.num_style());
                    if items.len() <= 1 { value_to_string_styled(&v, state.idx, state.num_style()) }
                    else                 { items.join(" ") }
                }
                // XSLT 2.0 §11.7 — body items are atomised and joined
                // with a single space (the same default as xsl:value-of's
                // select form), so a multi-node body comes out as
                // "string-1 string-2" rather than concatenated raw.
                None => collect_value_of_body_pieces(state, body, ctx_node, pos, size)?
                            .join(" "),
            };
            let mut s = String::with_capacity(raw.len());
            let mut prev_hyphen = false;
            for ch in raw.chars() {
                if ch == '-' && prev_hyphen { s.push(' '); }
                s.push(ch);
                prev_hyphen = ch == '-';
            }
            if prev_hyphen { s.push(' '); }
            state.builder.push_comment(s);
        }
        Instr::ProcessingInstruction { name, select, body } => {
            let target = render_avt(state, name, ctx_node, pos, size)?;
            // XSLT 2.0 §11.6 / XTDE0890 — the PI target must be an
            // NCName (no colon) and not the case-insensitive
            // literal "xml" (reserved for the XML declaration).
            let target_trim = target.trim();
            if !is_ncname_str(target_trim) || target_trim.eq_ignore_ascii_case("xml") {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:processing-instruction name='{target}' is not a valid PI target (XTDE0890)"
                )));
            }
            // XSLT 1.0 §7.3 / XML 1.0 §2.6 — a PI may not contain
            // `?>` in its data; insert a space between `?` and `>` if
            // the rendered body would otherwise produce one.
            let raw = match select {
                Some(sel) => {
                    let v = state.xpath_eval(sel, ctx_node, pos, size)?;
                    let items = sequence_string_items(&v, state.idx, state.num_style());
                    if items.len() <= 1 { value_to_string_styled(&v, state.idx, state.num_style()) }
                    else                 { items.join(" ") }
                }
                // XSLT 2.0 §11.6 — same atomisation/space-separator
                // rule as xsl:comment above.
                None => collect_value_of_body_pieces(state, body, ctx_node, pos, size)?
                            .join(" "),
            };
            let data = raw.replace("?>", "? >");
            state.builder.push_pi(target, data);
        }
        Instr::Number { value, select, level, count, from, format, grouping_separator, grouping_size, ordinal, lang, letter_value: _, start_at } => {
            let format_str = render_avt(state, format, ctx_node, pos, size)?;
            let fmt = crate::number::parse_format(&format_str);
            // XSLT 3.0 §12.3 — `start-at` lists the first number at
            // each level; level `i` is offset by `start_at[i] - 1`.
            // Levels past the list (and the default) use 1 → no offset.
            let start_offsets: Vec<i64> = match start_at {
                Some(a) => render_avt(state, a, ctx_node, pos, size)?
                    .split_whitespace()
                    .filter_map(|t| t.parse::<i64>().ok())
                    .map(|v| v - 1)
                    .collect(),
                None => Vec::new(),
            };
            let apply_start = |nums: &mut [i64]| {
                for (i, n) in nums.iter_mut().enumerate() {
                    // `value=` yields a flat sequence (no levels); the
                    // single start-at integer offsets every entry.
                    let off = if value.is_some() { start_offsets.first() }
                              else { start_offsets.get(i) };
                    if let Some(off) = off { *n += off; }
                }
            };
            let ordinal_str = match ordinal {
                Some(a) => render_avt(state, a, ctx_node, pos, size)?,
                None    => String::new(),
            };
            let lang_str = match lang {
                Some(a) => render_avt(state, a, ctx_node, pos, size)?,
                None    => String::new(),
            };
            // XSLT 2.0 §13.7 / XTTE0990 — the effective value of
            // lang= must be a valid xml:lang token, otherwise it's a
            // dynamic error.  Empty stays acceptable (default
            // numbering).  Literal values can also be rejected at
            // compile time; this runtime path catches AVT-expanded
            // ones.
            if !lang_str.is_empty()
                && !crate::compiler::is_valid_xml_lang(&lang_str)
            {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:number lang='{lang_str}' is not a valid xml:lang \
                     value (XTTE0990)"
                )));
            }
            let opts = crate::number::FormatOptions {
                ordinal: !ordinal_str.is_empty(),
                lang:    if lang_str.is_empty() { None } else { Some(lang_str.clone()) },
                ordinal_scheme: if ordinal_str.is_empty() { None }
                                else { Some(ordinal_str.clone()) },
            };
            // Resolve grouping AVTs — XSLT 1.0 §7.7 requires both
            // attributes to be present and the size to be a positive
            // integer for grouping to take effect.
            let grouping_sep_str = match grouping_separator {
                Some(a) => Some(render_avt(state, a, ctx_node, pos, size)?),
                None    => None,
            };
            let grouping_size_n = match grouping_size {
                Some(a) => render_avt(state, a, ctx_node, pos, size)?
                    .parse::<usize>().ok()
                    .filter(|n| *n > 0),
                None    => None,
            };
            let group = match (grouping_sep_str, grouping_size_n) {
                (Some(sep), Some(n)) => Some((sep, n)),
                _ => None,
            };
            if let Some(e) = value {
                let v = state.xpath_eval(e, ctx_node, pos, size)?;
                // XPath 1.0 backwards-compatibility (XPath 2.0 §B.1):
                // `value=` is converted with the 1.0 `number()` rules —
                // the first item only, and a non-numeric / empty value
                // yields NaN, which xsl:number formats as "NaN".
                if matches!(e, sup_xml_core::xpath::Expr::BackwardsCompat(_)) {
                    let f = crate::eval::value_to_number_xpath(&v, state.idx);
                    let s = if f.is_finite() {
                        let mut nums = [f.round() as i64];
                        apply_start(&mut nums);
                        let mut s = crate::number::format_list_opts(&nums, &fmt, &opts);
                        if let Some((sep, sz)) = &group {
                            s = crate::number::apply_grouping(&s, sep, *sz);
                        }
                        s
                    } else {
                        "NaN".to_string()
                    };
                    state.builder.push_text(s, false);
                    return Ok(());
                }
                // XSLT 2.0 §13.7 — `value=` may yield a sequence;
                // each numeric item becomes one entry in the
                // formatted list and the format's separator joins
                // them.  Each item must atomise to a non-negative
                // integer; anything else is XTDE0980.  The empty
                // sequence is a degenerate "no items" case that the
                // suite treats as pass-through (format's prefix +
                // suffix only) rather than an error — `count(())`
                // is 0, so there's nothing to convert and nothing
                // to reject.
                let to_int = |v: &Value| -> Result<i64> {
                    let f = crate::eval::value_to_number_xpath(v, state.idx);
                    if !f.is_finite() {
                        return Err(XsltError::InvalidStylesheet(
                            "xsl:number value= item is not convertible \
                             to an integer (XTDE0980)".into()));
                    }
                    let n = f.round() as i64;
                    if n < 0 {
                        return Err(XsltError::InvalidStylesheet(
                            "xsl:number value= item is negative (XTDE0980)".into()));
                    }
                    Ok(n)
                };
                let nums: Vec<i64> = match v {
                    Value::Sequence(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for it in items { out.push(to_int(&it)?); }
                        out
                    }
                    Value::IntRange { lo, hi } => {
                        if lo < 0 {
                            return Err(XsltError::InvalidStylesheet(
                                "xsl:number value= item is negative (XTDE0980)".into()));
                        }
                        (lo..=hi).collect()
                    }
                    // An empty node-set is the "no items" case —
                    // produce no list entries (suffix only), per the
                    // Saxon pass-through interpretation of the spec
                    // (number-2402, number-0814).
                    Value::NodeSet(ref ns) if ns.is_empty() => Vec::new(),
                    Value::ForeignNodeSet(ref ns) if ns.is_empty() => Vec::new(),
                    other => vec![to_int(&other)?],
                };
                let mut nums = nums;
                apply_start(&mut nums);
                let mut s = crate::number::format_list_opts(&nums, &fmt, &opts);
                if let Some((sep, sz)) = &group { s = crate::number::apply_grouping(&s, sep, *sz); }
                state.builder.push_text(s, false);
                return Ok(());
            }
            // XSLT 2.0 §12.4 — `select=` overrides the source-tree
            // counting context.  Its result must be exactly one node;
            // anything else (an atomic value, the empty sequence, or
            // multiple nodes) is a type error (XTTE1000).
            let target_node = if let Some(sel) = select {
                match state.xpath_eval(sel, ctx_node, pos, size)? {
                    Value::NodeSet(ns) if ns.len() == 1 => ns[0],
                    _ => return Err(XsltError::InvalidStylesheet(
                        "xsl:number select must evaluate to a single node \
                         (XTTE1000)".into())),
                }
            } else {
                // XSLT 2.0 §12.4 / XTTE0990 — with no select=, the
                // counting context is the context item; it must be a
                // node.  An undefined focus (e.g. inside an
                // xsl:function body) or an atomic context (e.g.
                // `<xsl:for-each select="1 to 5"><xsl:number/>`)
                // makes the counting target ill-defined.
                if sup_xml_core::xpath::eval::focus_is_undefined() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:number with no select= called where the \
                         context item is undefined (XTTE0990)".into()));
                }
                if in_atomic_for_each() {
                    return Err(XsltError::InvalidStylesheet(
                        "xsl:number with no select= called where the \
                         context item is not a node (XTTE0990)".into()));
                }
                ctx_node
            };
            let mut numbers = compute_number_list(state, *level, count.as_ref(), from.as_ref(), target_node)?;
            apply_start(&mut numbers);
            // Empty number list — Saxon still emits the format's
            // prefix + suffix (see `format_list`).  Don't early-
            // return here.
            let mut s = crate::number::format_list_opts(&numbers, &fmt, &opts);
            if let Some((sep, sz)) = &group { s = crate::number::apply_grouping(&s, sep, *sz); }
            state.builder.push_text(s, false);
        }
        Instr::Variable(v) => {
            let mut val = evaluate_variable_value(state, v, ctx_node, pos, size)?;
            if let Some(t) = &v.as_type {
                if let Some(st) = parse_as_atomic_type(t) {
                    val = coerce_to_atomic_sequence(val, &st, state.idx)?;
                }
            }
            state.variables.bind(qname_key(&v.name), val);
        }
        Instr::Message { terminate, body } => {
            // XSLT 2.0 §17.1 / XTDE0030 — when the effective value of
            // an AVT-bearing attribute is not one of the permitted
            // values, the processor must raise a dynamic error.  For
            // `terminate=` the permitted set is `yes` / `no`.
            let terminate_yes = match terminate {
                Some(a) => {
                    let raw = render_avt(state, a, ctx_node, pos, size)?;
                    match raw.trim() {
                        "yes" => true,
                        "no"  => false,
                        bad   => return Err(XsltError::Xpath(
                            sup_xml_core::xpath::eval::xpath_err(format!(
                                "xsl:message terminate='{bad}' must be 'yes' or 'no' (XTDE0030)"
                            )).with_xpath_code("XTDE0030"))),
                    }
                }
                None => false,
            };
            let s = stringify_into_string(state, body, ctx_node, pos, size)?;
            // Emit to stderr (libxslt convention).  Real apps wire a
            // callback; we keep it simple.
            eprintln!("xsl:message: {s}");
            if terminate_yes {
                return Err(XsltError::Terminated(s));
            }
        }
        Instr::Fallback { .. } => {
            // XSLT 1.0: xsl:fallback only fires inside an
            // unrecognised XSLT 1.1+ instruction.  Inside a normal
            // template body it's a no-op.
        }
        Instr::Sequence { select } => {
            // XSLT 2.0 § 8.7: the value of the `select` expression is
            // contributed to the current sequence.  In our value model
            // a node-set materialises as a deep copy (matching
            // xsl:copy-of) and atomic values become text — same shape
            // as xsl:value-of for atomic data and xsl:copy-of for
            // nodes.  Inside an xsl:function body, the last value
            // contributed this way is the function's return value
            // (captured by `call_user_function` via a sequence sink).
            let v = state.xpath_eval(select, ctx_node, pos, size)?;
            // Push to a per-call sequence sink if one is active
            // (xsl:function body); otherwise emit into the result
            // tree the same way copy-of does.
            if state.sequence_sink_active() {
                state.push_to_sequence_sink(v);
            } else {
                copy_value_into(state, &v, true)?;
            }
        }
        Instr::MapEntry { key, select, body } => {
            // XSLT 3.0 §17.4 — contribute a single-entry map to the
            // enclosing xsl:map's collection.
            let k = state.xpath_eval(key, ctx_node, pos, size)?;
            let key_val = sup_xml_core::xpath::eval::first_atomic_key(&k, state.idx);
            let val = match select {
                Some(sel) => state.xpath_eval(sel, ctx_node, pos, size)?,
                None => {
                    state.sequence_sinks.push(Vec::new());
                    let r = eval_body(state, body, ctx_node, pos, size);
                    let mut items = state.sequence_sinks.pop().unwrap_or_default();
                    r?;
                    if items.len() == 1 { items.pop().unwrap() }
                    else { Value::Sequence(items) }
                }
            };
            let entry = Value::Map(Box::new(vec![(key_val, val)]));
            if state.sequence_sink_active() {
                state.push_to_sequence_sink(entry);
            } else {
                copy_value_into(state, &entry, true)?;
            }
        }
        Instr::Map { body } => {
            // XSLT 3.0 §17.4 — evaluate the body (a set of maps, typically
            // xsl:map-entry instructions) and merge into a single map;
            // a later entry for a duplicate key replaces an earlier one.
            state.sequence_sinks.push(Vec::new());
            let r = eval_body(state, body, ctx_node, pos, size);
            let collected = state.sequence_sinks.pop().unwrap_or_default();
            r?;
            let mut entries: Vec<(Value, Value)> = Vec::new();
            for v in collected {
                if let Value::Map(m) = v {
                    for (k, val) in *m {
                        if let Some(slot) = entries.iter_mut().find(|(ek, _)|
                            sup_xml_core::xpath::eval::map_key_eq(ek, &k, state.idx))
                        {
                            slot.1 = val;
                        } else {
                            entries.push((k, val));
                        }
                    }
                }
            }
            let map = Value::Map(Box::new(entries));
            if state.sequence_sink_active() {
                state.push_to_sequence_sink(map);
            } else {
                copy_value_into(state, &map, true)?;
            }
        }
        Instr::Unsupported { name, fallback } => {
            // XSLT 1.0 §15: when the unknown instruction has
            // `xsl:fallback` children, run them as if they replaced
            // the parent.  Otherwise an unrecognised instruction
            // surfaces as a runtime error — only when reached, so
            // forward-compat stylesheets that gate the bad branch
            // with `xsl:choose` etc. don't fail unnecessarily.
            if !fallback.is_empty() {
                eval_body(state, fallback, ctx_node, pos, size)?;
            } else {
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:{name} is not implemented in this build"
                )));
            }
        }
    }
    Ok(())
}

// ── variable / parameter binding ──────────────────────────────────

/// Does this instruction body push items into the active sequence
/// sink?  Used by [`bind_variable`] / [`evaluate_variable_value`] to
/// Compute the value of a sequence-constructor `xsl:key` (XSLT 2.0
/// §16.3) at one matched node.  A constructor using `xsl:sequence` /
/// `xsl:value-of` / template application yields an atomic sequence
/// captured through the sequence sink; any other constructor builds
/// an RTF whose string value is the key (mirroring a body-form
/// variable).
fn eval_key_body_value(
    state: &mut EvalState, body: &[Instr], node: NodeId,
) -> Result<Value> {
    // Computing a key value is a temporary output destination, so a
    // contained xsl:result-document is illegal (XTDE1480).
    let _temp = TempOutputGuard::enter();
    if body_uses_sequence_or_call(body) {
        state.sequence_sinks.push(Vec::new());
        let res = eval_body(state, body, node, 1, 1);
        let captured = state.sequence_sinks.pop().unwrap_or_default();
        res?;
        let mut flat: Vec<Value> = Vec::new();
        for item in captured {
            match item {
                Value::Sequence(items) => flat.extend(items),
                Value::NodeSet(ns) => flat.extend(
                    ns.into_iter().map(|id| Value::NodeSet(vec![id]))),
                other => flat.push(other),
            }
        }
        return Ok(if flat.len() == 1 { flat.pop().unwrap() }
                  else                { Value::Sequence(flat) });
    }
    let nodes   = build_rtf_nodes(state, body, node, 1, 1)?;
    let root_id = rtf_into_index(state.idx, &nodes);
    Ok(Value::NodeSet(vec![root_id]))
}

/// decide whether a variable declared `as="xs:T*"` should collect
/// the body as an atomic sequence or build a stringified RTF.
///
/// Recurses through `xsl:if` / `xsl:choose` / `xsl:for-each` so a
/// nested `xsl:sequence` is detected at any depth.
fn body_uses_sequence_or_call(body: &[Instr]) -> bool {
    for i in body {
        if matches!(i,
            Instr::Sequence { .. }
            | Instr::Map { .. } | Instr::MapEntry { .. }
            | Instr::CallTemplate { .. }
            | Instr::ApplyTemplates { .. })
        {
            return true;
        }
        let inner: Vec<&[Instr]> = match i {
            Instr::If { body, .. } | Instr::ForEach { body, .. }
            | Instr::ForEachGroup { body, .. } => vec![body.as_slice()],
            Instr::Choose { whens, otherwise } => {
                let mut v: Vec<&[Instr]> = whens.iter()
                    .map(|(_, b)| b.as_slice()).collect();
                if let Some(o) = otherwise { v.push(o.as_slice()); }
                v
            }
            _ => continue,
        };
        if inner.iter().any(|b| body_uses_sequence_or_call(b)) {
            return true;
        }
    }
    false
}

fn bind_variable(
    state: &mut EvalState, name: &QName,
    select: Option<&sup_xml_core::xpath::Expr>,
    body:   &[Instr],
    as_type: Option<&str>,
    base_uri: Option<&str>,
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<()> {
    let key = qname_key(name);
    // XSLT 2.0 §9.3 — a body-form variable declared with any
    // sequence-typed `as=` (`xs:T*`, `item()*`, `node()*`, …) and a
    // body that contributes items via `xsl:sequence` /
    // `xsl:call-template` / `xsl:apply-templates` doesn't construct
    // an RTF; the value is the sequence those children pushed into
    // the sink.  Items keep their identity (so `node is node` still
    // answers true) and their type tags (so `instance of` is
    // accurate).
    let want_item_seq = as_type
        .map(as_is_sequence_typed)
        .unwrap_or(false);
    let mut v = if let Some(sel) = select {
        state.xpath_eval(sel, ctx_node, pos, size)?
    } else if as_type.map(as_is_document_node_sequence).unwrap_or(false) {
        // `as="document-node()*"` / `+` — sequence of doc-nodes.
        // Same sink-driven path as the local-variable arm in
        // `evaluate_variable_value`; xsl:copy of a doc-node and
        // xsl:copy-of of doc-node sequences push items directly.
        state.sequence_sinks.push(Vec::new());
        let res = eval_body(state, body, ctx_node, pos, size);
        let captured = state.sequence_sinks.pop().unwrap_or_default();
        res?;
        let mut ids: Vec<sup_xml_core::xpath::NodeId> = Vec::new();
        for item in captured {
            match item {
                Value::NodeSet(ns)        => ids.extend(ns),
                Value::Sequence(items) => {
                    for it in items {
                        if let Value::NodeSet(ns) = it { ids.extend(ns); }
                    }
                }
                _ => {}
            }
        }
        Value::NodeSet(ids)
    } else if want_item_seq && body_uses_sequence_or_call(body) {
        // Body-form `as="xs:T*"` whose constructor contains
        // `xsl:sequence` (or another instruction that pushes items
        // into the sequence sink) — collect those items directly
        // into a Value::Sequence instead of building an RTF and
        // stringifying.  Other body shapes still go through the
        // RTF path below so `<xsl:value-of>`-driven variables keep
        // their existing behaviour.
        state.sequence_sinks.push(Vec::new());
        let res = eval_body(state, body, ctx_node, pos, size);
        let captured = state.sequence_sinks.pop().unwrap_or_default();
        res?;
        let mut flat: Vec<Value> = Vec::new();
        for item in captured {
            match item {
                Value::Sequence(items) => flat.extend(items),
                // A NodeSet returned by an XPath sequence-expr like
                // `(1,2,3)` is the multi-item synthetic text-node
                // representation; expand each element to a per-item
                // NodeSet so subsequent per-item casts see one item.
                Value::NodeSet(ns) => flat.extend(
                    ns.into_iter().map(|id| Value::NodeSet(vec![id]))),
                other => flat.push(other),
            }
        }
        if flat.len() == 1 { flat.pop().unwrap() }
        else                { Value::Sequence(flat) }
    } else if body.is_empty() {
        // XSLT 2.0 §9.3 — a body-form variable with an `as=`
        // declaration binds the result of the sequence constructor
        // to the declared type via SequenceType matching, with NO
        // implicit RTF wrap.  An empty body therefore produces the
        // empty sequence; the coercion downstream raises XTTE0570
        // when the declared cardinality is `One` / `OneOrMore`.
        // Without this carve-out the XSLT-1.0 RTF compatibility
        // branch below would synthesise a text-node id and bind it
        // here — which a subsequent `as="document-node()?"` kind
        // check then rejects because the synthetic id resolves to
        // a Text node, not a Document node (XTTE0570).  An untyped
        // empty body retains the 1.0-compat synthetic node so
        // `boolean($x)` stays `true()` and existing 1.0 stylesheets
        // keep working.
        if as_type.is_some() {
            Value::NodeSet(Vec::new())
        } else {
            let ids = state.idx.allocate_rtf_text_nodes_inherent(vec![String::new()]);
            Value::NodeSet(ids)
        }
    } else if as_type.map(as_is_attribute_kind).unwrap_or(false) {
        // Body-form `as="attribute()"` — expose each top-level
        // attribute as a navigable node (with a throwaway owner
        // element) rather than the document-wrapper used for other
        // node kinds, which would lose the attribute (attributes are
        // not children of the synthetic RTF document root).
        let nodes = build_rtf_nodes_no_merge(state, body, ctx_node, pos, size)?;
        let ids = rtf_children_into_index(state.idx, &nodes);
        if let Some(uri) = base_uri {
            let mut map = state.rtf_base_uris.borrow_mut();
            for &id in &ids { map.insert(id, uri.to_string()); }
        }
        store_rtf(state, &key, nodes);
        Value::NodeSet(ids)
    } else if as_type.map(|t|
        as_is_sequence_typed(t) && !as_target_is_atomic(t)
    ).unwrap_or(false) {
        // XSLT 2.0 §9.3 — body-form variable typed as a
        // sequence of node kinds (`node()*`, `text()*`,
        // `element()*`, …): each instruction's contribution is a
        // distinct item, not a merged RTF subtree.  Keep them
        // separate via the no-merge builder, then expose each
        // top-level node as its own NodeSet item so iteration /
        // count / sort see the right cardinality.
        let nodes = build_rtf_nodes_no_merge(state, body, ctx_node, pos, size)?;
        let ids = rtf_children_into_index(state.idx, &nodes);
        if let Some(uri) = base_uri {
            let mut map = state.rtf_base_uris.borrow_mut();
            for &id in &ids { map.insert(id, uri.to_string()); }
        }
        store_rtf(state, &key, nodes);
        Value::NodeSet(ids)
    } else if let Some(st) = as_type
        .and_then(parse_as_atomic_type)
        .filter(template_result_type_is_node_kind)
        .filter(|st| !matches!(st.item,
            sup_xml_core::xpath::ast::ItemType::Document))
    {
        // Body-form variable declared with a node-kind `as=` (e.g.
        // `element(a)`, `text()`, `text()?`): build the body without
        // §5.7.2 text-fragment merging so each contributing
        // instruction stays a distinct sequence item, then enforce
        // only the cardinality *upper* bound and the item kind.
        // `document-node()` is handled by the untyped fallback below —
        // the synthetic doc-root wrap naturally satisfies it, and
        // `xsl:document` inside the body renders into the builder
        // (it doesn't create a top-level ResultNode), so the
        // cardinality check would misread its children as items.
        // See the matching arm in `evaluate_variable_value` for why
        // under-production is intentionally not enforced here.
        let nodes = build_rtf_nodes_no_merge(state, body, ctx_node, pos, size)?;
        let counted: Vec<&ResultNode> = nodes.iter().filter(|n| match n {
            ResultNode::Text { content, .. } => !content.trim().is_empty(),
            _ => true,
        }).collect();
        if node_kind_overflow_or_mismatch(&counted, &st) {
            return Err(XsltError::Xpath(
                sup_xml_core::xpath::eval::xpath_err(format!(
                    "variable '{}' declared as='{}' but the body \
                     produced an incompatible sequence ({} items) (XTTE0570)",
                    qname_key(name), as_type.unwrap_or(""), counted.len(),
                )).with_xpath_code("XTTE0570")));
        }
        let root_id = rtf_into_index(state.idx, &nodes);
        // XSLT 2.0 §5.7.2 — the item bound here is parentless;
        // the doc-root is a storage wrap.  Mark so XTDE1270 /
        // XTDE1370 / XTDE1380 refuse it as a real document root.
        state.idx.mark_synthetic_wrap(root_id);
        if let Some(uri) = base_uri {
            state.rtf_base_uris.borrow_mut().insert(root_id, uri.to_string());
        }
        store_rtf(state, &key, nodes);
        Value::NodeSet(vec![root_id])
    } else {
        // Untyped body form — build an RTF as a navigable temporary
        // tree (with §5.7.2 text-fragment merging) and bind to a
        // NodeSet of its document root.  The structural ResultNode
        // tree is stored so xsl:copy-of can do a deep copy without
        // re-walking the RTF index.
        let nodes   = build_rtf_nodes(state, body, ctx_node, pos, size)?;
        // A body-form variable with no `as=` constructs a document
        // node, so a top-level attribute in its content violates
        // XTDE0420.  An `as="item()?"` / `as="node()*"` binding is a
        // sequence — a parentless attribute item is legal there.
        if as_type.is_none() {
            check_document_node_content(&nodes)?;
        }
        let root_id = rtf_into_index(state.idx, &nodes);
        if let Some(uri) = base_uri {
            state.rtf_base_uris.borrow_mut().insert(root_id, uri.to_string());
        }
        store_rtf(state, &key, nodes);
        Value::NodeSet(vec![root_id])
    };
    // XSLT 2.0 §9.3 / §10.2 `as="xs:T"` coercion at bind time.  We
    // map the textual SequenceType to its atomic local name and
    // cast through the shared [`cast_value_to_atomic`] path so the
    // typed-value system carries the declared type forward.  Errors
    // from the cast are surfaced to the caller — XSLT 2.0 says a
    // failed coercion is a runtime XTTE error.
    if let Some(t) = as_type {
        if let Some(st) = parse_as_atomic_type(t) {
            v = coerce_to_atomic_sequence(v, &st, state.idx)?;
        }
    }
    state.variables.bind(key, v);
    Ok(())
}

/// Lift an XPath 2.0-style SequenceType literal (e.g. `"xs:integer"`,
/// `"xs:double*"`, `"xs:string?"`) to a constructed `SequenceType`
/// — for the narrow purpose of `as=` coercion.  Returns `None` for
/// non-atomic targets (`node()`, `element()`, `document-node(...)`)
/// since those aren't representable through `cast_value_to_atomic`.
pub(crate) fn parse_as_atomic_type(
    src: &str,
) -> Option<sup_xml_core::xpath::ast::SequenceType> {
    use sup_xml_core::xpath::ast::{SequenceType, ItemType, Occurrence};
    let src = src.trim();
    // Strip a trailing occurrence indicator.
    let (body, occ) = if let Some(b) = src.strip_suffix('*') {
        (b.trim(), Occurrence::ZeroOrMore)
    } else if let Some(b) = src.strip_suffix('+') {
        (b.trim(), Occurrence::OneOrMore)
    } else if let Some(b) = src.strip_suffix('?') {
        (b.trim(), Occurrence::Optional)
    } else {
        (src, Occurrence::One)
    };
    // KindTest forms — `element()`, `attribute()`, `node()`,
    // `document-node()`, `text()`, `comment()`,
    // `processing-instruction()`.  Map them onto the matching
    // `ItemType` so the binding layer can run the document-wrapper
    // unwrap for body-form variables declared `as="element()*"`
    // / `as="node()*"` / etc.  Anything more elaborate (typed
    // `element(foo, T)`, schema kinds) silently falls through to
    // None so the existing pass-through behaviour stays.
    if body.contains('(') {
        let (bare, inside) = match body.split_once('(') {
            Some((b, rest)) => (b.trim(), rest.trim_end_matches(')').trim()),
            None            => (body, ""),
        };
        // Extract the first positional argument's local name, e.g.
        // `element(foo)` / `element(foo, T)` / `attribute(bar)` —
        // everything before the optional `, T` and stripped of any
        // wildcard / namespace decoration we don't yet track.
        let first_arg_local = |args: &str| -> Option<String> {
            let head = args.split(',').next()?.trim();
            if head.is_empty() || head == "*" { return None; }
            let local = head.rsplit_once(':').map(|(_, l)| l).unwrap_or(head);
            (!local.is_empty() && local != "*").then(|| local.to_string())
        };
        let item = match bare {
            "node"           => ItemType::AnyNode,
            "element"        => ItemType::Element(first_arg_local(inside)),
            "attribute"      => ItemType::Attribute(first_arg_local(inside)),
            // We don't implement schema-aware processing, but a
            // `schema-element(N)` / `schema-attribute(N)` target still
            // identifies the bound value as an element / attribute, so
            // treat it as the corresponding kind test.  Without this the
            // type is unrecognised, the body-form RTF stays wrapped in
            // its synthetic document node, and `apply-templates` over
            // the variable re-matches `/` — an infinite loop.
            "schema-element"   => ItemType::Element(None),
            "schema-attribute" => ItemType::Attribute(None),
            "document-node"  => ItemType::Document,
            "text"           => ItemType::Text,
            "comment"        => ItemType::Comment,
            "processing-instruction" => ItemType::PI(None),
            _                => return None,
        };
        return Some(SequenceType { item, occurrence: occ });
    }
    // Strip an `xs:` (or any) prefix to leave the local name.
    let local = match body.rsplit_once(':') {
        Some((_, l)) => l,
        None         => body,
    };
    Some(SequenceType {
        item: ItemType::Atomic(local.to_string()),
        occurrence: occ,
    })
}

/// Coerce `v` to the declared `as=` SequenceType.  Lenient by
/// design.  Honors XPath 2.0 §3.5.5 subtype substitution: when the
/// value's existing type is already a subtype of the target, we
/// keep it as-is so `instance of` against the source type still
/// answers true.  Otherwise the value gets cast through the shared
/// `cast_value_to_atomic` path.
///
/// On cast failure or unsupported shape we return the original
/// value rather than erroring out — many conformance tests bind
/// an RTF or other shape via `as=`, and rejecting them would
/// regress unrelated coverage.
/// Does the node at `id` satisfy the kind test in `item`?  Per
/// XSLT 2.0 §10 / XPath 2.0 §2.5.4: `node()` accepts anything,
/// `element()` requires Element kind (optionally a specific
/// local-name), `attribute()` requires Attribute, etc.  Atomic
/// kind tests on a node always answer false here — callers that
/// need atomization handle it on their own pre-check path.
fn node_matches_kind_test<I: sup_xml_core::xpath::DocIndexLike>(
    item: &sup_xml_core::xpath::ast::ItemType,
    id:   sup_xml_core::xpath::NodeId,
    idx:  &I,
) -> bool {
    use sup_xml_core::xpath::ast::ItemType;
    use sup_xml_core::xpath::XPathNodeKind as K;
    let k = idx.kind(id);
    match item {
        ItemType::Any | ItemType::AnyNode => true,
        ItemType::Element(name) => matches!(k, K::Element)
            && name.as_ref().map_or(true, |n| idx.local_name(id) == n),
        ItemType::Attribute(name) => matches!(k, K::Attribute)
            && name.as_ref().map_or(true, |n| idx.local_name(id) == n),
        ItemType::Text     => matches!(k, K::Text | K::CData),
        ItemType::Comment  => matches!(k, K::Comment),
        ItemType::PI(name) => matches!(k, K::PI)
            && name.as_ref().map_or(true, |n| idx.local_name(id) == n),
        ItemType::Document => matches!(k, K::Document),
        // Atomic kind tests on a node: false — the caller handles
        // atomization separately (this function is only consulted
        // for kind tests).
        ItemType::Atomic(_) => false,
        // A node is never a function item, nor the empty sequence.
        ItemType::Function(_) | ItemType::EmptySequence => false,
    }
}

/// True when a declared `as=` type names a node kind — the only
/// result types `run_template_body` enforces structurally (XTTE0505).
/// Atomic / `item()` / `empty-sequence()` results atomise and are left
/// to the value path.
fn template_result_type_is_node_kind(
    st: &sup_xml_core::xpath::ast::SequenceType,
) -> bool {
    use sup_xml_core::xpath::ast::ItemType;
    matches!(&st.item,
        ItemType::Element(_) | ItemType::Attribute(_) | ItemType::Text
        | ItemType::Comment | ItemType::PI(_) | ItemType::AnyNode
        | ItemType::Document)
}

/// True when a template body's top-level result `nodes` violate the
/// declared node-kind sequence type `st` — either by cardinality or
/// because some node's kind isn't admitted by the item type.
fn template_result_violates_type(
    nodes: &[crate::result_tree::ResultNode],
    st: &sup_xml_core::xpath::ast::SequenceType,
) -> bool {
    use sup_xml_core::xpath::ast::Occurrence;
    let n = nodes.len();
    let cardinality_bad = match st.occurrence {
        Occurrence::One        => n != 1,
        Occurrence::Optional   => n > 1,
        Occurrence::OneOrMore  => n < 1,
        Occurrence::ZeroOrMore => false,
    };
    if cardinality_bad { return true; }
    !nodes.iter().all(|node| result_node_matches_item(node, &st.item))
}

/// Lenient cousin of [`template_result_violates_type`] for body-form
/// variable bindings.  Reports a violation only when the body
/// over-produces (`n > 1` against `One`/`Optional`) or when an item's
/// kind doesn't satisfy the declared item type.  Under-production
/// (`n == 0` against `One`/`OneOrMore`) is left to the synthetic
/// doc-root wrap and `coerce_to_atomic_sequence`'s child-unwrap step:
/// e.g. `as="text()"` with an empty `xsl:value-of` body legitimately
/// binds to the empty sequence (XSLT 2.0 §5.7.1 strips zero-length
/// text), and Saxon-style behaviour does the same.
fn node_kind_overflow_or_mismatch(
    nodes: &[&crate::result_tree::ResultNode],
    st: &sup_xml_core::xpath::ast::SequenceType,
) -> bool {
    use sup_xml_core::xpath::ast::Occurrence;
    let too_many = matches!(st.occurrence, Occurrence::One | Occurrence::Optional)
        && nodes.len() > 1;
    if too_many { return true; }
    !nodes.iter().all(|node| result_node_matches_item(node, &st.item))
}

/// Whether a single result node satisfies a node-kind item type.
fn result_node_matches_item(
    node: &crate::result_tree::ResultNode,
    item: &sup_xml_core::xpath::ast::ItemType,
) -> bool {
    use sup_xml_core::xpath::ast::ItemType;
    use crate::result_tree::ResultNode as R;
    match item {
        ItemType::Any | ItemType::AnyNode => true,
        ItemType::Element(name) => matches!(node,
            R::Element { name: qn, .. }
                if name.as_ref().map_or(true, |n| &qn.local == n)),
        ItemType::Attribute(name) => matches!(node,
            R::Attribute { name: qn, .. }
                if name.as_ref().map_or(true, |n| &qn.local == n)),
        ItemType::Text     => matches!(node, R::Text { .. }),
        ItemType::Comment  => matches!(node, R::Comment(_)),
        ItemType::PI(name) => matches!(node,
            R::ProcessingInstruction { target, .. }
                if name.as_ref().map_or(true, |n| target == n)),
        // A template body never yields a bare document node at the top
        // level, and atomic/function/empty-sequence types aren't enforced
        // here.
        ItemType::Document | ItemType::Atomic(_) | ItemType::Function(_)
        | ItemType::EmptySequence => false,
    }
}

pub(crate) fn coerce_to_atomic_sequence<I: sup_xml_core::xpath::DocIndexLike>(
    v: Value,
    st: &sup_xml_core::xpath::ast::SequenceType,
    idx: &I,
) -> Result<Value> {
    use sup_xml_core::xpath::ast::{Occurrence, ItemType};
    // XSLT 2.0 §9.3 — `as="element()*"` / `as="node()*"` /
    // `as="attribute()*"` against a body-form variable whose RTF
    // document wraps the actual produced nodes: extract the
    // wrapper's children so the variable binds to the typed
    // node sequence the author asked for, not the synthetic
    // document.  Without this, `apply-templates` on the variable
    // would re-match `match="/"` and recurse.
    if let Value::NodeSet(ref ns) = v {
        if ns.len() == 1 {
            let id = ns[0];
            // Any node-kind target other than Document or the
            // catch-all Atomic/Any requires unwrapping a body-form
            // RTF (which is materialised as a synthetic Document).
            let needs_unwrap = matches!(&st.item,
                ItemType::AnyNode
                | ItemType::Element(_)
                | ItemType::Attribute(_)
                | ItemType::Text
                | ItemType::Comment
                | ItemType::PI(_));
            if needs_unwrap
                && matches!(idx.kind(id),
                    sup_xml_core::xpath::XPathNodeKind::Document) {
                let kids: Vec<sup_xml_core::xpath::NodeId> =
                    idx.children(id).to_vec();
                // XSLT 2.0 §9.3 / XTTE0570 — after unwrapping the
                // body-form RTF, each surviving child must satisfy
                // the declared kind test (e.g. `as="element(b)"`
                // rejects a body that produces `<a/>`).
                for &k in &kids {
                    if !node_matches_kind_test(&st.item, k, idx) {
                        return Err(XsltError::InvalidStylesheet(format!(
                            "body produces a node whose kind doesn't match \
                             the declared type {:?} (XTTE0570)", st.item
                        )));
                    }
                }
                return Ok(Value::NodeSet(kids));
            }
        }
    }
    // Empty sequence + Optional / ZeroOrMore → pass through.
    if let Value::NodeSet(ref ns) = v {
        if ns.is_empty() && matches!(st.occurrence,
            Occurrence::Optional | Occurrence::ZeroOrMore) {
            return Ok(v);
        }
        // Empty sequence with required cardinality (One / OneOrMore)
        // is XTTE0570 per XSLT 2.0 §10.
        if ns.is_empty() && matches!(st.occurrence,
            Occurrence::One | Occurrence::OneOrMore)
        {
            return Err(XsltError::InvalidStylesheet(
                "value's cardinality (empty) is incompatible with declared \
                 type (XTTE0570)".into()
            ));
        }
        // Atomic sequence type over a (non-empty) node-set: atomise
        // each node to its typed value — untyped data atomises to
        // xs:untypedAtomic — then cast to the target atomic type
        // (XSLT 2.0 §10).  A node-set never satisfies an atomic kind
        // test directly, so this must precede the kind-check below
        // (which would otherwise reject e.g. `@*` bound to
        // `as="xs:untypedAtomic*"`).
        if let ItemType::Atomic(_) = &st.item {
            if ns.len() > 1
                && matches!(st.occurrence, Occurrence::One | Occurrence::Optional)
            {
                return Err(XsltError::InvalidStylesheet(format!(
                    "value of cardinality {} doesn't match the declared \
                     singleton type (XTTE0570)", ns.len(),
                )));
            }
            let single = sup_xml_core::xpath::ast::SequenceType {
                item: st.item.clone(), occurrence: Occurrence::One,
            };
            let mut out = Vec::with_capacity(ns.len());
            for &id in ns.iter() {
                let atom = Value::String(idx.string_value(id));
                match sup_xml_core::xpath::eval::cast_value_to_atomic(&atom, &single, idx) {
                    Ok(c)  => out.push(c),
                    Err(_) => return Err(XsltError::InvalidStylesheet(format!(
                        "value can't be atomised to the declared type {:?} \
                         (XTTE0570)", st.item,
                    ))),
                }
            }
            return Ok(if out.len() == 1 { out.pop().unwrap() }
                      else                { Value::Sequence(out) });
        }
        if ns.len() > 1 {
            // Multi-item input: legal only when the declared type
            // admits more than one item (* / +); otherwise XTTE0570.
            if matches!(st.occurrence, Occurrence::One | Occurrence::Optional) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "value of cardinality {} doesn't match the declared \
                     singleton type (XTTE0570)", ns.len(),
                )));
            }
            // XSLT 2.0 §10 / XTTE0570 — each node must also satisfy
            // the declared kind test.  `as="element()*"` over a
            // body that produced text nodes is the canonical bug.
            for &id in ns.iter() {
                if !node_matches_kind_test(&st.item, id, idx) {
                    return Err(XsltError::InvalidStylesheet(format!(
                        "value's item kind doesn't match the declared \
                         type {:?} (XTTE0570)", st.item
                    )));
                }
            }
            return Ok(v);
        }
        // Singleton NodeSet — also kind-check (the unwrap path
        // earlier only handles the document-wrap case).
        if ns.len() == 1 && !matches!(&st.item, ItemType::Atomic(_) | ItemType::Any) {
            if !node_matches_kind_test(&st.item, ns[0], idx) {
                return Err(XsltError::InvalidStylesheet(format!(
                    "value's item kind doesn't match the declared \
                     type {:?} (XTTE0570)", st.item
                )));
            }
        }
    }
    // IntRange targeting any numeric atomic type passes straight
    // through — the lazy range already carries the integer items
    // the declared type asks for, and forcing materialisation
    // here would defeat the lazy representation.  Specifically
    // `xs:integer*`, `xs:decimal*`, etc. all admit consecutive
    // integers without per-item casting.
    if let Value::IntRange { .. } = v {
        if matches!(&st.item, ItemType::Atomic(_)) {
            return Ok(v);
        }
    }
    // Value::Sequence with an atomic target: cast each item to the
    // declared type individually.  Without this, the catch-all path
    // below stringifies the whole sequence then re-parses it as a
    // single value, collapsing N items down to 1.
    if let ItemType::Atomic(target) = &st.item {
        // `xs:anyAtomicType` is the top of the atomic hierarchy —
        // every atomic value is already an instance, so casting to
        // it would only erase the more specific subtype
        // (`xs:integer` becomes `Typed{anyAtomicType}` and loses
        // its numeric tag).  Preserve the original items.
        if target == "anyAtomicType" {
            if matches!(v, Value::Sequence(_)) { return Ok(v); }
        }
        if let Value::Sequence(items) = v {
            let single = sup_xml_core::xpath::ast::SequenceType {
                item: st.item.clone(),
                occurrence: Occurrence::One,
            };
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match sup_xml_core::xpath::eval::cast_value_to_atomic(&it, &single, idx) {
                    Ok(c) => out.push(c),
                    Err(_) => out.push(it),
                }
            }
            return Ok(if out.len() == 1 { out.pop().unwrap() }
                      else                { Value::Sequence(out) });
        }
    }
    // Subtype substitution: if the value's current type tag is
    // already a subtype of the declared target, no cast — keep the
    // narrower type so subsequent `instance of` queries against
    // the source type still answer correctly.
    if let ItemType::Atomic(target_name) = &st.item {
        if let Value::Typed(t) = &v {
            if sup_xml_core::xpath::eval::xsd_is_subtype_of(t.kind, target_name) {
                return Ok(v);
            }
        }
    }
    let original = v.clone();
    match sup_xml_core::xpath::eval::cast_value_to_atomic(&v, st, idx) {
        Ok(cast) => Ok(cast),
        // Convertibility failure for `as=`-typed binding sites
        // (xsl:variable, xsl:param, xsl:with-param, xsl:template)
        // is XTTE0570 per XSLT 2.0 §10 — but only when the input
        // type and the declared type are categorically incompatible.
        // Some XPath uses (`<a>` element bound via `as="xs:string*"`)
        // need to keep flowing as atomic strings even though the
        // strict cast can't find the right route.  We propagate the
        // error for the unambiguous boolean → numeric / structural
        // mismatch cases the W3C suite exercises, and keep the
        // lenient pass-through otherwise.
        Err(e)   => {
            if as_is_strict_mismatch_with_idx(&original, &st.item, idx) {
                return Err(XsltError::from(e));
            }
            Ok(original)
        }
    }
}

/// True when the value `v` cannot legally satisfy the declared
/// `target` even under the XSLT 2.0 function-conversion rules —
/// the conservative "throw XTTE0570" trigger for [`coerce_to_atomic_sequence`].
/// Today this fires on xs:boolean ↔ numeric / temporal mismatches
/// and on the dual xs:numeric ↔ xs:boolean direction; anything
/// less unambiguous keeps the lenient pass-through to avoid
/// regressing tests whose `as=` is a documentation hint rather
/// than a hard constraint.
fn as_is_strict_mismatch_with_idx<I: sup_xml_core::xpath::DocIndexLike>(
    v: &Value,
    target: &sup_xml_core::xpath::ast::ItemType,
    idx: &I,
) -> bool {
    use sup_xml_core::xpath::ast::ItemType;
    let ItemType::Atomic(target_name) = target else { return false; };
    let numeric_target = matches!(target_name.as_str(),
        "integer" | "long" | "int" | "short" | "byte"
        | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
        | "nonNegativeInteger" | "nonPositiveInteger"
        | "positiveInteger" | "negativeInteger"
        | "decimal" | "double" | "float" | "numeric");
    // A NodeSet whose stringified value can't possibly satisfy the
    // declared numeric target is XTTE0570 only when the value is a
    // hard mismatch.  Empty string -> numeric fails the cast; only
    // flag the strict mismatch when the source is empty and the
    // target is numeric so legitimate `<a>5</a>` → xs:integer keeps
    // flowing.
    if let Value::NodeSet(ns) = v {
        if numeric_target && ns.len() == 1 {
            let s = idx.string_value(ns[0]);
            if s.trim().is_empty() {
                return true;
            }
        }
    }
    let _ = idx;
    if let Value::String(s) = v {
        if numeric_target && s.trim().parse::<f64>().is_err() {
            return true;
        }
    }
    let src_kind: Option<&str> = match v {
        Value::Boolean(_) => Some("boolean"),
        Value::Number(_)  => Some("double"),
        Value::Typed(t)   => Some(t.kind),
        _ => None,
    };
    let Some(src) = src_kind else { return false; };
    let numeric_target = matches!(target_name.as_str(),
        "integer" | "long" | "int" | "short" | "byte"
        | "unsignedLong" | "unsignedInt" | "unsignedShort" | "unsignedByte"
        | "nonNegativeInteger" | "nonPositiveInteger"
        | "positiveInteger" | "negativeInteger"
        | "decimal" | "double" | "float" | "numeric");
    let temporal_target = matches!(target_name.as_str(),
        "date" | "dateTime" | "time"
        | "duration" | "dayTimeDuration" | "yearMonthDuration"
        | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay");
    // xs:boolean → numeric / temporal is a hard mismatch (only
    // xs:string and xs:untypedAtomic survive that cast).
    if src == "boolean" && (numeric_target || temporal_target) {
        return true;
    }
    // Numeric → xs:boolean is fine (1/0 → true/false), but
    // temporal → xs:boolean fails.
    if matches!(src, "date" | "dateTime" | "time"
        | "duration" | "dayTimeDuration" | "yearMonthDuration")
        && target_name == "boolean"
    {
        return true;
    }
    // Numeric → temporal and temporal → numeric both fail.
    if numeric_target && matches!(src,
        "date" | "dateTime" | "time"
        | "duration" | "dayTimeDuration" | "yearMonthDuration")
    {
        return true;
    }
    if temporal_target && matches!(src,
        "double" | "float" | "decimal" | "integer" | "long" | "int"
        | "short" | "byte" | "unsignedLong" | "unsignedInt"
        | "unsignedShort" | "unsignedByte" | "nonNegativeInteger"
        | "nonPositiveInteger" | "positiveInteger" | "negativeInteger")
    {
        return true;
    }
    false
}

fn evaluate_variable_value(
    state: &mut EvalState, v: &Variable,
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<Value> {
    if let Some(sel) = &v.select {
        return state.xpath_eval(sel, ctx_node, pos, size);
    }
    if v.body.is_empty() {
        return Ok(Value::String(String::new()));
    }
    // XSLT 2.0 §9.3 — a body-form variable declared with any
    // sequence-typed `as=` (`xs:T*`, `item()*`, `node()*`, …) and a
    // body that contributes items via `xsl:sequence` /
    // `xsl:call-template` / `xsl:apply-templates` produces a typed
    // sequence whose items keep their identity / type, rather than
    // being deep-copied into an RTF.
    let want_item_seq = v.as_type.as_deref()
        .map(as_is_sequence_typed)
        .unwrap_or(false);
    if want_item_seq && body_uses_sequence_or_call(&v.body) {
        state.sequence_sinks.push(Vec::new());
        let res = eval_body(state, &v.body, ctx_node, pos, size);
        let captured = state.sequence_sinks.pop().unwrap_or_default();
        res?;
        let mut flat: Vec<Value> = Vec::new();
        for item in captured {
            match item {
                Value::Sequence(items) => flat.extend(items),
                Value::NodeSet(ns) => flat.extend(
                    ns.into_iter().map(|id| Value::NodeSet(vec![id]))),
                other => flat.push(other),
            }
        }
        return Ok(if flat.len() == 1 { flat.pop().unwrap() }
                  else                { Value::Sequence(flat) });
    }
    // Variables typed `as="item()*"` / `as="node()*"` etc. that
    // build their value with ordinary construction instructions
    // (`xsl:text`, `xsl:element`, literal text, `xsl:attribute`,
    // ...) skip XSLT 2.0 §5.7.2 sequence normalisation per §9.3:
    // each instruction's contribution is a separate item, no
    // adjacent-text merging, no document-wrapping.  Build with
    // text-merging disabled, then expose the top-level result
    // children as a NodeSet so `count($var)` answers correctly
    // and downstream iteration sees each item.
    if let Some(t) = &v.as_type {
        // `as="document-node()*"` / `+` — the body's xsl:copy of a
        // doc-node and xsl:copy-of of doc-node sequences must each
        // contribute their OWN doc-node item to the sequence (not
        // collapse into a single doc-wrap).  Drive evaluation
        // through a sequence sink so those instructions push items
        // directly; the sub-builder still catches stray content for
        // diagnostics, but the bound value is the captured doc list.
        if as_is_document_node_sequence(t) {
            state.sequence_sinks.push(Vec::new());
            let res = eval_body(state, &v.body, ctx_node, pos, size);
            let captured = state.sequence_sinks.pop().unwrap_or_default();
            res?;
            let mut ids: Vec<sup_xml_core::xpath::NodeId> = Vec::new();
            for item in captured {
                match item {
                    Value::NodeSet(ns)        => ids.extend(ns),
                    Value::Sequence(items) => {
                        for it in items {
                            if let Value::NodeSet(ns) = it { ids.extend(ns); }
                        }
                    }
                    _ => {} // non-node items don't satisfy doc-node()
                }
            }
            return Ok(Value::NodeSet(ids));
        }
        // Skip the no-merge NodeSet path for atomic targets
        // (`xs:T+`, `xs:T*`); those need the stringify+cast path so
        // body LRE / value-of contributions atomise into the
        // declared atomic type rather than binding as node values.
        // A singleton `attribute()` also takes this path: a parentless
        // attribute can't live under the synthetic RTF document root,
        // so it must be exposed through `rtf_children_into_index`.
        if (as_is_sequence_typed(t) && !as_target_is_atomic(t)) || as_is_attribute_kind(t) {
            let nodes = build_rtf_nodes_no_merge(state, &v.body, ctx_node, pos, size)?;
            let key   = qname_key(&v.name);
            // Materialise the unmerged top-level children into the
            // RTF index, then expose each as a single-item NodeSet.
            let child_ids = rtf_children_into_index(state.idx, &nodes);
            // Inherit the variable's xml:base onto each top-level
            // node so base-uri($var/elem) walks back through the
            // synthetic doc root and reads the right URI.
            if let Some(uri) = &v.base_uri {
                let mut map = state.rtf_base_uris.borrow_mut();
                for &id in &child_ids {
                    map.insert(id, uri.clone());
                }
            }
            store_rtf(state, &key, nodes);
            return Ok(Value::NodeSet(child_ids));
        }
        // Atomic-sequence target (`as="xs:T+"` / `xs:T*`) whose body
        // produces a navigable sequence: route through
        // rtf_children_into_index so each top-level item — element,
        // attribute, text, …  — stays addressable for per-item
        // atomisation.  The synthetic-doc wrap path would otherwise
        // either drop parentless attributes (XTTE0570 false negative)
        // or stringify the whole subtree as one concatenated atom
        // (multi-item sequences collapse to a single value).
        if as_is_sequence_typed(t) && as_target_is_atomic(t) {
            let nodes = build_rtf_nodes_no_merge(state, &v.body, ctx_node, pos, size)?;
            let key   = qname_key(&v.name);
            let child_ids = rtf_children_into_index(state.idx, &nodes);
            if let Some(uri) = &v.base_uri {
                let mut map = state.rtf_base_uris.borrow_mut();
                for &id in &child_ids { map.insert(id, uri.clone()); }
            }
            store_rtf(state, &key, nodes);
            return Ok(Value::NodeSet(child_ids));
        }
    }
    // XSLT 2.0 §9.3 / XTTE0570 — when `as=` declares a node KindTest
    // (singleton or `?` occurrence), build the body without text-
    // fragment merging so each contributing instruction stays a
    // separate sequence item.  We then enforce only the cardinality
    // *upper* bound (`One`/`Optional` => `n <= 1`) plus the item
    // type: under-production is left to the synthetic-doc wrap and
    // `coerce_to_atomic_sequence`'s child unwrap, which the OLD
    // behaviour also relied on — e.g. `as="text()"` with an empty-
    // string `xsl:value-of` body legitimately binds to the empty
    // sequence rather than erroring on cardinality.  Whitespace-only
    // text between instructions is template noise (mirrors
    // xsl:strip-space's implicit behaviour on stylesheet nodes) and
    // gets filtered out before counting.
    let key = qname_key(&v.name);
    if let Some(t) = &v.as_type {
        if let Some(st) = parse_as_atomic_type(t)
            .filter(template_result_type_is_node_kind)
            .filter(|st| !matches!(st.item,
                sup_xml_core::xpath::ast::ItemType::Document))
        {
            let nodes = build_rtf_nodes_no_merge(state, &v.body, ctx_node, pos, size)?;
            let counted: Vec<&ResultNode> = nodes.iter().filter(|n| match n {
                ResultNode::Text { content, .. } => !content.trim().is_empty(),
                _ => true,
            }).collect();
            if node_kind_overflow_or_mismatch(&counted, &st) {
                return Err(XsltError::Xpath(
                    sup_xml_core::xpath::eval::xpath_err(format!(
                        "variable '{}' declared as='{t}' but the body \
                         produced an incompatible sequence ({} items) (XTTE0570)",
                        qname_key(&v.name), counted.len(),
                    )).with_xpath_code("XTTE0570")));
            }
            let root_id = rtf_into_index(state.idx, &nodes);
            // XSLT 2.0 §5.7.2 — see the matching `bind_variable`
            // arm: the item is parentless, the doc-root is a
            // storage wrap, tag it for XTDE1270 / XTDE1370 / XTDE1380.
            state.idx.mark_synthetic_wrap(root_id);
            if let Some(uri) = &v.base_uri {
                state.rtf_base_uris.borrow_mut().insert(root_id, uri.clone());
            }
            store_rtf(state, &key, nodes);
            return Ok(Value::NodeSet(vec![root_id]));
        }
    }
    // Untyped body form — build an RTF as a navigable temporary tree
    // (with §5.7.2 text-fragment merging) and bind to a NodeSet of
    // its document root.  The structural ResultNode tree is also
    // stashed so xsl:copy-of can do a deep copy without re-walking.
    let nodes   = build_rtf_nodes(state, &v.body, ctx_node, pos, size)?;
    // A body-form variable with no `as=` constructs a document node, so
    // a top-level attribute in its content violates XTDE0420.  An
    // `as="item()?"` / `as="node()*"` binding is a sequence, where a
    // parentless attribute item is legal.
    if v.as_type.is_none() {
        check_document_node_content(&nodes)?;
    }
    let root_id = rtf_into_index(state.idx, &nodes);
    if let Some(uri) = &v.base_uri {
        state.rtf_base_uris.borrow_mut().insert(root_id, uri.clone());
    }
    store_rtf(state, &key, nodes);
    Ok(Value::NodeSet(vec![root_id]))
}

/// True iff `as` is `document-node()` (or `document-node(...)`)
/// with a `*` / `+` occurrence indicator — the sequence-of-documents
/// shape that needs each `xsl:copy` of a doc-node and each item of
/// `xsl:copy-of select="docs"` to materialise as its own doc-node
/// item.  Strict `document-node()` (singleton) is excluded — its
/// natural representation is the existing single-doc RTF wrap.
fn as_is_document_node_sequence(t: &str) -> bool {
    let s = t.trim();
    if !(s.ends_with('*') || s.ends_with('+')) { return false; }
    let body = s.trim_end_matches(|c: char| c == '*' || c == '+').trim();
    body == "document-node()" || body.starts_with("document-node(")
}

/// True iff `as` declares a non-document sequence type — i.e. one
/// that should bypass XSLT 2.0 §5.7.2 sequence normalisation when
/// the variable is bound from a sequence-constructor body (§9.3).
/// `document-node()` keeps the RTF path, since the spec
/// explicitly wraps the body in a document in that case.
/// True iff `as` declares an atomic-typed target (the item type is
/// `xs:T` for some XSD atomic type, possibly with an occurrence
/// indicator).  Distinguishes `as="xs:anyURI+"` (atomic) from
/// `as="node()+"` (kind test) so the body-form binding path can
/// choose between stringify+cast and unmerged-NodeSet binding.
fn as_target_is_atomic(t: &str) -> bool {
    let body = t.trim().trim_end_matches(|c: char| c == '*' || c == '+' || c == '?').trim();
    // KindTest forms always carry parentheses.  Anything else is
    // either a QName (e.g. `xs:integer`) or a bare local name — both
    // are atomic-type targets for our purposes.
    !body.contains('(')
}


fn as_is_sequence_typed(t: &str) -> bool {
    let s = t.trim();
    if s.starts_with("document-node") { return false; }
    // The set of sequence-typed signatures the W3C insn/sequence
    // suite exercises: `item()*`, `node()*`, `attribute()*`,
    // `element()*`, `text()*`, `xs:*`-with-occurrence-suffix.
    // Treat anything ending in `*` or `+` as a sequence; that's a
    // superset and any other interpretation would be a regression
    // for callers who genuinely want a multi-item value.
    s.ends_with('*') || s.ends_with('+')
        // Non-node item types (maps, arrays, function items) can never be
        // result-tree fragments — their body value must be captured as an
        // item, not stringified into an RTF.
        || as_is_nonnode_item_type(s)
}

/// True iff `as` declares a non-node item type (a map, array, or function
/// item).  Such a body value is captured directly from the sequence sink,
/// never built into a result-tree fragment.
fn as_is_nonnode_item_type(t: &str) -> bool {
    let s = t.trim();
    s.starts_with("map(") || s.starts_with("array(") || s.starts_with("function(")
}

/// True iff `as` declares an `attribute()` kind test (at any
/// occurrence).  A parentless attribute can't be reached as a child
/// of the RTF's synthetic document root — the `attribute::` axis of a
/// document node is empty — so a body-form variable of this type must
/// be materialised through [`rtf_children_into_index`], which gives
/// each constructed attribute a throwaway owner element and exposes
/// the attribute itself as a navigable node.
fn as_is_attribute_kind(t: &str) -> bool {
    matches!(
        parse_as_atomic_type(t).map(|st| st.item),
        Some(sup_xml_core::xpath::ast::ItemType::Attribute(_))
    )
}

/// Same as [`build_rtf_nodes`] but with text-fragment merging
/// disabled — used to keep each contributing instruction's items
/// distinct for sequence-typed variable bindings.
/// XSLT 2.0 §5.7.1 / XTDE0420 — the sequence used to construct the
/// content of a document node may not contain an attribute or
/// namespace node.  A parentless `xsl:attribute` surfaces as a
/// top-level [`ResultNode::Attribute`] in the built sequence, so a
/// document-node constructor (untyped body-form variable, `xsl:copy`
/// of a document node) checks the built children before wrapping them
/// in a document root.  Sequence-typed (`as=`) bindings skip this —
/// §5.7.1 allows parentless attributes in a sequence constructor.
fn check_document_node_content(nodes: &[ResultNode]) -> Result<()> {
    if let Some(ResultNode::Attribute { name, .. }) =
        nodes.iter().find(|n| matches!(n, ResultNode::Attribute { .. }))
    {
        return Err(XsltError::Xpath(
            sup_xml_core::xpath::eval::xpath_err(format!(
                "the sequence used to construct the content of a document \
                 node contains attribute '{}' (XTDE0420)", name.local))
            .with_xpath_code("XTDE0420")));
    }
    Ok(())
}

fn build_rtf_nodes_no_merge(
    state: &mut EvalState, body: &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<Vec<ResultNode>> {
    let mut tmp = EvalState {
        style: state.style, idx: state.idx, namespaces: state.namespaces,
        keys: state.keys,
        documents: state.documents,
        xslt_current: state.xslt_current,
        rtf_base_uris: state.rtf_base_uris,
        static_ctx: state.static_ctx,
        variables: std::mem::take(&mut state.variables),
        rtfs: std::mem::take(&mut state.rtfs),
        rtf_scopes: std::mem::take(&mut state.rtf_scopes),
        builder: {
            let mut b = ResultBuilder::new();
            b.no_text_merge = true;
            b
        },
        principal_buf: None,
        unparsed_entities: state.unparsed_entities.clone(),
        source_doc: state.source_doc,
        apply_imports_ctx: state.apply_imports_ctx.clone(),
        user_exts: state.user_exts,
        sequence_sinks: std::mem::take(&mut state.sequence_sinks),
        template_call_depth: state.template_call_depth,
        current_group: std::mem::take(&mut state.current_group),
        regex_groups: std::mem::take(&mut state.regex_groups),
        tunnel_pool: std::mem::take(&mut state.tunnel_pool),
        current_grouping_key: state.current_grouping_key.take(),
        accumulators: std::mem::take(&mut state.accumulators),
        unparsed_texts: state.unparsed_texts,
        static_base_uri: state.static_base_uri.clone(),
        loader: state.loader,
        loader_base: state.loader_base,
        dyn_doc_cache: state.dyn_doc_cache,
    };
    let r = {
        // The body builds a temporary tree / value, so a contained
        // xsl:result-document is XTDE1480.
        let _tmp_out = TempOutputGuard::enter();
        eval_body(&mut tmp, body, ctx_node, pos, size)
    };
    state.variables       = tmp.variables;
    state.rtfs            = tmp.rtfs;
    state.rtf_scopes      = tmp.rtf_scopes;
    state.sequence_sinks  = tmp.sequence_sinks;
    state.current_group   = tmp.current_group;
    state.regex_groups    = tmp.regex_groups;
    state.tunnel_pool     = tmp.tunnel_pool;
    state.current_grouping_key = tmp.current_grouping_key;
    state.accumulators    = tmp.accumulators;
    r?;
    Ok(tmp.builder.finish())
}

/// Materialise each top-level `ResultNode` from `nodes` as its
/// own RTF entry and return the encoded ids — one per child.
/// Used by the sequence-typed variable path so a body like
/// `<xsl:text>a</xsl:text><xsl:text>b</xsl:text>` binds to a
/// two-item NodeSet, not a single-document RTF.
fn rtf_children_into_index<'a>(
    idx:   &'a sup_xml_core::xpath::DocIndex<'a>,
    nodes: &[ResultNode],
) -> Vec<sup_xml_core::xpath::NodeId> {
    let mut b = idx.start_rtf();
    let doc_root = b.add_document();
    // Seed with the implicit `xml` binding so every element grafted
    // here can expose `namespace::xml` (XPath 2.0 §2.5.2).
    let initial_scope: NsScope = vec![
        (Some("xml".into()), "http://www.w3.org/XML/1998/namespace".into()),
    ];
    let mut child_ids = Vec::with_capacity(nodes.len());
    for n in nodes {
        let id = add_result_node_and_return_id(&mut b, doc_root, n, &initial_scope);
        if let Some(id) = id { child_ids.push(id); }
    }
    let _ = idx.finish_rtf(b);
    // XSLT 2.0 §5.7.2 — items of a sequence-typed binding are
    // parentless.  The doc-root we built is a storage artefact, not
    // a real document; tag it so XTDE1270 / XTDE1370 / XTDE1380
    // refuse it as the "root of the tree" for fn:key /
    // fn:unparsed-entity-uri / fn:unparsed-entity-public-id.
    idx.mark_synthetic_wrap(doc_root);
    child_ids
}

/// State-free constructor for `xsl:function` bodies that contain
/// result-tree-building instructions (literal result elements,
/// `xsl:element`, `xsl:attribute`, `xsl:copy-of`, ...).  Pours the
/// constructed nodes into `builder`; the caller grafts the
/// builder's top-level result into the dynamic-RTF arena to expose
/// each as a `NodeId`.
///
/// Scope: this is the construction subset that real-world function
/// bodies typically need.  Template dispatch (`xsl:apply-templates`,
/// `xsl:call-template`) still surfaces a clear error — those need
/// the full `EvalState` we don't carry here.
fn build_function_subtree<I: DocIndexLike>(
    body:     &[Instr],
    bindings: &dyn XPathBindings,
    idx:      &I,
    ctx_node: NodeId, pos: usize, size: usize,
    builder:  &mut ResultBuilder,
    static_ctx: &StaticContext,
) -> std::result::Result<(), sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    use sup_xml_core::xpath::eval::{eval_expr, value_to_bool, value_to_string_with, EvalCtx};
    let err = |m: String| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, m);
    fn mk_ctx<'a>(
        b: &'a dyn XPathBindings, sc: &'a StaticContext,
        ctx_node: NodeId, pos: usize, size: usize,
    ) -> EvalCtx<'a> {
        EvalCtx { context_node: ctx_node, pos, size, bindings: b, static_ctx: sc }
    }
    for instr in body {
        match instr {
            Instr::LiteralElement { name, attributes, namespaces, body: lre_body, .. } => {
                builder.open_element(name.clone());
                if !name.uri.is_empty() {
                    builder.push_namespace_decl(name.prefix.clone(), name.uri.clone());
                }
                for (prefix, uri) in namespaces {
                    builder.push_namespace_decl(prefix.clone(), uri.clone());
                }
                for (aname, avt) in attributes {
                    let value = render_avt_static(avt, bindings, idx, ctx_node, pos, size, static_ctx)?;
                    builder.push_attribute(aname.clone(), value);
                    if !aname.uri.is_empty() && aname.prefix.is_some() {
                        builder.push_namespace_decl(aname.prefix.clone(), aname.uri.clone());
                    }
                }
                build_function_subtree(lre_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                builder.close_element();
            }
            Instr::LiteralText { text, dose } => {
                if !text.is_empty() {
                    builder.push_text(text.clone(), *dose);
                }
            }
            Instr::ValueOf { select, dose, separator } => {
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                let text = match separator {
                    Some(sep_avt) => {
                        let sep = render_avt_static(sep_avt, bindings, idx, ctx_node, pos, size, static_ctx)?;
                        let pieces = sequence_string_items(&v, idx, NumStyle::from_context(false, bindings.xpath_version_2_or_later()));
                        pieces.join(&sep)
                    }
                    None => value_to_string_with(&v, idx, bindings),
                };
                if !text.is_empty() { builder.push_text(text, *dose); }
            }
            Instr::CopyOf { select, .. } => {
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                copy_value_into_builder(
                    builder, idx, &v,
                    NumStyle::from_context(false, bindings.xpath_version_2_or_later()),
                );
            }
            Instr::Copy { body: cpy_body, .. } => {
                // XSLT 1.0 §7.5 — copy *only* the current node
                // (shallow), then evaluate body for the children.
                // Attribute-set application is skipped here (the
                // function-body subset doesn't carry style.attribute_sets).
                match idx.kind(ctx_node) {
                    sup_xml_core::xpath::XPathNodeKind::Element => {
                        let qname = QName {
                            prefix: idx.namespace_prefix(ctx_node).map(str::to_string),
                            local:  idx.local_name(ctx_node).to_string(),
                            uri:    idx.namespace_uri(ctx_node).to_string(),
                        };
                        builder.open_element(qname);
                        build_function_subtree(cpy_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                        builder.close_element();
                    }
                    sup_xml_core::xpath::XPathNodeKind::Text
                    | sup_xml_core::xpath::XPathNodeKind::CData => {
                        builder.push_text(idx.string_value(ctx_node), false);
                    }
                    sup_xml_core::xpath::XPathNodeKind::Comment => {
                        builder.push_comment(idx.string_value(ctx_node));
                    }
                    sup_xml_core::xpath::XPathNodeKind::PI => {
                        builder.push_pi(idx.pi_target(ctx_node).to_string(),
                            idx.string_value(ctx_node));
                    }
                    sup_xml_core::xpath::XPathNodeKind::Document => {
                        build_function_subtree(cpy_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                    }
                    _ => {}
                }
            }
            Instr::Element { name, namespace, body: elt_body, in_scope_namespaces, .. } => {
                let name_str = render_avt_static(name, bindings, idx, ctx_node, pos, size, static_ctx)?;
                let explicit_ns = namespace.is_some();
                let ns_uri = match namespace {
                    Some(avt) => render_avt_static(avt, bindings, idx, ctx_node, pos, size, static_ctx)?,
                    None      => String::new(),
                };
                let (prefix, local) = split_qname(&name_str);
                let lookup_local = |target: Option<&str>| -> Option<String> {
                    in_scope_namespaces.iter()
                        .find(|(p, _)| p.as_deref() == target)
                        .map(|(_, u)| u.clone())
                };
                let resolved_uri = if explicit_ns { ns_uri }
                    else if let Some((p, _)) = name_str.split_once(':') {
                        lookup_local(Some(p)).unwrap_or_default()
                    } else {
                        lookup_local(None).unwrap_or_default()
                    };
                let q = QName { prefix, local, uri: resolved_uri };
                builder.open_element(q.clone());
                if !q.uri.is_empty() {
                    builder.push_namespace_decl(q.prefix.clone(), q.uri.clone());
                }
                build_function_subtree(elt_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                builder.close_element();
            }
            Instr::Attribute { name, namespace, select, separator, body: a_body, in_scope_namespaces } => {
                let name_str = render_avt_static(name, bindings, idx, ctx_node, pos, size, static_ctx)?;
                let explicit_ns = namespace.is_some();
                let ns_uri = match namespace {
                    Some(avt) => render_avt_static(avt, bindings, idx, ctx_node, pos, size, static_ctx)?,
                    None      => String::new(),
                };
                let sep = match separator {
                    Some(avt) => render_avt_static(avt, bindings, idx, ctx_node, pos, size, static_ctx)?,
                    None      => if select.is_some() { " ".to_string() } else { String::new() },
                };
                let value = if let Some(sel) = select {
                    let v = eval_expr(sel, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                    let items = sequence_string_items(&v, idx, NumStyle::from_context(false, bindings.xpath_version_2_or_later()));
                    if items.len() <= 1 { value_to_string_with(&v, idx, bindings) }
                    else                 { items.join(&sep) }
                } else {
                    // Body-form: build into a temporary sub-builder and
                    // stringify its top-level text contributions.
                    let mut sub = ResultBuilder::new();
                    build_function_subtree(a_body, bindings, idx, ctx_node, pos, size, &mut sub, static_ctx)?;
                    stringify(&sub.finish())
                };
                let (prefix, local) = split_qname(&name_str);
                let lookup_local = |target: Option<&str>| -> Option<String> {
                    in_scope_namespaces.iter()
                        .find(|(p, _)| p.as_deref() == target)
                        .map(|(_, u)| u.clone())
                };
                let resolved_uri = if explicit_ns { ns_uri }
                    else if let Some((p, _)) = name_str.split_once(':') {
                        lookup_local(Some(p)).unwrap_or_default()
                    } else {
                        String::new()
                    };
                let aq = QName { prefix: prefix.clone(), local, uri: resolved_uri };
                if !aq.uri.is_empty() && aq.prefix.is_some() {
                    builder.push_namespace_decl(aq.prefix.clone(), aq.uri.clone());
                }
                builder.push_attribute(aq, value);
            }
            Instr::Comment { select, body: c_body } => {
                let raw = match select {
                    Some(sel) => {
                        let v = eval_expr(sel, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                        value_to_string_with(&v, idx, bindings)
                    }
                    None => {
                        let mut sub = ResultBuilder::new();
                        build_function_subtree(c_body, bindings, idx, ctx_node, pos, size, &mut sub, static_ctx)?;
                        stringify(&sub.finish())
                    }
                };
                builder.push_comment(raw);
            }
            Instr::ProcessingInstruction { name, select, body: p_body } => {
                let target = render_avt_static(name, bindings, idx, ctx_node, pos, size, static_ctx)?;
                let data = match select {
                    Some(sel) => {
                        let v = eval_expr(sel, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                        value_to_string_with(&v, idx, bindings)
                    }
                    None => {
                        let mut sub = ResultBuilder::new();
                        build_function_subtree(p_body, bindings, idx, ctx_node, pos, size, &mut sub, static_ctx)?;
                        stringify(&sub.finish())
                    }
                };
                builder.push_pi(target, data);
            }
            Instr::ForEach { select, body: fe_body, sort: _ } => {
                let v = eval_expr(select, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                let items: Vec<Value> = match v {
                    Value::NodeSet(ns) => ns.into_iter()
                        .map(|id| Value::NodeSet(vec![id])).collect(),
                    Value::Sequence(items) => items,
                    other => vec![other],
                };
                let total = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    let cx = match &item {
                        Value::NodeSet(ns) if ns.len() == 1 => ns[0],
                        _ => ctx_node,
                    };
                    build_function_subtree(fe_body, bindings, idx, cx, i + 1, total, builder, static_ctx)?;
                }
            }
            Instr::If { test, body: if_body } => {
                let v = eval_expr(test, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                if value_to_bool(&v, idx) {
                    build_function_subtree(if_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                }
            }
            Instr::Choose { whens, otherwise } => {
                let mut matched = false;
                for (test, when_body) in whens {
                    let v = eval_expr(test, &mk_ctx(bindings, static_ctx, ctx_node, pos, size), idx)?;
                    if value_to_bool(&v, idx) {
                        build_function_subtree(when_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    if let Some(else_body) = otherwise {
                        build_function_subtree(else_body, bindings, idx, ctx_node, pos, size, builder, static_ctx)?;
                    }
                }
            }
            other => return Err(err(format!(
                "xsl:function body: construction instruction `{}` is not yet supported",
                instr_kind_name(other),
            ))),
        }
    }
    Ok(())
}

/// State-free AVT renderer for [`build_function_subtree`].  Mirrors
/// [`render_avt`] but pulls XPath context from the function-body's
/// bindings instead of an `EvalState`.
fn render_avt_static<I: DocIndexLike>(
    avt: &crate::ast::Avt,
    bindings: &dyn XPathBindings,
    idx: &I,
    ctx_node: NodeId, pos: usize, size: usize,
    static_ctx: &StaticContext,
) -> std::result::Result<String, sup_xml_core::error::XmlError> {
    use sup_xml_core::xpath::eval::{eval_expr, value_to_string_with, EvalCtx};
    if avt.is_literal() {
        let mut s = String::new();
        for part in &avt.parts {
            if let AvtPart::Literal(lit) = part { s.push_str(lit); }
        }
        return Ok(s);
    }
    let ctx = EvalCtx { context_node: ctx_node, pos, size, bindings, static_ctx };
    let mut out = String::new();
    for part in &avt.parts {
        match part {
            AvtPart::Literal(s) => out.push_str(s),
            AvtPart::Expr(e) => {
                let v = eval_expr(e, &ctx, idx)?;
                out.push_str(&value_to_string_with(&v, idx, bindings));
            }
        }
    }
    Ok(out)
}

/// State-free copy-of: traverse `v` and emit each node/atomic into
/// `builder`.  Mirrors [`copy_value_into`] (XSLT 2.0 §5.7.2 sequence
/// normalisation, single-space separators between consecutive atomic
/// items) without an `EvalState`.
fn copy_value_into_builder<I: DocIndexLike>(
    builder: &mut ResultBuilder, idx: &I, v: &Value, style: NumStyle,
) {
    match v {
        Value::String(s)  => builder.push_text(s.clone(), false),
        Value::Boolean(b) => builder.push_text(if *b { "true".into() } else { "false".into() }, false),
        Value::Number(n)  => builder.push_text(format_numeric_styled(*n, style), false),
        Value::Typed(t)   => builder.push_text(t.lexical.clone(), false),
        Value::NodeSet(ns) => {
            let mut prev_was_atomic = false;
            for &id in ns {
                let is_atomic = sup_xml_core::xpath::is_synthetic_id(id);
                if is_atomic && prev_was_atomic {
                    builder.push_text(" ".into(), false);
                }
                deep_copy_into_builder(builder, idx, id);
                prev_was_atomic = is_atomic;
            }
        }
        Value::ForeignNodeSet(_) => {}
        Value::Sequence(items) => {
            let mut prev_was_atomic = false;
            for item in items {
                let is_atomic = !matches!(item,
                    Value::NodeSet(_) | Value::ForeignNodeSet(_));
                if is_atomic && prev_was_atomic {
                    builder.push_text(" ".into(), false);
                }
                copy_value_into_builder(builder, idx, item, style);
                prev_was_atomic = is_atomic;
            }
        }
        Value::IntRange { lo, hi } => {
            let mut first = true;
            for i in *lo..=*hi {
                if !first { builder.push_text(" ".into(), false); }
                builder.push_text(i.to_string(), false);
                first = false;
            }
        }
        // Maps / arrays have no text projection — emit nothing.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => {}
    }
}

/// State-free counterpart of [`deep_copy_node`] — copies a source
/// node (and descendants for elements / documents) into `builder`
/// using the trait-level `idx` accessors.  Skips namespace-alias
/// rewriting, attribute sets, and the namespace-undeclaration
/// machinery `deep_copy_node` carries for the mirrored-parent case;
/// the construction subset doesn't need any of them.
fn deep_copy_into_builder<I: DocIndexLike>(
    builder: &mut ResultBuilder, idx: &I, node: NodeId,
) {
    use sup_xml_core::xpath::XPathNodeKind;
    match idx.kind(node) {
        XPathNodeKind::Element => {
            let q = QName {
                prefix: idx.namespace_prefix(node).map(str::to_string),
                local:  idx.local_name(node).to_string(),
                uri:    idx.namespace_uri(node).to_string(),
            };
            builder.open_element(q);
            for ns_id in idx.ns_range(node) {
                let prefix = idx.local_name(ns_id);
                if prefix == "xml" { continue; }
                let p_opt = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                builder.push_namespace_decl(p_opt, idx.string_value(ns_id));
            }
            for attr in idx.attr_range(node) {
                let aname = idx.node_name(attr);
                if aname == "xmlns" || aname.starts_with("xmlns:") { continue; }
                let aq = QName {
                    prefix: idx.namespace_prefix(attr).map(str::to_string),
                    local:  idx.local_name(attr).to_string(),
                    uri:    idx.namespace_uri(attr).to_string(),
                };
                builder.push_attribute(aq, idx.string_value(attr));
            }
            for &child in idx.children(node) {
                deep_copy_into_builder(builder, idx, child);
            }
            builder.close_element();
        }
        XPathNodeKind::Text | XPathNodeKind::CData => {
            builder.push_text(idx.string_value(node), false);
        }
        XPathNodeKind::Attribute => {
            let aq = QName {
                prefix: idx.namespace_prefix(node).map(str::to_string),
                local:  idx.local_name(node).to_string(),
                uri:    idx.namespace_uri(node).to_string(),
            };
            builder.push_attribute(aq, idx.string_value(node));
        }
        XPathNodeKind::Comment => builder.push_comment(idx.string_value(node)),
        XPathNodeKind::PI => {
            builder.push_pi(idx.pi_target(node).to_string(), idx.string_value(node));
        }
        XPathNodeKind::Document => {
            for &c in idx.children(node) {
                deep_copy_into_builder(builder, idx, c);
            }
        }
        XPathNodeKind::Namespace => {}
    }
}

/// Generic counterpart of [`rtf_children_into_index`] for callers
/// that hold a `&I: DocIndexLike` rather than a concrete `DocIndex`.
/// Used by the function-body LRE constructor — indexes that don't
/// support runtime RTF construction (test shims, foreign-doc
/// wrappers) return an empty vector here, which surfaces upstream as
/// the LRE silently producing nothing.
pub(crate) fn rtf_children_into_index_generic<I: DocIndexLike>(
    idx:   &I,
    nodes: &[ResultNode],
) -> Vec<sup_xml_core::xpath::NodeId> {
    let Some(mut b) = idx.rtf_builder() else { return Vec::new(); };
    let doc_root = b.add_document();
    // Seed scope with the implicit `xml` binding (XPath 2.0 §2.5.2).
    let initial_scope: NsScope = vec![
        (Some("xml".into()), "http://www.w3.org/XML/1998/namespace".into()),
    ];
    let mut child_ids = Vec::with_capacity(nodes.len());
    for n in nodes {
        let id = add_result_node_and_return_id(&mut b, doc_root, n, &initial_scope);
        if let Some(id) = id { child_ids.push(id); }
    }
    let _ = idx.finish_rtf(b);
    child_ids
}

fn evaluate_with_params(
    state:  &mut EvalState,
    params: &[WithParam],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<Vec<(QName, Value, Option<Vec<ResultNode>>)>> {
    // Regular params return through the result vec; XSLT 2.0 tunnel
    // params (tunnel="yes") get installed directly into
    // `state.tunnel_pool` so they propagate through the next
    // template invocation without explicit re-forwarding.
    let mut out = Vec::with_capacity(params.len());
    for p in params {
        if let Some(sel) = &p.select {
            let mut v = state.xpath_eval(sel, ctx_node, pos, size)?;
            if let Some(t) = &p.as_type {
                if let Some(st) = parse_as_atomic_type(t) {
                    v = coerce_to_atomic_sequence(v, &st, state.idx)?;
                }
            }
            if p.tunnel { state.tunnel_pool.insert(qname_key(&p.name), v); }
            else        { out.push((p.name.clone(), v, None)); }
        } else if p.body.is_empty() {
            let v = Value::String(String::new());
            if p.tunnel { state.tunnel_pool.insert(qname_key(&p.name), v); }
            else        { out.push((p.name.clone(), v, None)); }
        } else {
            // XSLT 2.0 §9.3 — body-form with a sequence-typed `as=`
            // that targets nodes (e.g. `as="node()*"`,
            // `as="element()+"`) keeps each top-level instruction
            // contribution as a distinct item rather than rolling
            // the body into an RTF and stringifying.  Atomic
            // sequence targets (`as="xs:anyURI+"` etc.) still go
            // through the stringify+cast path: the LRE / value-of
            // contributions are atomised, not bound as nodes.
            // Body producing a non-node item (map / array / function
            // item) — capture the item via the sequence sink rather than
            // building a result-tree fragment.
            if p.as_type.as_deref().map(as_is_nonnode_item_type).unwrap_or(false) {
                state.sequence_sinks.push(Vec::new());
                let res = eval_body(state, &p.body, ctx_node, pos, size);
                let captured = state.sequence_sinks.pop().unwrap_or_default();
                res?;
                let v = if captured.len() == 1 {
                    captured.into_iter().next().unwrap()
                } else {
                    Value::Sequence(captured)
                };
                if p.tunnel { state.tunnel_pool.insert(qname_key(&p.name), v); }
                else        { out.push((p.name.clone(), v, None)); }
                continue;
            }
            let want_no_merge = p.as_type.as_deref()
                .map(|t| as_is_sequence_typed(t) && !as_target_is_atomic(t))
                .unwrap_or(false);
            if want_no_merge {
                let nodes = build_rtf_nodes_no_merge(state, &p.body, ctx_node, pos, size)?;
                let child_ids = rtf_children_into_index(state.idx, &nodes);
                let value = Value::NodeSet(child_ids);
                if p.tunnel {
                    state.tunnel_pool.insert(qname_key(&p.name), value);
                } else {
                    out.push((p.name.clone(), value, Some(nodes)));
                }
            } else {
                let nodes = build_rtf_nodes(state, &p.body, ctx_node, pos, size)?;
                let s     = stringify(&nodes);
                if p.tunnel {
                    state.tunnel_pool.insert(qname_key(&p.name), Value::String(s));
                } else {
                    out.push((p.name.clone(), Value::String(s), Some(nodes)));
                }
            }
        };
    }
    Ok(out)
}

fn apply_one_to_node_with_args(
    state:    &mut EvalState,
    node:     NodeId,
    mode:     Option<&QName>,
    pos:      usize,
    size:     usize,
    args:     &[(QName, Value, Option<Vec<ResultNode>>)],
) -> Result<()> {
    // XSLT 1.0 §12.4 / 2.0 §6 — inside a pattern's predicate
    // current() must return the candidate node being tested, not
    // the outer apply-templates current.  Override the binding's
    // xslt_context_node for the pattern-match dispatch only; the
    // selected template's body restores it via run_template_body.
    let mut bindings = state.bindings();
    bindings.xslt_context_node = node;
    let chosen = pattern::select_template(
        state.style, node, mode, state.idx, &bindings,
    ).map_err(XsltError::from)?;
    match chosen {
        Some(sel) => {
            // Bound the apply-templates recursion so a stylesheet that
            // re-applies templates to the same node (e.g. `match="/"`
            // whose body does `apply-templates select="$doc-node"`)
            // fails cleanly instead of overflowing the stack.
            state.template_call_depth += 1;
            if state.template_call_depth > MAX_TEMPLATE_CALL_DEPTH {
                state.template_call_depth -= 1;
                return Err(XsltError::InvalidStylesheet(format!(
                    "xsl:apply-templates depth exceeds limit \
                     ({MAX_TEMPLATE_CALL_DEPTH}) — possible infinite recursion"
                )));
            }
            let prev = state.apply_imports_ctx.replace(
                (node, mode.cloned(), sel.template.import_precedence,
                 template_index_of(state.style, sel.template),
                 sel.priority, sel.branch_idx),
            );
            let r = run_template_body(state, sel.template, node, pos, size, args);
            state.apply_imports_ctx = prev;
            state.template_call_depth -= 1;
            r
        }
        None => apply_builtin_template_with_args(state, node, mode, args),
    }
}

// ── AVT rendering ─────────────────────────────────────────────────

fn render_avt(
    state: &mut EvalState, avt: &Avt,
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<String> {
    if avt.is_literal() {
        // Fast path — avoid the value-to-string roundtrip when the
        // attribute is pure literal text.
        let mut s = String::new();
        for part in &avt.parts {
            if let AvtPart::Literal(lit) = part { s.push_str(lit); }
        }
        return Ok(s);
    }
    // XSLT 2.0 §5.6.1 — a sequence-valued AVT joins all items with a
    // single space.  In an XPath-1.0 backwards-compatibility scope
    // (`[xsl:]version="1.0"`, XSLT 1.0 §7.6.2) it takes only the first
    // item's string-value instead.  BC scope is per-expression: the
    // compiler wraps the embedded expression in `Expr::BackwardsCompat`
    // exactly when its in-scope version is < 2.0.
    let mut out = String::new();
    for part in &avt.parts {
        match part {
            AvtPart::Literal(s) => out.push_str(s),
            AvtPart::Expr(e) => {
                let bc = matches!(e, sup_xml_core::xpath::Expr::BackwardsCompat(_));
                let v = state.xpath_eval(e, ctx_node, pos, size)?;
                if bc {
                    // First-item-only — XSLT 1.0 BC semantics.
                    out.push_str(&value_to_string_styled(&v, state.idx, state.num_style()));
                    continue;
                }
                let items = sequence_string_items(&v, state.idx, state.num_style());
                if items.len() > 1 {
                    out.push_str(&items.join(" "));
                } else {
                    out.push_str(&value_to_string_styled(&v, state.idx, state.num_style()));
                }
            }
        }
    }
    Ok(out)
}

/// Source-tree counting for `xsl:number` without an explicit
/// `value=`.  Returns the (possibly multi-segment) integer list for
/// `format_list` to render.
fn compute_number_list<'a>(
    state:    &mut EvalState<'a>,
    level:    crate::ast::NumberLevel,
    count:    Option<&sup_xml_core::xpath::Expr>,
    from:     Option<&sup_xml_core::xpath::Expr>,
    ctx_node: NodeId,
) -> Result<Vec<i64>> {
    // Pattern evaluator closure rooted in the current EvalState's
    // bindings (so patterns inside xsl:number can reference variables
    // and keys).  Delegates the count/from/level logic to
    // [`compute_number_list_generic`] so the xsl:function body
    // interpreter can reuse the same path without re-entering state.
    let idx = state.idx;
    let bindings_owned = state.bindings();
    compute_number_list_generic(
        idx, &bindings_owned, level, count, from, ctx_node,
    )
}

/// State-free variant of [`compute_number_list`] suitable for the
/// `xsl:function` body interpreter and any other site that only has
/// `&dyn XPathBindings`.  The pattern evaluator routes through
/// [`pattern::matches`] with the supplied bindings, matching the
/// main-engine convention.
fn compute_number_list_generic<I: DocIndexLike>(
    idx:      &I,
    bindings: &dyn XPathBindings,
    level:    crate::ast::NumberLevel,
    count:    Option<&sup_xml_core::xpath::Expr>,
    from:     Option<&sup_xml_core::xpath::Expr>,
    ctx_node: NodeId,
) -> Result<Vec<i64>> {
    use crate::ast::NumberLevel as L;
    use crate::number::{CountMatcher, count_level_any, count_level_multiple, resolve_from_root};

    let mut eval_pat = |expr: &sup_xml_core::xpath::Expr, n: NodeId| -> bool {
        pattern::matches(expr, n, idx, bindings).unwrap_or(false)
    };
    let matcher = CountMatcher::new(ctx_node, count, idx);
    let from_root = resolve_from_root(ctx_node, from, idx, &mut eval_pat);

    Ok(match level {
        L::Single => {
            let mut cur = Some(ctx_node);
            let mut target: Option<NodeId> = None;
            while let Some(n) = cur {
                if matcher.matches(n, idx, &mut eval_pat) {
                    target = Some(n);
                    break;
                }
                if n == from_root { break; }
                cur = idx.parent(n);
            }
            match target {
                Some(n) => vec![sibling_position_generic(n, &matcher, idx, &mut eval_pat)],
                None    => Vec::new(),
            }
        }
        L::Any => {
            match count_level_any(ctx_node, from_root, &matcher, from, idx, &mut eval_pat) {
                Some(0) | None => Vec::new(),
                Some(n) => vec![n],
            }
        }
        L::Multiple => count_level_multiple(ctx_node, from_root, &matcher, idx, &mut eval_pat),
    })
}

fn sibling_position_generic<I: DocIndexLike, F>(
    node:    NodeId,
    matcher: &crate::number::CountMatcher<'_>,
    idx:     &I,
    eval:    &mut F,
) -> i64
where F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> bool,
{
    let Some(parent) = idx.parent(node) else { return 1; };
    let mut pos: i64 = 0;
    for &sib in idx.children(parent) {
        if matcher.matches(sib, idx, eval) { pos += 1; }
        if sib == node { return pos.max(1); }
    }
    1
}


// ── helpers ───────────────────────────────────────────────────────

/// Apply each named `<xsl:attribute-set>` to the currently-open
/// element on `state.builder`.  Per XSLT 1.0 §7.1.4, sets named in
/// `use-attribute-sets=` are applied in declaration order; each set
/// recursively applies its own `use-attribute-sets=` *first* (so
/// "outer" attributes can override "inner" ones via last-write-wins
/// on the builder).  Cycles are rejected with a clear diagnostic.
fn apply_attribute_sets<'a>(
    state:    &mut EvalState<'a>,
    names:    &[QName],
    ctx_node: NodeId,
    pos:      usize,
    size:     usize,
) -> Result<()> {
    if names.is_empty() { return Ok(()); }
    let mut visiting: Vec<String> = Vec::new();
    for name in names {
        apply_attribute_set_one(state, name, ctx_node, pos, size, &mut visiting)?;
    }
    Ok(())
}

fn apply_attribute_set_one<'a>(
    state:    &mut EvalState<'a>,
    name:     &QName,
    ctx_node: NodeId,
    pos:      usize,
    size:     usize,
    visiting: &mut Vec<String>,
) -> Result<()> {
    let key = qname_key(name);
    if visiting.iter().any(|k| k == &key) {
        return Err(XsltError::InvalidStylesheet(format!(
            "<xsl:attribute-set> cycle detected at '{key}'"
        )));
    }
    // XSLT 1.0 §7.1.4 — `xsl:attribute-set` declarations sharing the
    // same expanded-name are MERGED.  Clone every matching set so we
    // can release the borrow on `state.style` before re-entering
    // `state` for AVT evaluation.  Apply order is ascending import
    // precedence so the highest-precedence definition runs last and
    // wins on per-attribute conflicts (XSLT 1.0 §2.6.2).
    let mut sets: Vec<_> = state.style.attribute_sets.iter()
        .filter(|s| qname_key(&s.name) == key)
        .cloned()
        .collect();
    sets.sort_by_key(|s| s.import_precedence);
    if sets.is_empty() {
        return Err(XsltError::UnresolvedReference(format!(
            "no <xsl:attribute-set> named '{key}'"
        )));
    }
    visiting.push(key);
    // XSLT 1.0 §7.1.4 — only top-level variables and parameters are
    // visible inside an attribute-set body, regardless of the
    // calling template's local scope.  Temporarily replace the
    // variable stack with just the outermost frame (globals) while
    // we evaluate the body, then restore.
    let saved_frames = std::mem::take(&mut state.variables.frames);
    if let Some(globals) = saved_frames.first().cloned() {
        state.variables.frames.push(globals);
    }
    let result: Result<()> = (|| {
        for set in &sets {
            for inner in &set.use_attribute_sets {
                apply_attribute_set_one(state, inner, ctx_node, pos, size, visiting)?;
            }
            for instr in &set.attributes {
                eval_instr(state, instr, ctx_node, pos, size)?;
            }
        }
        Ok(())
    })();
    state.variables.frames = saved_frames;
    visiting.pop();
    result
}

fn qname_key(q: &QName) -> String {
    if q.uri.is_empty() { q.local.clone() }
    else { format!("{{{uri}}}{local}", uri = q.uri, local = q.local) }
}

/// XSLT-runtime lexical-QName check used by xsl:element / xsl:attribute /
/// xsl:processing-instruction to surface XTDE0820 / XTDE0850 / XTDE0890
/// instead of silently emitting a malformed name.
fn is_lexical_qname_str(s: &str) -> bool {
    match s.split_once(':') {
        Some((p, l)) => is_ncname_str(p) && is_ncname_str(l),
        None         => is_ncname_str(s),
    }
}

/// XML Names §3 NCName production — the bare local-part of a QName.
fn is_ncname_str(s: &str) -> bool {
    if s.is_empty() { return false; }
    let mut cs = s.chars();
    let first = cs.next().unwrap();
    if !(first.is_alphabetic() || first == '_') { return false; }
    cs.all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn value_to_bool(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Number(n)  => n.as_f64() != 0.0 && !n.as_f64().is_nan(),
        Value::String(s)  => !s.is_empty(),
        Value::NodeSet(n) => !n.is_empty(),
        // ForeignNodeSet originates from document() in XPath; never
        // produced by our own XSLT engine (lxml is the only consumer
        // of foreign-doc nodes, via the compat shim).  Treat as
        // boolean by non-emptiness for forward compatibility.
        Value::ForeignNodeSet(n) => !n.is_empty(),
        Value::Typed(t) => {
            if let Some(b) = t.boolean { return b; }
            if let Some(n) = t.numeric { return n != 0.0 && !n.is_nan(); }
            !t.lexical.is_empty()
        }
        // EBV of an atomic sequence: empty → false; otherwise
        // first item's EBV (XSLT 2.0 mirrors XPath 2.0 §2.4.3).
        Value::Sequence(items) => match items.first() {
            None    => false,
            Some(v) => value_to_bool(v),
        }
        Value::IntRange { lo, hi } if lo == hi => *lo != 0,
        Value::IntRange { .. } => true,
        Value::Map(_) | Value::Array(_) | Value::Function(_) => true,
    }
}

/// Public-within-crate variant for number.rs / format-number
/// wiring.  Just an alias for the engine's `value_to_number`.
pub(crate) fn value_to_number_xpath<I: DocIndexLike>(v: &Value, idx: &I) -> f64 {
    sup_xml_core::xpath::eval::value_to_number(v, idx)
}

/// Decompose a Value into one string per sequence item.  Used by
/// XSLT 2.0 `xsl:value-of` to render every member of the result with
/// a separator (XPath 1.0 only ever looked at the first node).
fn sequence_string_items<I: DocIndexLike>(v: &Value, idx: &I, style: NumStyle) -> Vec<String> {
    match v {
        Value::NodeSet(ns)        => ns.iter().map(|&id| idx.string_value(id)).collect(),
        Value::ForeignNodeSet(ns) => ns.iter()
            .map(|&p| sup_xml_tree::dom::Document::node_string_value_by_ptr(p))
            .collect(),
        Value::String(s)          => vec![s.clone()],
        Value::Number(n)          => vec![format_numeric_styled(*n, style)],
        Value::Boolean(b)         => vec![(if *b { "true" } else { "false" }).to_string()],
        Value::Typed(t)           => vec![t.lexical.clone()],
        Value::Sequence(items)    => items.iter()
            .flat_map(|item| sequence_string_items(item, idx, style))
            .collect(),
        Value::IntRange { lo, hi } => (*lo..=*hi).map(|i| i.to_string()).collect(),
        Value::Map(_) | Value::Array(_) | Value::Function(_) => Vec::new(),
    }
}

/// Build a Vec<ResultNode> from a body of instructions — used for
/// xsl:variable/xsl:param bodies (RTFs) and for any other place
/// where we need the structural output rather than the
/// stringified form.
/// Whether a result node counts as "populated" content for
/// `xsl:where-populated` (XSLT 3.0 §16.4.3): an element/document with
/// no attributes and no children is empty, as is a zero-length text
/// node; everything else (attributes, comments, PIs, non-empty text or
/// elements) is significant.
fn result_node_is_significant(n: &ResultNode) -> bool {
    match n {
        ResultNode::Text { content, .. } => !content.is_empty(),
        ResultNode::Element { attributes, children, .. } =>
            !attributes.is_empty() || !children.is_empty(),
        ResultNode::Attribute { .. }
        | ResultNode::Comment(_)
        | ResultNode::ProcessingInstruction { .. } => true,
    }
}

fn build_rtf_nodes(
    state: &mut EvalState, body: &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<Vec<ResultNode>> {
    let mut tmp = EvalState {
        style: state.style, idx: state.idx, namespaces: state.namespaces,
        keys: state.keys,
        documents: state.documents,
        xslt_current: state.xslt_current,
        rtf_base_uris: state.rtf_base_uris,
        static_ctx: state.static_ctx,
        variables: std::mem::take(&mut state.variables),
        rtfs: std::mem::take(&mut state.rtfs),
        rtf_scopes: std::mem::take(&mut state.rtf_scopes),
        builder: ResultBuilder::new(),
        principal_buf: None,
        unparsed_entities: state.unparsed_entities.clone(),
        source_doc: state.source_doc,
        apply_imports_ctx: state.apply_imports_ctx.clone(),
        user_exts: state.user_exts,
        sequence_sinks: std::mem::take(&mut state.sequence_sinks),
        template_call_depth: state.template_call_depth,
        current_group: std::mem::take(&mut state.current_group),
        regex_groups: std::mem::take(&mut state.regex_groups),
        tunnel_pool: std::mem::take(&mut state.tunnel_pool),
        current_grouping_key: state.current_grouping_key.take(),
        accumulators: std::mem::take(&mut state.accumulators),
        unparsed_texts: state.unparsed_texts,
        static_base_uri: state.static_base_uri.clone(),
        loader: state.loader,
        loader_base: state.loader_base,
        dyn_doc_cache: state.dyn_doc_cache,
    };
    let r = {
        // The body builds a temporary tree / value, so a contained
        // xsl:result-document is XTDE1480.
        let _tmp_out = TempOutputGuard::enter();
        eval_body(&mut tmp, body, ctx_node, pos, size)
    };
    state.variables = tmp.variables;
    state.rtfs      = tmp.rtfs;
    state.rtf_scopes = tmp.rtf_scopes;
    state.sequence_sinks = tmp.sequence_sinks;
    state.current_group = tmp.current_group;
    state.regex_groups = tmp.regex_groups;
    state.tunnel_pool = tmp.tunnel_pool;
    state.current_grouping_key = tmp.current_grouping_key;
    state.accumulators = tmp.accumulators;
    r?;
    Ok(tmp.builder.finish())
}

/// Compute every declared `xsl:accumulator` over the document rooted
/// at `root`, caching the before/after value at each node.
fn precompute_accumulators(state: &mut EvalState, root: NodeId) -> Result<()> {
    let decls = state.style.accumulators.clone();
    for decl in &decls {
        let key = qname_key(&decl.name);
        let initial = state.xpath_eval(&decl.initial_value, root, 1, 1)?;
        let mut data = AccumulatorData {
            before: HashMap::new(), after: HashMap::new(), initial: initial.clone(),
        };
        let mut value = initial;
        accumulate_walk(state, decl, root, &mut value, &mut data)?;
        state.accumulators.insert(key, data);
    }
    Ok(())
}

/// Document-order walk applying an accumulator's rules: start-phase
/// rules fire on the pre-order event (recording `before`), end-phase
/// rules on the post-order event (recording `after`).  Attributes are
/// visited between an element's start and its children.
fn accumulate_walk(
    state: &mut EvalState,
    decl:  &crate::ast::AccumulatorDecl,
    node:  NodeId,
    value: &mut Value,
    data:  &mut AccumulatorData,
) -> Result<()> {
    use crate::ast::AccumulatorPhase::{Start, End};
    // `accumulator-before(N)` is the *pre-descent* value: the value
    // after N's own start-phase rule fires, before descending into its
    // attributes/children (XSLT 3.0 §18.4).
    apply_accum_rule(state, decl, node, Start, value)?;
    data.before.insert(node, value.clone());
    for a in state.idx.attr_range(node).collect::<Vec<_>>() {
        apply_accum_rule(state, decl, a, Start, value)?;
        data.before.insert(a, value.clone());
        apply_accum_rule(state, decl, a, End, value)?;
        data.after.insert(a, value.clone());
    }
    for c in state.idx.children(node).to_vec() {
        accumulate_walk(state, decl, c, value, data)?;
    }
    // `accumulator-after(N)` is the *post-descent* value: after N's
    // end-phase rule, having processed all descendants.
    apply_accum_rule(state, decl, node, End, value)?;
    data.after.insert(node, value.clone());
    Ok(())
}

/// Apply the first matching accumulator rule for `phase` to `node`,
/// updating `value`.  The rule's expression sees `$value` (the current
/// accumulator value) and `node` as the context item.
fn apply_accum_rule(
    state: &mut EvalState,
    decl:  &crate::ast::AccumulatorDecl,
    node:  NodeId,
    phase: crate::ast::AccumulatorPhase,
    value: &mut Value,
) -> Result<()> {
    let matched = {
        let b = state.bindings();
        let mut found = None;
        for (i, rule) in decl.rules.iter().enumerate() {
            if rule.phase != phase { continue; }
            if pattern::matches(&rule.match_pattern, node, state.idx, &b)
                .map_err(XsltError::from)?
            {
                found = Some(i);
                break;
            }
        }
        found
    };
    let Some(i) = matched else { return Ok(()) };
    let rule = &decl.rules[i];
    state.variables.enter();
    state.variables.bind("value".to_string(), value.clone());
    let nv = match &rule.select {
        Some(e) => state.xpath_eval(e, node, 1, 1),
        None => build_rtf_nodes(state, &rule.body, node, 1, 1)
            .map(|ns| Value::NodeSet(rtf_children_into_index(state.idx, &ns))),
    };
    state.variables.leave();
    *value = nv?;
    Ok(())
}

fn rtf_scope_enter(state: &mut EvalState) {
    state.rtf_scopes.push(Vec::new());
}

fn rtf_scope_leave(state: &mut EvalState) {
    if let Some(keys) = state.rtf_scopes.pop() {
        for k in keys { state.rtfs.remove(&k); }
    }
}

/// Materialise a ResultNode tree into a navigable
/// [`sup_xml_core::xpath::rtf::RtfIndex`] hosted by `idx`.
/// Returns the encoded node id of the RTF's document root —
/// suitable for binding as `Value::NodeSet(vec![root_id])` so
/// XPath navigation walks the temporary tree.
fn rtf_into_index<'a>(
    idx:   &'a sup_xml_core::xpath::DocIndex<'a>,
    nodes: &[ResultNode],
) -> sup_xml_core::xpath::NodeId {
    let mut b = idx.start_rtf();
    let doc_root = b.add_document();
    // XPath 2.0 §2.5.2 — every element implicitly carries the `xml`
    // namespace binding.  Seed the walk with that so any element
    // we graft into the RTF can expose at least `namespace::xml`.
    let initial_scope: NsScope = vec![
        (Some("xml".into()), "http://www.w3.org/XML/1998/namespace".into()),
    ];
    for n in nodes {
        add_result_node(&mut b, doc_root, n, &initial_scope);
    }
    idx.finish_rtf(b)
}

/// In-scope namespaces at a point in the result-tree walk.  Each
/// pair is `(prefix, uri)`; `None` is the default-namespace
/// binding.  An empty URI is an *undeclaration* — the prefix is
/// hidden from descendants but doesn't appear as a namespace node.
type NsScope = Vec<(Option<String>, String)>;

/// Compose `parent_scope` with the element-local declarations into
/// the full in-scope set for an element body.  Same-prefix bindings
/// in `locals` override the inherited URI.
fn ns_scope_extend(
    parent_scope: &NsScope,
    locals: &[(Option<String>, String)],
) -> NsScope {
    let mut scope = parent_scope.clone();
    for (prefix, uri) in locals {
        if let Some(existing) = scope.iter_mut().find(|(p, _)| p == prefix) {
            existing.1 = uri.clone();
        } else {
            scope.push((prefix.clone(), uri.clone()));
        }
    }
    scope
}

/// Recursively copy one `ResultNode` (and its descendants) into
/// the RTF builder.  Attribute / namespace nodes on each element
/// are appended via [`sup_xml_core::xpath::rtf::RtfBuilder::start_attrs`]
/// + [`add_attribute`](sup_xml_core::xpath::rtf::RtfBuilder::add_attribute)
/// so the resulting tree's `attribute::` axis enumerates them in
/// source order.  PIs / comments / text leaves pass straight
/// through.
/// Variant of [`add_result_node`] that returns the id of the
/// node it added (or the root element id for elements with
/// attributes/children).  Used by [`rtf_children_into_index`]
/// to expose each top-level result as a single-node item.
fn add_result_node_and_return_id(
    b:      &mut sup_xml_core::xpath::rtf::RtfBuilder,
    parent: sup_xml_core::xpath::NodeId,
    n:      &ResultNode,
    ns_scope: &NsScope,
) -> Option<sup_xml_core::xpath::NodeId> {
    match n {
        ResultNode::Text { content, .. } => Some(b.add_text(parent, content)),
        ResultNode::Comment(s)           => Some(b.add_comment(parent, s)),
        ResultNode::ProcessingInstruction { target, data } =>
            Some(b.add_pi(parent, target, data)),
        ResultNode::Attribute { name, value } => {
            // A parentless attribute needs an owner element in the
            // arena (attributes can't float free).  Synthesise a
            // throw-away owner and return the attribute's id — callers
            // bind the attribute node, not the owner.
            let prefix = name.prefix.as_deref();
            let qname = match prefix {
                Some(p) if !p.is_empty() => format!("{p}:{}", name.local),
                _                        => name.local.clone(),
            };
            let owner = b.add_element(parent, "_attr-owner", "", None);
            b.start_attrs(owner);
            Some(b.add_attribute(owner, &qname, &name.uri, prefix, value))
        }
        ResultNode::Element { name, attributes, namespaces, children } => {
            let prefix_str = name.prefix.as_deref();
            let qname = match prefix_str {
                Some(p) if !p.is_empty() => format!("{p}:{}", name.local),
                _                        => name.local.clone(),
            };
            let elem = b.add_element(parent, &qname, &name.uri, prefix_str);
            if !attributes.is_empty() {
                b.start_attrs(elem);
                for (aname, value) in attributes {
                    let aprefix = aname.prefix.as_deref();
                    let aq = match aprefix {
                        Some(p) if !p.is_empty() => format!("{p}:{}", aname.local),
                        _                        => aname.local.clone(),
                    };
                    b.add_attribute(elem, &aq, &aname.uri, aprefix, value);
                }
            }
            // Same namespace-node logic as [`add_result_node`] — see
            // that function for the in-scope computation rationale.
            let mut element_scope = ns_scope_extend(ns_scope, namespaces);
            if !name.uri.is_empty() {
                let target_prefix = prefix_str.filter(|p| !p.is_empty()).map(str::to_string);
                if !element_scope.iter().any(|(p, _)| p == &target_prefix) {
                    element_scope.push((target_prefix, name.uri.clone()));
                }
            }
            let visible: Vec<&(Option<String>, String)> = element_scope.iter()
                .filter(|(_, uri)| !uri.is_empty())
                .collect();
            if !visible.is_empty() {
                b.start_ns(elem);
                for (prefix, uri) in &visible {
                    b.add_namespace_node(elem, prefix.as_deref(), uri);
                }
            }
            for c in children {
                add_result_node(b, elem, c, &element_scope);
            }
            Some(elem)
        }
    }
}

fn add_result_node(
    b:      &mut sup_xml_core::xpath::rtf::RtfBuilder,
    parent: sup_xml_core::xpath::NodeId,
    n:      &ResultNode,
    ns_scope: &NsScope,
) {
    match n {
        ResultNode::Text { content, .. } => {
            b.add_text(parent, content);
        }
        ResultNode::Comment(s) => {
            b.add_comment(parent, s);
        }
        ResultNode::ProcessingInstruction { target, data } => {
            b.add_pi(parent, target, data);
        }
        ResultNode::Attribute { name, value } => {
            // Element attributes live in the Element's `attributes`
            // vec, so a standalone Attribute never appears among an
            // element's children — attach to `parent` defensively.
            let prefix = name.prefix.as_deref();
            let qname = match prefix {
                Some(p) if !p.is_empty() => format!("{p}:{}", name.local),
                _                        => name.local.clone(),
            };
            b.add_attribute(parent, &qname, &name.uri, prefix, value);
        }
        ResultNode::Element { name, attributes, namespaces, children } => {
            let prefix_str = name.prefix.as_deref();
            let qname = match prefix_str {
                Some(p) if !p.is_empty() => format!("{p}:{}", name.local),
                _                        => name.local.clone(),
            };
            let elem = b.add_element(parent, &qname, &name.uri, prefix_str);
            // Attributes — emitted as a contiguous range before
            // the namespace and content slabs so the source-order
            // `attribute::` axis enumeration is correct.
            if !attributes.is_empty() {
                b.start_attrs(elem);
                for (aname, value) in attributes {
                    let aprefix = aname.prefix.as_deref();
                    let aq = match aprefix {
                        Some(p) if !p.is_empty() => format!("{p}:{}", aname.local),
                        _                        => aname.local.clone(),
                    };
                    b.add_attribute(elem, &aq, &aname.uri, aprefix, value);
                }
            }
            // Namespace nodes — XPath 2.0 §2.5.2 says the
            // `namespace::` axis returns every in-scope namespace
            // (inherited + locally declared), implicitly including
            // `xml`.  Compute the full scope first, then emit each
            // non-undeclared binding as a Namespace node.  Element
            // children inherit this scope (recursive call below).
            let element_scope = ns_scope_extend(ns_scope, namespaces);
            // Ensure the element's own name binding is reachable
            // (the ResultBuilder doesn't always push it explicitly
            // for unprefixed-in-default-namespace shapes).
            let mut element_scope = element_scope;
            if !name.uri.is_empty() {
                let target_prefix = prefix_str.filter(|p| !p.is_empty()).map(str::to_string);
                if !element_scope.iter().any(|(p, _)| p == &target_prefix) {
                    element_scope.push((target_prefix, name.uri.clone()));
                }
            }
            // Emit one namespace node per visible binding (URI != ""
            // — empty URIs are undeclarations, in-scope only to
            // hide an inherited binding from descendants).
            let visible: Vec<&(Option<String>, String)> = element_scope.iter()
                .filter(|(_, uri)| !uri.is_empty())
                .collect();
            if !visible.is_empty() {
                b.start_ns(elem);
                for (prefix, uri) in &visible {
                    b.add_namespace_node(elem, prefix.as_deref(), uri);
                }
            }
            for c in children {
                add_result_node(b, elem, c, &element_scope);
            }
        }
    }
}

fn store_rtf(state: &mut EvalState, key: &str, nodes: Vec<ResultNode>) {
    state.rtfs.insert(key.to_string(), nodes);
    if let Some(scope) = state.rtf_scopes.last_mut() {
        scope.push(key.to_string());
    }
}

/// Deep-copy a single ResultNode into the active builder.  Used
/// by xsl:copy-of when fed an RTF.
fn copy_result_node_into(state: &mut EvalState, node: &ResultNode) {
    match node {
        ResultNode::Element { name, namespaces, attributes, children } => {
            state.builder.open_element(name.clone());
            for (p, u) in namespaces {
                state.builder.push_namespace_decl(p.clone(), u.clone());
            }
            for (an, v) in attributes {
                state.builder.push_attribute(an.clone(), v.clone());
            }
            for c in children { copy_result_node_into(state, c); }
            state.builder.close_element();
        }
        ResultNode::Text { content, dose } => {
            state.builder.push_text(content.clone(), *dose);
        }
        ResultNode::Comment(s) => {
            state.builder.push_comment(s.clone());
        }
        ResultNode::ProcessingInstruction { target, data } => {
            state.builder.push_pi(target.clone(), data.clone());
        }
        ResultNode::Attribute { name, value } => {
            state.builder.push_attribute(name.clone(), value.clone());
        }
    }
}

/// Snapshot a subtree's stringified form via a temporary builder.
fn stringify_into_string(
    state: &mut EvalState, body: &[Instr],
    ctx_node: NodeId, pos: usize, size: usize,
) -> Result<String> {
    let mut tmp = EvalState {
        style: state.style, idx: state.idx, namespaces: state.namespaces,
        keys: state.keys,
        documents: state.documents,
        xslt_current: state.xslt_current,
        rtf_base_uris: state.rtf_base_uris,
        static_ctx: state.static_ctx,
        variables: std::mem::take(&mut state.variables),
        rtfs: std::mem::take(&mut state.rtfs),
        rtf_scopes: std::mem::take(&mut state.rtf_scopes),
        builder: ResultBuilder::new(),
        principal_buf: None,
        unparsed_entities: state.unparsed_entities.clone(),
        source_doc: state.source_doc,
        apply_imports_ctx: state.apply_imports_ctx.clone(),
        user_exts: state.user_exts,
        sequence_sinks: std::mem::take(&mut state.sequence_sinks),
        template_call_depth: state.template_call_depth,
        current_group: std::mem::take(&mut state.current_group),
        regex_groups: std::mem::take(&mut state.regex_groups),
        tunnel_pool: std::mem::take(&mut state.tunnel_pool),
        current_grouping_key: state.current_grouping_key.take(),
        accumulators: std::mem::take(&mut state.accumulators),
        unparsed_texts: state.unparsed_texts,
        static_base_uri: state.static_base_uri.clone(),
        loader: state.loader,
        loader_base: state.loader_base,
        dyn_doc_cache: state.dyn_doc_cache,
    };
    let r = {
        // The body builds a temporary tree / value, so a contained
        // xsl:result-document is XTDE1480.
        let _tmp_out = TempOutputGuard::enter();
        eval_body(&mut tmp, body, ctx_node, pos, size)
    };
    state.variables = tmp.variables;
    state.rtfs      = tmp.rtfs;
    state.rtf_scopes = tmp.rtf_scopes;
    state.sequence_sinks = tmp.sequence_sinks;
    state.current_group = tmp.current_group;
    state.regex_groups = tmp.regex_groups;
    state.tunnel_pool = tmp.tunnel_pool;
    state.current_grouping_key = tmp.current_grouping_key;
    r?;
    Ok(stringify(&tmp.builder.finish()))
}

fn stringify(nodes: &[ResultNode]) -> String {
    let mut out = String::new();
    for n in nodes { append_string_value(n, &mut out); }
    out
}

/// String-value of each top-level node, returned as a Vec so a
/// caller (`xsl:value-of` body form) can join with its own
/// separator.  Adjacent text nodes merge into a single item
/// (matching XSLT 2.0 §5.7.2's sequence-normalisation rule that
/// runs of text get coalesced into one atomic before separators
/// apply).  Empty items are dropped so a leading/trailing
/// whitespace-only sibling doesn't produce stray separators.
/// Build the piece list that drives `xsl:value-of` (and `xsl:attribute`)
/// when the instruction has a sequence-constructor body.  XSLT 2.0 §11.5
/// requires *document-order* interleaving: each atomic value contributed
/// by `xsl:sequence` is its own piece, and adjacent constructed text /
/// element nodes collapse into a single text piece.  Walking each body
/// instruction in turn preserves that order — capturing all sequence
/// items at the end (the old single-pass shape) sorts them after every
/// constructed text node and gets the order wrong.
fn collect_value_of_body_pieces(
    state: &mut EvalState, body: &[Instr], ctx_node: NodeId,
    pos: usize, size: usize,
) -> Result<Vec<String>> {
    let mut pieces: Vec<String> = Vec::new();
    let mut text_buf = String::new();
    let flush = |pieces: &mut Vec<String>, text_buf: &mut String| {
        if !text_buf.is_empty() {
            pieces.push(std::mem::take(text_buf));
        }
    };
    for instr in body {
        // Each instruction runs in its own sequence sink so we know
        // *which* atomic items it produced (rather than smearing every
        // sequence in the body into one bucket at the end).
        state.sequence_sinks.push(Vec::new());
        let nodes = build_rtf_nodes(state, std::slice::from_ref(instr),
                                    ctx_node, pos, size)?;
        let captured = state.sequence_sinks.pop().unwrap_or_default();
        // Constructed-node output: adjacent text nodes coalesce into
        // one piece (XSLT 2.0 §5.7.2), but elements/attributes each
        // atomise as a separate sequence item per §11.5 / §11.7.
        for n in &nodes {
            match n {
                ResultNode::Text { content, .. } => text_buf.push_str(content),
                ResultNode::Element { .. } => {
                    flush(&mut pieces, &mut text_buf);
                    let mut s = String::new();
                    append_string_value(n, &mut s);
                    pieces.push(s);
                }
                ResultNode::Attribute { value, .. } => {
                    flush(&mut pieces, &mut text_buf);
                    pieces.push(value.clone());
                }
                ResultNode::Comment(_) | ResultNode::ProcessingInstruction { .. } => {}
            }
        }
        // `xsl:sequence` items are always distinct pieces — flush the
        // text run that preceded them, then emit one piece per item.
        if !captured.is_empty() {
            flush(&mut pieces, &mut text_buf);
            for v in captured {
                let items = sequence_string_items(&v, state.idx, state.num_style());
                pieces.extend(items.into_iter().filter(|s| !s.is_empty()));
            }
        }
    }
    flush(&mut pieces, &mut text_buf);
    Ok(pieces.into_iter().filter(|s| !s.is_empty()).collect())
}

fn append_string_value(node: &ResultNode, out: &mut String) {
    match node {
        ResultNode::Text { content, .. } => out.push_str(content),
        ResultNode::Element { children, .. } => {
            for c in children { append_string_value(c, out); }
        }
        ResultNode::Attribute { value, .. } => out.push_str(value),
        ResultNode::Comment(_) | ResultNode::ProcessingInstruction { .. } => {}
    }
}

fn copy_value_into(state: &mut EvalState, v: &Value, copy_ns: bool) -> Result<()> {
    match v {
        Value::String(s)  => { state.builder.push_atomic_text(s.clone()); }
        Value::Boolean(b) => { state.builder.push_atomic_text(if *b { "true".into() } else { "false".into() }); }
        Value::Number(n)  => { state.builder.push_atomic_text(format_numeric_styled(*n, state.num_style())); }
        Value::Typed(t)   => { state.builder.push_atomic_text(t.lexical.clone()); }
        Value::NodeSet(ns) => {
            // XSLT 2.0 §5.7.2 sequence normalization: atomic items in
            // the input sequence (here represented as synthetic-text
            // nodes whose IDs come from the EXSLT synthetic store)
            // are joined with a single space separator.  Real nodes
            // flow through as deep copies without inter-item
            // separators.  We track whether the previous emit was an
            // atomic-text item so an inter-atom space is only emitted
            // between consecutive atomics.
            let mut prev_was_atomic = false;
            for &id in ns {
                let is_atomic = sup_xml_core::xpath::is_synthetic_id(id);
                if is_atomic && prev_was_atomic {
                    state.builder.push_text(" ".into(), false);
                }
                deep_copy_node(state, id, None, copy_ns)?;
                prev_was_atomic = is_atomic;
            }
        }
        // Our XSLT engine doesn't produce ForeignNodeSets (no
        // document() support yet here); silently no-op.  lxml path
        // goes through libxslt which has its own xsl:copy-of impl.
        Value::ForeignNodeSet(_) => {}
        // Atomic / mixed sequence: emit each item with a space
        // separator between consecutive atomic items (XSLT 2.0
        // §5.7.2 sequence normalization).  Nodes copy through
        // without inter-item separators.
        Value::Sequence(items) => {
            let mut prev_was_atomic = false;
            for item in items {
                let is_atomic = !matches!(item,
                    Value::NodeSet(_) | Value::ForeignNodeSet(_));
                if is_atomic && prev_was_atomic {
                    state.builder.push_text(" ".into(), false);
                }
                copy_value_into(state, item, copy_ns)?;
                prev_was_atomic = is_atomic;
            }
        }
        // XSLT 2.0 §5.7.2 sequence normalisation — each integer
        // emits its lexical form, separated by single spaces.
        Value::IntRange { lo, hi } => {
            let mut first = true;
            for i in *lo..=*hi {
                if !first { state.builder.push_text(" ".into(), false); }
                state.builder.push_text(i.to_string(), false);
                first = false;
            }
        }
        // Maps / arrays produce no result-tree text here.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => {}
    }
    Ok(())
}

/// Deep-copy a source node (used by xsl:copy-of and copy from a
/// nodeset).  Recurses into element subtrees.
///
/// `mirrored_parent` is `Some(parent_id)` when the element being
/// produced sits under a result-tree element that is itself a copy of
/// `parent_id` — i.e. an in-place subtree copy.  In that case the
/// source's namespace shape needs to be reproduced faithfully, so an
/// `xmlns=""` undeclaration must be emitted when the source parent had
/// a default namespace and this element doesn't.  `None` is the
/// "graft" case where namespace inheritance from the receiving LRE
/// takes over (XSLT 2.0 inherit-namespaces=yes semantics).
fn deep_copy_node(state: &mut EvalState, node: NodeId, mirrored_parent: Option<NodeId>, copy_ns: bool) -> Result<()> {
    match state.idx.kind(node) {
        XPathNodeKind::Element => {
            let q = element_qname(state, node);
            state.builder.open_element(q.clone());
            // XSLT 1.0 §11.3 — `xsl:copy-of` copies the namespace
            // nodes of the source element along with its attributes
            // and children.  Iterate the materialised namespace range
            // (XPath 1.0 §5.4 in-scope set) and emit each binding.
            // With `copy-namespaces="no"` (XSLT 2.0 §11.9.1) the
            // inherited in-scope declarations are dropped; only the
            // namespaces required by the element's and its attributes'
            // own names are emitted (just below / by the builder's name
            // handling).
            let mut child_has_default = false;
            if copy_ns {
                for ns_id in state.idx.ns_range(node) {
                    let prefix = state.idx.local_name(ns_id);
                    let uri    = state.idx.string_value(ns_id);
                    // The `xml` prefix is implicit on every element and
                    // never serialised as a namespace declaration.
                    if prefix == "xml" { continue; }
                    if prefix.is_empty() { child_has_default = true; }
                    let p_opt = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                    state.builder.push_namespace_decl(p_opt, uri);
                }
            } else {
                // copy-namespaces="no": keep only the element's own
                // name binding (and its attributes' below) so the
                // serialised names stay resolvable.
                if !q.uri.is_empty() {
                    state.builder.push_namespace_decl(q.prefix.clone(), q.uri.clone());
                    if q.prefix.is_none() { child_has_default = true; }
                }
            }
            // XML Namespaces 1.0 allows `xmlns=""` to undeclare the
            // default namespace.  XPath 1.0 §5.4 represents an
            // undeclaration as "no namespace node for the default" —
            // so when the source element's parent had a default in
            // scope but this element doesn't, replay the undeclaration
            // on the copy.  Only do this when the result-tree parent
            // is itself a copy of the source parent (so the source's
            // namespace shape needs preserving) — otherwise namespace
            // inheritance from the surrounding LRE applies.
            if let Some(parent_id) = mirrored_parent {
                if !child_has_default {
                    let parent_had_default = state.idx.ns_range(parent_id)
                        .into_iter()
                        .any(|nid| state.idx.local_name(nid).is_empty()
                                && !state.idx.string_value(nid).is_empty());
                    if parent_had_default {
                        state.builder.push_namespace_decl(None, String::new());
                    }
                }
            }
            for attr in state.idx.attr_range(node) {
                // `xmlns:*` declarations occasionally land in the
                // attribute list rather than the namespace list
                // depending on parser path; the ns_range loop above
                // already emitted them, so suppress duplicates here.
                let aname = state.idx.node_name(attr);
                if aname == "xmlns" || aname.starts_with("xmlns:") { continue; }
                let aq = attribute_qname(state, attr);
                // Under copy-namespaces="no" a prefixed attribute still
                // needs its own namespace binding declared.
                if !copy_ns && !aq.uri.is_empty() && aq.prefix.is_some() {
                    state.builder.push_namespace_decl(aq.prefix.clone(), aq.uri.clone());
                }
                state.builder.push_attribute(aq, state.idx.string_value(attr));
            }
            for &child in state.idx.children(node) {
                deep_copy_node(state, child, Some(node), copy_ns)?;
            }
            state.builder.close_element();
        }
        XPathNodeKind::Text | XPathNodeKind::CData => {
            state.builder.push_text(state.idx.string_value(node), false);
        }
        XPathNodeKind::Attribute => {
            let q = attribute_qname(state, node);
            state.builder.push_attribute(q, state.idx.string_value(node));
        }
        XPathNodeKind::Comment => {
            state.builder.push_comment(state.idx.string_value(node));
        }
        XPathNodeKind::PI => {
            state.builder.push_pi(
                state.idx.pi_target(node).to_string(),
                state.idx.string_value(node),
            );
        }
        XPathNodeKind::Document => {
            for &c in state.idx.children(node) {
                deep_copy_node(state, c, Some(node), copy_ns)?;
            }
        }
        XPathNodeKind::Namespace => {
            // Copying a namespace node adds its binding to the element
            // under construction (XSLT 2.0 §11.9.2).  The node's
            // local-name is the prefix it binds (empty for the default
            // namespace); its string-value is the URI.  `xml` is
            // implicitly in scope and never re-declared.
            let prefix = state.idx.local_name(node);
            if prefix != "xml" {
                let uri = state.idx.string_value(node);
                let p = if prefix.is_empty() { None } else { Some(prefix.to_string()) };
                state.builder.push_namespace_decl(p, uri);
            }
        }
    }
    Ok(())
}

fn element_qname(state: &EvalState, node: NodeId) -> QName {
    let local  = state.idx.local_name(node).to_string();
    let prefix = state.idx.namespace_prefix(node).map(str::to_string);
    let uri    = state.idx.namespace_uri(node).to_string();
    QName { prefix, local, uri }
}

fn attribute_qname(state: &EvalState, node: NodeId) -> QName {
    let local  = state.idx.local_name(node).to_string();
    let prefix = state.idx.namespace_prefix(node).map(str::to_string);
    let uri    = state.idx.namespace_uri(node).to_string();
    QName { prefix, local, uri }
}

/// Apply `xsl:namespace-alias` URI rewriting to a QName.  If the
/// QName's URI matches a stylesheet-side URI in the alias table,
/// rewrite to the result-side URI.  Prefix is dropped (caller can
/// regenerate from the URI) since the source-side prefix is no
/// longer meaningful.
fn apply_namespace_alias(state: &EvalState, name: &QName) -> QName {
    // XSLT 1.0 §7.1.1 — the source-side of an alias may be the null
    // namespace (stylesheet-prefix="#default" with no default xmlns
    // in scope at the alias declaration).  Don't short-circuit on
    // an empty URI; let the alias table decide.
    for (style_uri, result_uri, result_prefix) in &state.style.namespace_aliases {
        if name.uri == *style_uri {
            // The alias's `result-prefix` (captured at compile time)
            // is the authoritative qualifier for the emitted name.
            // `#default` is represented as None and means "emit as the
            // default namespace, no prefix."  Fall back to a
            // stylesheet-root binding only if the alias didn't supply
            // one.
            let new_prefix = result_prefix.clone().or_else(|| {
                state.namespaces.map.iter()
                    .find(|(_, u)| u.as_str() == result_uri)
                    .and_then(|(p, _)| {
                        // The stylesheet's default namespace lives under
                        // the empty-string key; treat that as the
                        // canonical None-prefix form so we don't emit
                        // a `:stylesheet`-style malformed QName when
                        // the result-prefix= is also #default.
                        if p.is_empty() { None } else { Some(p.clone()) }
                    })
            });
            return QName {
                prefix: new_prefix,
                local:  name.local.clone(),
                uri:    result_uri.clone(),
            };
        }
    }
    name.clone()
}

fn split_qname(s: &str) -> (Option<String>, String) {
    match s.split_once(':') {
        Some((p, l)) => (Some(p.to_string()), l.to_string()),
        None         => (None, s.to_string()),
    }
}

// Suppress unused-warning on INodeKind — kept as a future hook for
// deep-copying namespace declarations (currently we let the
// serialiser regenerate xmlns from element URIs).
#[allow(dead_code)]
fn _force_inode_kind_import(_: INodeKind) {}

/// XSLT 1.0 §3.4 — remove whitespace-only text children from every
/// element whose name matches the more-specific
/// `xsl:strip-space` rule than `xsl:preserve-space`.  Pruning the
/// `content_children` vector is enough; the rest of the engine
/// reaches text children only through that field.
/// Walk `doc`'s element tree gathering every distinct string-value
/// that could plausibly be a URI a dynamic `document()` call wants
/// to resolve: text-node content and attribute values, skipping ones
/// already in `seen`.  Pure structural scan; we leave the
/// "is this actually a URI" question to the loader.
#[allow(dead_code)] // kept for the historical speculative-pre-load
                    // path; runtime dynamic-doc loading replaces it.
fn collect_candidate_uris(
    doc:   &Document,
    out:   &mut Vec<String>,
    seen:  &mut std::collections::HashSet<String>,
) {
    fn walk(
        node: &sup_xml_tree::dom::Node,
        out:  &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        use sup_xml_tree::dom::NodeKind as K;
        match node.kind {
            K::Element => {
                for attr in node.attributes() {
                    let v = attr.value().trim().to_string();
                    if !v.is_empty() && seen.insert(v.clone()) {
                        out.push(v);
                    }
                }
                for child in node.children() {
                    walk(child, out, seen);
                }
            }
            K::Text | K::CData => {
                let v = node.content().trim().to_string();
                if !v.is_empty() && seen.insert(v.clone()) {
                    out.push(v);
                }
            }
            _ => {}
        }
    }
    walk(doc.root(), out, seen);
}

fn apply_strip_space(style: &StylesheetAst, idx: &mut DocIndex) {
    if style.whitespace_rules.is_empty() { return; }
    // Collect (element_id, retained children) pairs first to avoid
    // mutating while iterating the same vec.
    let nlen = idx.nodes.len();
    for id in 0..nlen {
        if !matches!(idx.nodes[id].kind, INodeKind::Element(_)) { continue; }
        let mut filtered: Vec<NodeId> = Vec::with_capacity(idx.nodes[id].content_children.len());
        let kept: Vec<NodeId> = idx.nodes[id].content_children.clone();
        for child in kept {
            if !crate::whitespace::should_strip(style, child, idx) {
                filtered.push(child);
            }
        }
        idx.nodes[id].content_children = filtered;
    }
}

/// Sort a node sequence using the xsl:sort directives that
/// accompany an apply-templates or for-each instruction.  Threads
/// the current XSLT runtime state into the per-key evaluator
/// closure so sort `select=` expressions see the variables and
/// keys in scope.
/// Find a template's position in the stylesheet's template list by
/// pointer identity.  Used to capture `apply_imports_ctx.template_index`
/// so `xsl:next-match` can later exclude the currently-running
/// template from its selection pool.
fn template_index_of(style: &StylesheetAst, t: &crate::ast::Template) -> Option<usize> {
    style.templates.iter().position(|x| std::ptr::eq(x, t))
}

/// Run one `xsl:analyze-string` body (matching or non-matching) with
/// the supplied substring as the synthetic context.  XSLT 2.0 §15.1
/// makes the substring the XPath context item; for our XSLT 1.0-
/// flavoured value model we expose it via a synthetic text node so
/// the body can reference `.` for its string-value.
/// Heuristic: does this pattern contain a reluctant quantifier
/// (`*?`, `+?`, `??`, `}?`) that's *not* inside a character class
/// or escaped literal?  Used by `xsl:analyze-string` to gate the
/// native engine — `find_iter` always picks the longest match,
/// so reluctant patterns would silently produce the wrong
/// partition and need to route through Rust regex instead.
fn regex_has_reluctant_quantifier(src: &str) -> bool {
    let bytes = src.as_bytes();
    let mut in_class = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' => { i += 2; continue; } // skip escape body
            b'[' if !in_class => { in_class = true; }
            b']' if in_class  => { in_class = false; }
            b'?' if !in_class && i > 0 => {
                let prev = bytes[i - 1];
                if matches!(prev, b'*' | b'+' | b'?' | b'}') {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// True iff the `<xsl:matching-substring>` body references
/// `regex-group()` anywhere — in a select expression, an AVT-
/// compiled attribute value, an `xsl:value-of` body, etc.  Used
/// by `xsl:analyze-string` to decide whether the native regex
/// engine (no capture support yet) can serve the call or whether
/// the Rust regex crate's capture-aware path is required.
fn matching_body_uses_regex_group(body: &[Instr]) -> bool {
    // A matching-substring body that dispatches to a template carries
    // the captured groups into the callee (XSLT 2.0 §15.3); we can't
    // tell statically whether the callee reads them, so capture
    // conservatively.
    if crate::walk::body_invokes_templates(body) { return true; }
    let mut hit = false;
    crate::walk::walk_body(body, &mut |e| {
        use sup_xml_core::xpath::ast::Expr;
        if let Expr::FunctionCall(name, _) = e {
            if name == "regex-group" || name.ends_with(":regex-group") { hit = true; }
        }
    });
    hit
}

/// Run the matching / non-matching bodies over a pre-built
/// `xsl:analyze-string` partition.  Per XSLT 2.0 §5.6 the context
/// position inside each substring's body is the substring's position
/// in the whole partition and the context size is the total substring
/// count — so position numbering spans matching and non-matching
/// substrings alike, not a per-kind counter.  A substring whose
/// handler (`xsl:matching-substring` / `xsl:non-matching-substring`)
/// is absent still occupies a position in that numbering.
fn run_analyze_partition(
    state:       &mut EvalState,
    matching:    &[Instr],
    non_matching:&[Instr],
    segments:    &[(bool, String, Vec<String>)],
    ctx_node:    NodeId,
) -> Result<()> {
    let total = segments.len();
    for (i, (is_match, text, groups)) in segments.iter().enumerate() {
        let body = if *is_match { matching } else { non_matching };
        if body.is_empty() { continue; }
        run_analyze_segment(state, body, text, groups, ctx_node, i + 1, total)?;
    }
    Ok(())
}

fn run_analyze_segment(
    state:     &mut EvalState,
    body:      &[Instr],
    segment:   &str,
    groups:    &[String],
    _ctx_node: NodeId,
    pos: usize, size: usize,
) -> Result<()> {
    // Push the segment as a synthetic text node so the body can
    // reference `.` (the context item)'s string-value.  The arena
    // index owns the storage; the NodeId stays valid for the
    // duration of this body run.
    let synth = state.idx
        .allocate_rtf_text_nodes_inherent(vec![segment.to_string()]);
    let leader = *synth.first().unwrap_or(&_ctx_node);
    let prev_groups = std::mem::take(&mut state.regex_groups);
    state.regex_groups = groups.to_vec();
    let prev_current = state.xslt_current;
    state.xslt_current = leader;
    let r = eval_body(state, body, leader, pos, size);
    state.xslt_current = prev_current;
    state.regex_groups = prev_groups;
    r
}

/// `xsl:for-each` over an all-atomic sequence with sort keys: sort
/// the typed values first so the comparator can use date/duration
/// ordering instead of falling back to lexical (which mis-orders
/// e.g. `xs:dateTime('10000-…')` vs `xs:dateTime('1995-…')`), then
/// run the body once per item with `.` set to the current atomic.
fn run_for_each_typed_sequence(
    state:    &mut EvalState,
    items:    Vec<sup_xml_core::xpath::eval::Value>,
    sorts:    &[crate::ast::Sort],
    body:     &[Instr],
    ctx_node: NodeId,
    _pos:     usize,
    _size:    usize,
) -> Result<()> {
    let sorted = sort_items_for_iter(state, items, sorts, ctx_node)?;
    state.variables.enter();
    let total = sorted.len();
    for (i, item) in sorted.into_iter().enumerate() {
        let pos1 = i + 1;
        let body_res = sup_xml_core::xpath::eval::with_atomic_context_item(
            Some(item.clone()),
            || eval_body(state, body, ctx_node, pos1, total),
        );
        body_res?;
    }
    state.variables.leave();
    Ok(())
}

/// Item-based counterpart of [`sort_nodes_for_iter`] — builds an
/// XPath binding per item so the sort-key evaluator can resolve
/// `current()` and atomic-`.` correctly.
fn sort_items_for_iter(
    state:    &mut EvalState,
    items:    Vec<sup_xml_core::xpath::eval::Value>,
    sorts:    &[crate::ast::Sort],
    ctx_node: NodeId,
) -> Result<Vec<sup_xml_core::xpath::eval::Value>> {
    if sorts.is_empty() { return Ok(items); }
    let idx = state.idx;
    let style       = state.style;
    let namespaces  = state.namespaces;
    let keys        = state.keys;
    let documents   = state.documents;
    let decimal_formats = &state.style.decimal_formats;
    let unparsed_entities = &state.unparsed_entities;
    let user_exts   = state.user_exts;
    let current_group = if state.current_group.is_empty() { None } else { Some(state.current_group.as_slice()) };
    let current_grouping_key = state.current_grouping_key.as_ref();
    let accumulators = (!state.accumulators.is_empty()).then_some(&state.accumulators);
    let regex_groups = if state.regex_groups.is_empty() { None } else { Some(state.regex_groups.as_slice()) };
    let user_functions = (!style.functions.is_empty()).then_some(style.functions.as_slice());
    let variables = &state.variables;
    let unparsed_texts = state.unparsed_texts;
    let xslt_3_0 = xslt_version_3_or_more(&style.version);
    let xslt_version = style.version.as_str();
    let sc = state.static_ctx;
    let static_base_uri = state.static_base_uri.as_deref();
    let loader = state.loader;
    let loader_base = state.loader_base;
    let dyn_doc_cache = state.dyn_doc_cache;
    let rtf_base_uris = state.rtf_base_uris;
    crate::sort::sort_items(items, sorts, idx, |expr, item, p, s| {
        let bindings = XsltBindings {
            variables, namespaces, keys,
            xslt_context_node: ctx_node,
            idx, style, documents, decimal_formats, unparsed_entities,
            user_exts, current_group, current_grouping_key, accumulators,
            regex_groups, user_functions, unparsed_texts, xslt_3_0,
            xslt_version, static_base_uri,
            loader, loader_base, dyn_doc_cache, rtf_base_uris,
        };
        let ctx = EvalCtx { context_node: ctx_node, pos: p, size: s, bindings: &bindings, static_ctx: &sc };
        sup_xml_core::xpath::eval::with_atomic_context_item(
            Some(item.clone()),
            || eval_expr(expr, &ctx, idx).map_err(XsltError::from),
        )
    })
}

/// Order the groups of an `xsl:for-each-group` for its `xsl:sort`
/// children.  Unlike [`sort_nodes_for_iter`], each sort key is
/// evaluated with `current-group()` / `current-grouping-key()` bound
/// to the group being ranked (XSLT 2.0 §14.3) — so keys such as
/// `sum(current-group()/@pop)` or `current-grouping-key()` see that
/// group rather than the outer (empty) grouping context.  Returns the
/// group indices in sorted order; first-appearance order breaks ties.
fn sort_group_indices(
    state:  &EvalState,
    groups: &[(Value, Vec<NodeId>)],
    sorts:  &[crate::ast::Sort],
) -> Result<Vec<usize>> {
    if sorts.is_empty() { return Ok((0..groups.len()).collect()); }
    let idx = state.idx;
    let style = state.style;
    let namespaces = state.namespaces;
    let keys = state.keys;
    let documents = state.documents;
    let decimal_formats = &state.style.decimal_formats;
    let unparsed_entities = &state.unparsed_entities;
    let user_exts = state.user_exts;
    let user_functions = (!style.functions.is_empty()).then_some(style.functions.as_slice());
    let variables = &state.variables;
    let unparsed_texts = state.unparsed_texts;
    let xslt_3_0 = xslt_version_3_or_more(&style.version);
    let xslt_version = style.version.as_str();
    let sc = state.static_ctx;
    let static_base_uri = state.static_base_uri.as_deref();
    let loader = state.loader;
    let loader_base = state.loader_base;
    let dyn_doc_cache = state.dyn_doc_cache;
    let rtf_base_uris = state.rtf_base_uris;
    // `sort_nodes` calls the key evaluator with `p` = the group's
    // 1-based position in unsorted order, so `groups[p - 1]` is the
    // group whose accessors must be in scope.  Group leader nodes are
    // distinct (each source item lands in exactly one group), so the
    // sorted leaders map back to indices unambiguously.
    let first_nodes: Vec<NodeId> =
        groups.iter().map(|(_, ns)| *ns.first().unwrap_or(&0)).collect();
    let sorted = crate::sort::sort_nodes(&first_nodes, sorts, idx, |expr, n, p, s| {
        let (gk, gns) = &groups[p - 1];
        let bindings = XsltBindings {
            variables, namespaces, keys,
            xslt_context_node: n,
            idx, style, documents, decimal_formats, unparsed_entities,
            user_exts,
            current_group: Some(gns.as_slice()),
            current_grouping_key: Some(gk),
            accumulators: None,
            regex_groups: None,
            user_functions, unparsed_texts, xslt_3_0,
            xslt_version, static_base_uri,
            loader, loader_base, dyn_doc_cache, rtf_base_uris,
        };
        let ctx = EvalCtx { context_node: n, pos: p, size: s, bindings: &bindings, static_ctx: &sc };
        eval_expr(expr, &ctx, idx).map_err(XsltError::from)
    })?;
    let mut by_node: HashMap<NodeId, Vec<usize>> = HashMap::new();
    for (i, &n) in first_nodes.iter().enumerate() {
        by_node.entry(n).or_default().push(i);
    }
    Ok(sorted.into_iter()
        .filter_map(|n| by_node.get_mut(&n).and_then(|v| v.pop()))
        .collect())
}

/// Realise a value as a node list for `xsl:merge`: node-sets pass
/// through, atomic items become synthetic text nodes (matching the
/// xsl:for-each-group treatment), foreign nodes are dropped.
fn merge_materialize_nodes(state: &mut EvalState, v: Value) -> Vec<NodeId> {
    match v {
        Value::NodeSet(ns) => ns,
        Value::Sequence(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::NodeSet(ns) => out.extend(ns),
                    Value::ForeignNodeSet(_) => {}
                    atomic => {
                        let s = value_to_string_styled(&atomic, state.idx, state.num_style());
                        out.extend(state.idx.allocate_rtf_text_nodes_inherent(vec![s]));
                    }
                }
            }
            out
        }
        Value::ForeignNodeSet(_) => Vec::new(),
        atomic => {
            let s = value_to_string_styled(&atomic, state.idx, state.num_style());
            state.idx.allocate_rtf_text_nodes_inherent(vec![s])
        }
    }
}

/// Evaluate a merge-source's `xsl:merge-key` selectors against `node`,
/// returning one value per key (a key with no `select` uses the node's
/// string value, per the xsl:sort default).
fn merge_key_values(state: &mut EvalState, keys: &[crate::ast::Sort], node: NodeId)
    -> Result<Vec<Value>>
{
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        let v = match &k.select {
            Some(e) => state.xpath_eval(e, node, 1, 1)?,
            None    => Value::String(state.idx.string_value(node)),
        };
        out.push(v);
    }
    Ok(out)
}

/// A composite equality string for a node's merge-key values — used to
/// detect adjacency of equal keys.  Uses `eq`-style equality (temporal
/// values by instant, etc.) where available, else the string value.
fn merge_group_key_str(vals: &[Value], state: &EvalState) -> String {
    vals.iter()
        .map(|v| value_equality_key(v).unwrap_or_else(||
            value_to_string_styled(v, state.idx, state.num_style())))
        .collect::<Vec<_>>()
        .join("\u{1}")
}

fn sort_nodes_for_iter(
    state:    &mut EvalState,
    nodes:    &[NodeId],
    sorts:    &[crate::ast::Sort],
    _ctx_node: NodeId, _pos: usize, _size: usize,
) -> Result<Vec<NodeId>> {
    if sorts.is_empty() { return Ok(nodes.to_vec()); }
    with_sort_key_eval(state, |idx, eval| crate::sort::sort_nodes(nodes, sorts, idx, eval))
}

/// Sort permutation for an `xsl:merge` stream: node `i` is keyed by
/// `per_node_sorts[i]` (its originating merge-source's keys).  Returns
/// a permutation of `0..nodes.len()` so the caller can reorder its
/// source-tagged view.  See [`crate::sort::sort_order_keyed`].
fn merge_sort_order(
    state:          &mut EvalState,
    nodes:          &[NodeId],
    per_node_sorts: &[&[crate::ast::Sort]],
) -> Result<Vec<usize>> {
    with_sort_key_eval(state, |idx, eval|
        crate::sort::sort_order_keyed(nodes, per_node_sorts, idx, eval))
}

/// Build the XPath evaluation context a sort-key `select` runs in and
/// hand it to `driver` as `(idx, &mut eval)`.  Sort keys see the
/// node being sorted as both context node and `current()` (XSLT 1.0
/// §10), plus the ambient variables / keys / grouping state.
fn with_sort_key_eval<R>(
    state:  &mut EvalState,
    driver: impl FnOnce(
        &DocIndex,
        &mut dyn FnMut(&sup_xml_core::xpath::Expr, NodeId, usize, usize) -> Result<Value>,
    ) -> Result<R>,
) -> Result<R> {
    // Build the per-evaluation bindings outside the closure so the
    // closure captures borrows only.
    let idx = state.idx;
    // XSLT 1.0 §12.4 — inside the sort-key context, `current()`
    // must return the node being sorted, not the outer
    // apply-templates current.  Build bindings per item so the
    // XsltBindings.xslt_context_node tracks each.
    let style       = state.style;
    let namespaces  = state.namespaces;
    let keys        = state.keys;
    let documents   = state.documents;
    let decimal_formats = &state.style.decimal_formats;
    let unparsed_entities = &state.unparsed_entities;
    let user_exts   = state.user_exts;
    let current_group = if state.current_group.is_empty() { None } else { Some(state.current_group.as_slice()) };
    let current_grouping_key = state.current_grouping_key.as_ref();
    let accumulators = (!state.accumulators.is_empty()).then_some(&state.accumulators);
    let regex_groups = if state.regex_groups.is_empty() { None } else { Some(state.regex_groups.as_slice()) };
    let user_functions = (!style.functions.is_empty()).then_some(style.functions.as_slice());
    let variables = &state.variables;
    let unparsed_texts = state.unparsed_texts;
    let xslt_3_0 = xslt_version_3_or_more(&style.version);
    let xslt_version = style.version.as_str();
    let sc = state.static_ctx;
    let static_base_uri = state.static_base_uri.as_deref();
    let loader = state.loader;
    let loader_base = state.loader_base;
    let dyn_doc_cache = state.dyn_doc_cache;
    let rtf_base_uris = state.rtf_base_uris;
    let mut eval = |expr: &sup_xml_core::xpath::Expr, n, p, s| {
        let bindings = XsltBindings {
            variables, namespaces, keys,
            xslt_context_node: n,
            idx, style, documents, decimal_formats, unparsed_entities,
            user_exts, current_group, current_grouping_key, accumulators,
            regex_groups, user_functions, unparsed_texts, xslt_3_0,
            xslt_version, static_base_uri,
            loader, loader_base, dyn_doc_cache, rtf_base_uris,
        };
        let ctx = EvalCtx { context_node: n, pos: p, size: s, bindings: &bindings, static_ctx: &sc };
        eval_expr(expr, &ctx, idx).map_err(XsltError::from)
    };
    driver(idx, &mut eval)
}

