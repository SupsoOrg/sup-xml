#![forbid(unsafe_code)]

//! W3C XInclude 1.0 — arena-DOM port of [`crate::xinclude`].
//!
//! Mirrors the legacy [`crate::xinclude::process_xincludes`] behaviour, but
//! operates on the arena DOM ([`sup_xml_tree::dom::Document`]).
//!
//! # API shape: returns a *new* [`Document`] (not mutate-in-place)
//!
//! The legacy implementation mutates the tree in place: it locates every
//! `<xi:include>` element inside a parent's `Vec<Node>`, removes it, and
//! splices in the resolved nodes.  That model maps poorly onto the arena
//! DOM because:
//!
//! 1. Arena nodes live in a [`bumpalo::Bump`] that's owned by the
//!    [`Document`]; per-node allocations cannot be freed individually.  An
//!    `<xi:include>` element that gets "replaced" would simply leak its
//!    bytes (harmless, but wasteful).
//! 2. Included content is parsed into a separate [`Document`] with its own
//!    arena.  An arena `Node<'doc>` carries references (`&'doc str`,
//!    `&'doc Namespace`) tied to that arena and so cannot be moved into
//!    a different one — it must be deep-copied.
//! 3. The neat way to deep-copy *across* arenas while at the same time
//!    expanding nested `xi:include` elements is to do both in a single
//!    walk: read the source tree, allocate equivalents in a destination
//!    builder, and substitute include elements as they're encountered.
//!
//! So this module's public entry point [`process_xincludes`]
//! returns a new [`Document`] rather than mutating its argument.  The
//! original document is left untouched.
//!
use std::collections::HashSet;
use std::sync::Arc;

use sup_xml_tree::dom::{Document, DocumentBuilder, Node, NodeKind};

use crate::entity_resolver::{EntityResolver, ResolveError};
use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::options::ParseOptions;
use crate::parser::parse_str;

/// XInclude namespace URI.  Constant per spec § 4.
pub const XINCLUDE_NS: &str = "http://www.w3.org/2001/XInclude";

/// Options for [`process_xincludes`].
#[derive(Clone)]
pub struct XIncludeOptions {
    /// Resolver used to fetch referenced resources (XML or text).
    /// Without one, every `xi:include` with `href` triggers
    /// `xi:fallback` or errors.
    pub resolver: Option<Arc<dyn EntityResolver>>,
    /// Maximum include depth — protects against pathological
    /// recursion that escapes the cycle-detector (e.g. a → b → a').
    /// Default: 16.
    pub max_depth: u32,
    /// Maximum total bytes across all included resources.  Protects
    /// against XInclude bombs analogous to billion-laughs.  Default:
    /// 10 MB.
    pub max_total_bytes: u64,
}

impl Default for XIncludeOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl XIncludeOptions {
    /// Construct with default limits (depth 16, total bytes 10 MB) and no
    /// resolver — every `xi:include` with `href` will trigger `xi:fallback`
    /// or error until a resolver is set.
    pub fn new() -> Self {
        Self {
            resolver: None,
            max_depth: 16,
            max_total_bytes: 10 * 1024 * 1024,
        }
    }
}

/// Process every `xi:include` element in `doc`, returning a fresh
/// [`Document`] in which each include has been replaced by the resolved
/// content.  Operates by deep-copying the original tree into a new arena;
/// `doc` is left untouched.
///
/// Errors:
/// - `XmlError(domain=Io)` — resolver refused or file unreadable AND no
///   `<xi:fallback>` was provided
/// - `XmlError(domain=Parser)` — included XML didn't parse
/// - `XmlError(domain=Validation)` — cycle detected, depth/byte limit
///   exceeded, or `xi:include` element used incorrectly (missing href,
///   bad parse= value, etc.)
///
/// # API divergence from legacy
///
/// The legacy [`crate::xinclude::process_xincludes`] mutates its argument
/// in-place.  This arena version returns a new [`Document`] instead — see
/// the module docs for the reasoning.
pub fn process_xincludes(
    doc: &Document,
    opts: &XIncludeOptions,
) -> Result<Document> {
    let b = DocumentBuilder::new();
    let mut state = State {
        opts: opts.clone(),
        hrefs: HashSet::new(),
        depth: 0,
        total_bytes: 0,
    };

    // Copy XML declaration fields verbatim.
    b.set_version(doc.version.clone());
    b.set_encoding(doc.encoding.clone());
    b.set_standalone(doc.standalone);

    let src_root = doc.root();
    // The root itself can be `<xi:include>` in pathological cases, but
    // that's not legal XInclude (the included content must replace it
    // and a document must have exactly one root element).  Reject by
    // refusing to flatten the root.  Detected indirectly: if the root
    // is an xi:include, copy_element_into produces zero or more nodes
    // and the destination ends up rootless.
    if src_root.kind == NodeKind::Element && is_xinclude_element_named(src_root.name()) {
        return Err(validation_err(
            "xi:include is not allowed as the document root element",
        ));
    }

    let dst_root = copy_subtree(&b, src_root, &mut state)?;
    b.set_root(dst_root);
    Ok(b.build())
}

// ── state ───────────────────────────────────────────────────────────────────

struct State {
    opts: XIncludeOptions,
    /// Stack of resolved hrefs already in the include chain — for cycle
    /// detection.
    hrefs: HashSet<String>,
    depth: u32,
    total_bytes: u64,
}

// ── core walker ─────────────────────────────────────────────────────────────

/// Deep-copy `src` (an arbitrary node) into the destination arena owned by
/// `b`, returning the new node.  Caller is responsible for attaching the
/// result to a parent (or marking it as the root).
///
/// Encountering `<xi:include>` while copying a *child list* (inside
/// [`copy_children_into`]) triggers expansion.  This function is for nodes
/// that are NOT themselves include elements — callers must filter or use
/// [`copy_children_into`] which handles the splice.
fn copy_subtree<'a>(
    b: &'a DocumentBuilder,
    src: &Node<'_>,
    state: &mut State,
) -> Result<&'a Node<'a>> {
    match src.kind {
        NodeKind::Element => {
            let name = b.alloc_str(src.name());
            let el = b.new_element(name);
            for attr in src.attributes() {
                let aname = b.alloc_str(attr.name());
                let aval = b.alloc_str(attr.value());
                let new_attr = b.new_attribute(aname, aval);
                b.append_attribute(el, new_attr);
            }
            copy_children_into(b, el, src, state)?;
            Ok(el)
        }
        NodeKind::Text => {
            let content = b.alloc_str(src.content());
            Ok(b.new_text(content))
        }
        NodeKind::CData => {
            let content = b.alloc_str(src.content());
            Ok(b.new_cdata(content))
        }
        NodeKind::Comment => {
            let content = b.alloc_str(src.content());
            Ok(b.new_comment(content))
        }
        NodeKind::Pi => {
            let target = b.alloc_str(src.name());
            let content = src.content_opt().map(|c| &*b.alloc_str(c));
            Ok(b.new_pi(target, content))
        }
        NodeKind::EntityRef => {
            // Preserve the unresolved reference across the XInclude
            // copy.  Source was parsed with `resolve_entities=false`;
            // the included view should round-trip the same.
            let name    = b.alloc_str(src.name());
            let content = b.alloc_str(src.content());
            Ok(b.new_entity_ref(name, content))
        }
        // c-abi-only discriminant; never appears on a real Node.
        NodeKind::Attribute => unreachable!("Attribute kind never appears on a Node"),
        NodeKind::Document  => unreachable!("Document kind never appears on a Node"),
        NodeKind::DocumentFragment => unreachable!(
            "DocumentFragment is a compat-shim transient; XInclude does not walk into one"
        ),
        NodeKind::DtdDecl => unreachable!(
            "DtdDecl is an internal-subset child; XInclude copies element subtrees only"
        ),
        NodeKind::Dtd => unreachable!(
            "Dtd is a document-level internal-subset node; XInclude copies element subtrees only"
        ),
    }
}

/// Copy `src_parent`'s children into `dst_parent`, expanding any
/// `<xi:include>` elements encountered along the way.
fn copy_children_into<'a>(
    b: &'a DocumentBuilder,
    dst_parent: &'a Node<'a>,
    src_parent: &Node<'_>,
    state: &mut State,
) -> Result<()> {
    for child in src_parent.children() {
        if child.kind == NodeKind::Element && is_xinclude_element(child) {
            // Resolve and splice in zero or more nodes.
            let replacements = resolve_include(b, child, state)?;
            for n in replacements {
                b.append_child(dst_parent, n);
            }
        } else {
            let new_child = copy_subtree(b, child, state)?;
            b.append_child(dst_parent, new_child);
        }
    }
    Ok(())
}

// ── xi:include detection ────────────────────────────────────────────────────

fn is_xinclude_element(elem: &Node<'_>) -> bool {
    // First check namespace if set — most reliable.
    if let Some(ns) = elem.namespace.get() {
        if ns.href() == XINCLUDE_NS {
            let local = local_name(elem.name());
            return local == "include";
        }
    }
    // Fall back to name-and-binding heuristic (matches legacy behaviour
    // for non-namespace-aware parses).
    is_xinclude_element_named_with_attrs(elem)
}

/// Same as [`is_xinclude_element`] but works on just the element's name
/// (used for the root-element check before any element body inspection).
fn is_xinclude_element_named(name: &str) -> bool {
    name == "xi:include"
}

/// Heuristic check used when the arena DOM was parsed without namespace
/// awareness (so [`Node::namespace`] is unset): inspect the element's
/// attributes for an `xmlns*` binding to the XInclude namespace.
fn is_xinclude_element_named_with_attrs(elem: &Node<'_>) -> bool {
    let name = elem.name();
    let local = local_name(name);
    if local != "include" {
        return false;
    }
    // Either the element name has a prefix bound on this element to
    // the XInclude namespace (xmlns:xi="..." on this element), or the
    // default namespace is XInclude.  We only check this element —
    // matches legacy v1 behaviour, which lacked full namespace-scope
    // tracking.
    if elem.attributes().any(|a| {
        (a.name() == "xmlns" || a.name().starts_with("xmlns:"))
            && a.value() == XINCLUDE_NS
    }) {
        return true;
    }
    // Last-resort heuristic — the prefix-bare local name "xi:include"
    // is distinctive enough that we accept it even without finding a
    // binding.  Trade-off documented in legacy module.
    name == "xi:include"
}

fn is_xi_fallback(elem: &Node<'_>) -> bool {
    let local = local_name(elem.name());
    local == "fallback"
}

#[inline]
fn local_name(qname: &str) -> &str {
    qname.rsplit_once(':').map(|(_, l)| l).unwrap_or(qname)
}

// ── attribute parsing ───────────────────────────────────────────────────────

#[derive(Default)]
struct XiAttrs {
    href: Option<String>,
    parse: XiParseMode,
    /// XPointer fragment selector (XInclude § 4.2).  When `Some`,
    /// the included document is parsed first, then the xpointer is
    /// evaluated against it and only the matching subtree is
    /// spliced in.  `None` (the common case) splices the entire
    /// included root.
    xpointer: Option<String>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum XiParseMode {
    #[default]
    Xml,
    Text,
}

fn parse_xi_include_attrs(elem: &Node<'_>) -> Result<XiAttrs> {
    let mut out = XiAttrs::default();
    for attr in elem.attributes() {
        let local = local_name(attr.name());
        match local {
            "href" => out.href = Some(attr.value().to_string()),
            "parse" => {
                out.parse = match attr.value() {
                    "xml" => XiParseMode::Xml,
                    "text" => XiParseMode::Text,
                    other => {
                        return Err(validation_err(format!(
                            "xi:include parse={other:?} is not supported \
                             (only \"xml\" and \"text\")"
                        )))
                    }
                };
            }
            "xpointer" => {
                out.xpointer = Some(attr.value().to_string());
            }
            // accept, accept-language, encoding, xml:base — silently
            // ignored.  See legacy module docs.
            _ => {}
        }
    }
    Ok(out)
}

// ── include resolution ──────────────────────────────────────────────────────

/// Resolve a single `xi:include` element, returning the destination-arena
/// nodes that should replace it in its parent's children list.  Handles
/// fallback: on primary-resolution failure, falls back to the element's
/// `<xi:fallback>` child (if any), failing with the primary error
/// otherwise.
fn resolve_include<'a>(
    b: &'a DocumentBuilder,
    include_elem: &Node<'_>,
    state: &mut State,
) -> Result<Vec<&'a Node<'a>>> {
    state.depth += 1;
    if state.depth > state.opts.max_depth {
        state.depth -= 1;
        return Err(validation_err(format!(
            "XInclude depth limit ({}) exceeded — possible recursion",
            state.opts.max_depth
        )));
    }

    let primary = resolve_include_inner(b, include_elem, state);
    state.depth -= 1;

    match primary {
        Ok(nodes) => Ok(nodes),
        Err(primary_err) => {
            // Try fallback: search children for <xi:fallback>.
            let fallback_elem = include_elem.children().find(|c| {
                c.kind == NodeKind::Element && is_xi_fallback(c)
            });
            match fallback_elem {
                Some(fb) => {
                    // Build a transparent container in the destination
                    // arena: copy the fallback's children into it,
                    // recursing through the standard walker (which
                    // handles any further xi:includes inside the
                    // fallback per XInclude § 4.4).  We don't actually
                    // attach the container — we collect the children it
                    // accumulated.
                    let tmp = b.new_element(b.alloc_str("__xi_fallback_tmp__"));
                    copy_children_into(b, tmp, fb, state)?;
                    // Detach children from tmp and collect.  We could
                    // also just return the children-iterator's nodes
                    // directly — `tmp` will leak in the arena (a single
                    // unreferenced placeholder element, negligible).
                    // Detaching keeps things tidy and ensures the
                    // caller can re-append them.
                    let mut out = Vec::new();
                    let mut cur = tmp.first_child.get();
                    while let Some(c) = cur {
                        let next = c.next_sibling.get();
                        b.detach(c);
                        out.push(c);
                        cur = next;
                    }
                    Ok(out)
                }
                None => Err(primary_err),
            }
        }
    }
}

fn resolve_include_inner<'a>(
    b: &'a DocumentBuilder,
    include_elem: &Node<'_>,
    state: &mut State,
) -> Result<Vec<&'a Node<'a>>> {
    let attrs = parse_xi_include_attrs(include_elem)?;

    let href = attrs.href.ok_or_else(|| {
        validation_err(
            "xi:include without href is not supported in v1 (use \
             href to reference an external resource)",
        )
    })?;

    if state.hrefs.contains(&href) {
        return Err(validation_err(format!(
            "XInclude cycle detected: {href:?} is already in the \
             include chain"
        )));
    }

    let resolver = state
        .opts
        .resolver
        .as_ref()
        .cloned()
        .ok_or_else(|| {
            io_err(format!(
                "xi:include {href:?} cannot be resolved — no \
                 resolver configured (set XIncludeOptions::resolver)"
            ))
        })?;

    let bytes = resolver
        .resolve(None, &href, None)
        .map_err(|e| match e {
            ResolveError::Refused(msg) => io_err(format!(
                "xi:include {href:?} refused by resolver: {msg}"
            )),
            ResolveError::Io(io) => io_err(format!(
                "xi:include {href:?} I/O error: {io}"
            )),
            ResolveError::Other(other) => io_err(format!(
                "xi:include {href:?} resolver error: {other}"
            )),
        })?;

    let added = bytes.len() as u64;
    if state.total_bytes.saturating_add(added) > state.opts.max_total_bytes {
        return Err(validation_err(format!(
            "XInclude byte budget ({}) exceeded by {href:?}",
            state.opts.max_total_bytes
        )));
    }
    state.total_bytes += added;

    match attrs.parse {
        XiParseMode::Xml => {
            let text = std::str::from_utf8(&bytes).map_err(|e| {
                XmlError::new(
                    ErrorDomain::Encoding,
                    ErrorLevel::Fatal,
                    format!("xi:include {href:?} bytes are not UTF-8: {e}"),
                )
            })?;
            // Parse the included document into its own arena.  We then
            // walk its tree and copy nodes into `b`'s arena, expanding
            // nested xi:include elements along the way.
            let sub_doc = parse_str(text, &ParseOptions::default())?;

            state.hrefs.insert(href.clone());

            // XPointer fragment selection (XInclude § 4.2).  When the
            // xpointer attribute is set, the included document is
            // first resolved by the xpointer; only the matching
            // subtree is then spliced in.  When unset, splice the
            // whole document root — common case.
            let result: Result<Vec<&Node<'_>>> = (|| {
                let targets: Vec<&Node<'_>> = match &attrs.xpointer {
                    Some(xp) => resolve_xpointer(&sub_doc, xp, &href)?,
                    None     => vec![sub_doc.root()],
                };
                let mut out: Vec<&Node<'_>> = Vec::with_capacity(targets.len());
                for tgt in targets {
                    // If the matched node is itself an xi:include,
                    // expand it (mirrors the legacy root-is-xinclude
                    // handling).  This also covers the case where an
                    // xpointer selected an xi:include inside the
                    // included doc.
                    if is_xinclude_element(tgt) {
                        out.extend(resolve_include(b, tgt, state)?);
                    } else {
                        out.push(copy_subtree(b, tgt, state)?);
                    }
                }
                Ok(out)
            })();

            state.hrefs.remove(&href);
            result
        }
        XiParseMode::Text => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let alloc = b.alloc_str(&text);
            Ok(vec![b.new_text(alloc)])
        }
    }
}

// ── xpointer ────────────────────────────────────────────────────────────────

/// Resolve an XPointer expression against an included document,
/// returning the element/document nodes selected, in document order.
///
/// This is the XInclude § 4.2 fragment-selector subset, shared by the
/// pure-Rust engine and the libxml2-compat C-ABI XInclude path.
/// Supports the three forms most commonly seen in practice; anything
/// else returns a validation error so the caller can fall back to
/// `xi:fallback` (or fail loudly rather than splice the wrong content):
///
/// - **`xpointer(EXPR)`** — `EXPR` is parsed as XPath 1.0 and
///   evaluated against the included document.  The resulting
///   nodeset is returned in document order.  Empty nodeset is an
///   error (no nodes → can't splice anything).
/// - **`element(/N1/N2/...)`** — the ChildSequence scheme.  Walk
///   from the document root, picking the Nth element-typed child
///   at each step (1-based).  XInclude § 4.2 / XPointer
///   ChildSequence.
/// - **`element(NAME)`** / **`element(NAME/N1/...)`** — start with
///   the element whose `id` attribute is `NAME`, then optionally
///   walk a ChildSequence from there.
/// - **`NAME`** (bare ID) — shorthand for `element(NAME)`.
pub fn resolve_xpointer<'a>(
    sub_doc:  &'a sup_xml_tree::dom::Document,
    expr:     &str,
    href:     &str,
) -> Result<Vec<&'a Node<'a>>> {
    let bad = |msg: String| validation_err(format!(
        "xi:include xpointer={expr:?} ({href}): {msg}"
    ));

    // ── xpointer(EXPR) scheme: full XPath ─────────────────────────────────
    if let Some(rest) = expr.strip_prefix("xpointer(") {
        let inner = rest.strip_suffix(')').ok_or_else(|| {
            bad("missing closing `)` after xpointer scheme".to_string())
        })?;
        let result = crate::xpath::xpath_eval(sub_doc, inner)
            .map_err(|e| bad(format!("XPath evaluation failed: {e}")))?;
        let ids = match result {
            crate::xpath::XPathValue::NodeSet(ns) if !ns.is_empty() => ns,
            crate::xpath::XPathValue::NodeSet(_) => {
                return Err(bad("XPath returned an empty nodeset".to_string()));
            }
            _ => return Err(bad(
                "XPath must return a nodeset for xi:include splicing".to_string()
            )),
        };
        // Resolve NodeId → &Node via DocIndex.  We can't share the
        // sub_doc's index across the function boundary easily, so
        // rebuild it locally — XInclude expansion is the cold path,
        // not perf-critical.
        let idx = crate::xpath::context::DocIndex::build(sub_doc);
        let mut out: Vec<&Node<'_>> = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(n) = node_id_to_arena(&idx, id) {
                out.push(n);
            }
        }
        if out.is_empty() {
            return Err(bad(
                "XPath matched only non-element nodes (text, attributes, \
                 etc.) — XInclude can only splice element / document \
                 subtrees".to_string()
            ));
        }
        return Ok(out);
    }

    // ── element(...) scheme: ChildSequence or ID + ChildSequence ──────────
    if let Some(rest) = expr.strip_prefix("element(") {
        let inner = rest.strip_suffix(')').ok_or_else(|| {
            bad("missing closing `)` after element scheme".to_string())
        })?;
        // Split into (optional name) + ChildSequence segments.
        // Leading `/` means "anchor at root, walk the ChildSequence".
        // Leading name means "find element by ID, then walk".
        let trimmed = inner.trim();
        let (start, seq): (Option<&str>, &str) = if trimmed.starts_with('/') {
            (None, trimmed)
        } else {
            // ID (and optional `/` ChildSequence appended).
            match trimmed.find('/') {
                Some(slash) => (Some(&trimmed[..slash]), &trimmed[slash..]),
                None        => (Some(trimmed), ""),
            }
        };
        // XPointer ChildSequence anchored at `/` starts FROM the
        // document — its first step (`/N`) selects the Nth top-level
        // element, of which XML allows exactly one (the root).  So
        // `/1` is the root, `/1/2` is the root's 2nd element child,
        // etc.  When the start is an ID, the named element IS the
        // anchor and walks descend from there directly.
        let (anchor, seq_after_root): (&Node<'_>, &str) = match start {
            None => {
                // Strip leading `/` and the mandatory `1` (or it's
                // out-of-range).  The remainder walks from the root.
                let after_slash = seq.trim_start_matches('/');
                let (first, rest) = match after_slash.find('/') {
                    Some(i) => (&after_slash[..i], &after_slash[i + 1..]),
                    None    => (after_slash, ""),
                };
                if !first.is_empty() {
                    let n: usize = first.parse().map_err(|_| {
                        bad(format!("ChildSequence root step {first:?} is not a positive integer"))
                    })?;
                    if n != 1 {
                        return Err(bad(format!(
                            "ChildSequence root step must be 1 (only one root \
                             element exists); got {n}"
                        )));
                    }
                }
                (sub_doc.root(), rest)
            }
            Some(name) => {
                let n = find_by_id(sub_doc, name).ok_or_else(|| {
                    bad(format!("no element with id={name:?}"))
                })?;
                (n, seq.trim_start_matches('/'))
            }
        };
        let target = walk_child_sequence(anchor, seq_after_root, &bad)?;
        return Ok(vec![target]);
    }

    // ── bare ID (no scheme parens) ────────────────────────────────────────
    if !expr.contains('(') && !expr.contains('/') && !expr.is_empty() {
        let n = find_by_id(sub_doc, expr).ok_or_else(|| {
            bad(format!("no element with id={expr:?}"))
        })?;
        return Ok(vec![n]);
    }

    Err(bad(format!(
        "unrecognized xpointer form — supported: \
         `xpointer(EXPR)`, `element(/N/...)`, `element(NAME[/N/...])`, \
         or bare `NAME`"
    )))
}

/// Walk a ChildSequence (slash-separated positive integers) from
/// `anchor`, picking the Nth element-typed child at each step
/// (1-based).  `seq` may be empty (→ return `anchor`), `/1` (→ first
/// child), `/1/2/3` (→ third child of the second child of the first
/// child), etc.  Leading `/` is allowed and skipped.
fn walk_child_sequence<'a, F>(
    anchor: &'a Node<'a>,
    seq:    &str,
    bad:    &F,
) -> Result<&'a Node<'a>>
where
    F: Fn(String) -> XmlError,
{
    let mut current = anchor;
    let mut remaining = seq.trim_start_matches('/');
    while !remaining.is_empty() {
        let (head, tail) = match remaining.find('/') {
            Some(i) => (&remaining[..i], &remaining[i + 1..]),
            None    => (remaining, ""),
        };
        let n: usize = head.parse().map_err(|_| {
            bad(format!("ChildSequence step {head:?} is not a positive integer"))
        })?;
        if n == 0 {
            return Err(bad(
                "ChildSequence steps are 1-based; got 0".to_string(),
            ));
        }
        let mut next_node: Option<&Node<'_>> = None;
        let mut seen = 0usize;
        for child in current.children() {
            if child.is_element() {
                seen += 1;
                if seen == n {
                    next_node = Some(child);
                    break;
                }
            }
        }
        current = next_node.ok_or_else(|| {
            bad(format!(
                "ChildSequence step {n} out of range — only {seen} element \
                 child(ren) under <{}>",
                current.name()
            ))
        })?;
        remaining = tail;
    }
    Ok(current)
}

/// Walk the document looking for an element whose `id`-like
/// attribute equals `id`.  No DTD-driven ID-attribute resolution
/// yet — we accept the literal attribute `id` or `xml:id`, which
/// covers the overwhelming majority of fragment uses.
fn find_by_id<'a>(
    doc: &'a sup_xml_tree::dom::Document,
    id:  &str,
) -> Option<&'a Node<'a>> {
    fn walk<'a>(n: &'a Node<'a>, id: &str) -> Option<&'a Node<'a>> {
        if n.is_element() {
            for a in n.attributes() {
                let nm = a.name();
                if (nm == "id" || nm == "xml:id" || nm.ends_with(":id"))
                    && a.value() == id
                {
                    return Some(n);
                }
            }
        }
        for c in n.children() {
            if let Some(found) = walk(c, id) {
                return Some(found);
            }
        }
        None
    }
    walk(doc.root(), id)
}

/// Look up the arena Node behind an XPath `NodeId`.  Returns `None`
/// for nodes the engine indexes that don't have a tree backing
/// (Document virtual root, namespace synthetic nodes).
fn node_id_to_arena<'doc>(
    idx: &crate::xpath::context::DocIndex<'doc>,
    id:  crate::xpath::NodeId,
) -> Option<&'doc Node<'doc>> {
    use crate::xpath::context::INodeKind;
    match &idx.nodes.get(id)?.kind {
        INodeKind::Element(n) => Some(n),
        // For non-element matches we don't splice — XInclude needs
        // element- or document-rooted subtrees.  Caller filters.
        _ => None,
    }
}

// ── error helpers ───────────────────────────────────────────────────────────

fn io_err(msg: String) -> XmlError {
    XmlError::new(ErrorDomain::Io, ErrorLevel::Fatal, msg)
}

fn validation_err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::Validation, ErrorLevel::Fatal, msg)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_resolver::InMemoryResolver;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn opts_with_resolver(map: HashMap<String, Vec<u8>>) -> XIncludeOptions {
        let mut r = InMemoryResolver::new();
        for (sys, bytes) in map {
            r = r.with_system(&sys, bytes);
        }
        XIncludeOptions {
            resolver: Some(Arc::new(r)),
            ..XIncludeOptions::new()
        }
    }

    fn parse(xml: &str) -> Document {
        parse_str(xml, &ParseOptions::default()).expect("parse")
    }

    #[test]
    fn xinclude_xml_replaces_include_with_referenced_subtree() {
        let mut docs = HashMap::new();
        docs.insert("part.xml".to_string(), b"<chunk>hello</chunk>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="part.xml"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let root = out.root();
        assert_eq!(root.name(), "root");
        // The xi:include is replaced by the <chunk> element.
        let kids: Vec<_> = root.children().collect();
        assert_eq!(kids.len(), 1);
        let chunk = kids[0];
        assert_eq!(chunk.kind, NodeKind::Element);
        assert_eq!(chunk.name(), "chunk");
        assert_eq!(chunk.text_content(), Some("hello"));
    }

    #[test]
    fn xinclude_text_includes_raw_bytes_as_text_node() {
        let mut docs = HashMap::new();
        docs.insert(
            "readme.txt".to_string(),
            b"raw <text> with & specials".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="readme.txt" parse="text"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let root = out.root();
        let kids: Vec<_> = root.children().collect();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].kind, NodeKind::Text);
        assert_eq!(kids[0].content(), "raw <text> with & specials");
    }

    #[test]
    fn xinclude_fallback_used_when_resolve_fails() {
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="missing.xml">
                <xi:fallback><default>fallback content</default></xi:fallback>
            </xi:include>
        </root>"#;
        let doc = parse(xml);
        // Empty resolver — `missing.xml` won't be found.
        let out = process_xincludes(&doc, &opts_with_resolver(HashMap::new()))
            .unwrap();
        let root = out.root();
        let has_default = root.children().any(|c| {
            c.kind == NodeKind::Element && c.name() == "default"
        });
        assert!(
            has_default,
            "fallback content should replace the failed include"
        );
    }

    #[test]
    fn xinclude_no_resolver_errors_on_include() {
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="x.xml"/></root>"#;
        let doc = parse(xml);
        let opts = XIncludeOptions::new();
        let err = process_xincludes(&doc, &opts).expect_err("no resolver");
        assert!(err.message.contains("no resolver"), "got: {}", err.message);
    }

    #[test]
    fn xinclude_recursive_processing() {
        let mut docs = HashMap::new();
        docs.insert(
            "outer.xml".to_string(),
            br#"<wrap xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="inner.xml"/></wrap>"#.to_vec(),
        );
        docs.insert("inner.xml".to_string(), b"<leaf>x</leaf>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="outer.xml"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        // Walk the tree to confirm the leaf is reachable.
        let root = out.root();
        let wrap = root.children().next().unwrap();
        assert_eq!(wrap.name(), "wrap");
        let leaf = wrap.children().next().unwrap();
        assert_eq!(leaf.name(), "leaf");
        assert_eq!(leaf.text_content(), Some("x"));
    }

    #[test]
    fn xinclude_cycle_detected() {
        let mut docs = HashMap::new();
        docs.insert(
            "a.xml".to_string(),
            br#"<a xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="a.xml"/></a>"#.to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="a.xml"/></root>"#;
        let doc = parse(xml);
        let err = process_xincludes(&doc, &opts_with_resolver(docs))
            .expect_err("cycle");
        assert!(
            err.message.contains("cycle") || err.message.contains("depth"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn xinclude_max_depth_enforced() {
        let mut docs = HashMap::new();
        for (i, next) in [(1, 2), (2, 3), (3, 4), (4, 5), (5, 6)] {
            docs.insert(
                format!("d{i}.xml"),
                format!(
                    r#"<l xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="d{next}.xml"/></l>"#
                )
                .into_bytes(),
            );
        }
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="d1.xml"/></root>"#;
        let doc = parse(xml);
        let opts = XIncludeOptions {
            max_depth: 2,
            ..opts_with_resolver(docs)
        };
        let err = process_xincludes(&doc, &opts).expect_err("depth limit");
        assert!(err.message.contains("depth"), "got: {}", err.message);
    }

    #[test]
    fn xinclude_no_xi_elements_is_noop() {
        let xml = "<r><a/><b/></r>";
        let doc = parse(xml);
        let out = process_xincludes(&doc, &XIncludeOptions::new()).unwrap();
        // Tree shape preserved.
        let root = out.root();
        assert_eq!(root.name(), "r");
        let kids: Vec<&str> = root.children().map(|c| c.name()).collect();
        assert_eq!(kids, vec!["a", "b"]);
    }

    #[test]
    fn xinclude_unsupported_parse_mode_errors() {
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="x" parse="binary"/></root>"#;
        let doc = parse(xml);
        let err = process_xincludes(&doc, &opts_with_resolver(HashMap::new()))
            .expect_err("bad parse mode");
        assert!(err.message.contains("parse"), "got: {}", err.message);
    }

    // ── arena-specific tests ────────────────────────────────────────────

    #[test]
    fn xinclude_preserves_surrounding_siblings() {
        // Make sure the splice doesn't disturb other children.
        let mut docs = HashMap::new();
        docs.insert("p.xml".to_string(), b"<inc/>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <before/>
            <xi:include href="p.xml"/>
            <after/>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let names: Vec<&str> = out
            .root()
            .children()
            .filter(|c| c.kind == NodeKind::Element)
            .map(|c| c.name())
            .collect();
        assert_eq!(names, vec!["before", "inc", "after"]);
    }

    #[test]
    fn xinclude_copies_attributes_on_other_elements() {
        // Ensure deep-copy preserves attributes on non-include elements.
        let mut docs = HashMap::new();
        docs.insert("p.xml".to_string(), b"<i/>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude" id="r" class="c"><xi:include href="p.xml"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let root = out.root();
        let attrs: Vec<(&str, &str)> = root
            .attributes()
            .filter(|a| a.name() != "xmlns:xi")
            .map(|a| (a.name(), a.value()))
            .collect();
        // Order preserved; xmlns:xi may or may not appear in the iter
        // depending on parser path — filter for clarity.
        assert!(attrs.iter().any(|&(n, v)| n == "id" && v == "r"));
        assert!(attrs.iter().any(|&(n, v)| n == "class" && v == "c"));
    }

    #[test]
    fn xinclude_returns_independent_document() {
        // Output document must be a separate arena that survives the
        // input being dropped.
        let mut docs = HashMap::new();
        docs.insert("p.xml".to_string(), b"<inc>hi</inc>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="p.xml"/></root>"#;
        let opts = opts_with_resolver(docs);
        let out = {
            let doc = parse(xml);
            process_xincludes(&doc, &opts).unwrap()
            // `doc` drops here.
        };
        assert_eq!(out.root().name(), "root");
        let inc = out.root().children().next().unwrap();
        assert_eq!(inc.name(), "inc");
        assert_eq!(inc.text_content(), Some("hi"));
    }

    #[test]
    fn xinclude_xml_decl_fields_preserved() {
        let mut docs = HashMap::new();
        docs.insert("p.xml".to_string(), b"<x/>".to_vec());
        let xml = r#"<?xml version="1.1" encoding="UTF-8" standalone="yes"?><root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="p.xml"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        assert_eq!(out.version, "1.1");
        assert_eq!(out.encoding, "UTF-8");
        assert_eq!(out.standalone, Some(true));
    }

    #[test]
    fn xinclude_text_mode_does_not_parse_xml() {
        // parse="text" content should be inserted as-is, even if it
        // contains XML-like syntax — verifies we route to the text path.
        let mut docs = HashMap::new();
        docs.insert(
            "snippet.txt".to_string(),
            b"<not-parsed/>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="snippet.txt" parse="text"/></root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let kids: Vec<_> = out.root().children().collect();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].kind, NodeKind::Text);
        assert_eq!(kids[0].content(), "<not-parsed/>");
    }

    #[test]
    fn xinclude_fallback_with_nested_include() {
        // Per XInclude § 4.4, xi:include inside xi:fallback should also
        // get processed.
        let mut docs = HashMap::new();
        docs.insert("ok.xml".to_string(), b"<from-fallback/>".to_vec());
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="missing.xml">
                <xi:fallback><xi:include href="ok.xml"/></xi:fallback>
            </xi:include>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let has_inner = out
            .root()
            .children()
            .any(|c| c.kind == NodeKind::Element && c.name() == "from-fallback");
        assert!(
            has_inner,
            "xi:include nested in fallback should be expanded"
        );
    }

    // ── XPointer fragment selection ────────────────────────────────

    /// `xpointer(EXPR)` — the inner expression is XPath 1.0.
    /// Selects a subtree from the included doc rather than the
    /// whole root.
    #[test]
    fn xinclude_xpointer_xpath_selects_subtree() {
        let mut docs = HashMap::new();
        docs.insert(
            "part.xml".to_string(),
            b"<a><b><c>match</c></b><b><c>skip</c></b></a>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="part.xml" xpointer="xpointer(/a/b[1]/c)"/>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let included: Vec<_> = out.root().children()
            .filter(|c| c.kind == NodeKind::Element)
            .collect();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].name(), "c");
        assert_eq!(included[0].text_content(), Some("match"));
    }

    /// `element(/1/2/3)` — ChildSequence scheme.  Walks element
    /// children by 1-based index.
    #[test]
    fn xinclude_xpointer_element_child_sequence() {
        let mut docs = HashMap::new();
        docs.insert(
            "part.xml".to_string(),
            b"<a><b id='b1'/><b id='b2'><c id='c1'/><c id='c2'/></b></a>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="part.xml" xpointer="element(/1/2/2)"/>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        // /1 → <a>; /1/2 → second <b>; /1/2/2 → second <c id="c2">.
        let included: Vec<_> = out.root().children()
            .filter(|c| c.kind == NodeKind::Element)
            .collect();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].name(), "c");
        let id_attr = included[0].attributes()
            .find(|a| a.name() == "id")
            .map(|a| a.value());
        assert_eq!(id_attr, Some("c2"));
    }

    /// `element(NAME)` — fragment-id lookup by `id` attribute.
    #[test]
    fn xinclude_xpointer_element_fragment_id() {
        let mut docs = HashMap::new();
        docs.insert(
            "part.xml".to_string(),
            b"<a><b id='hit'><inner/></b><b id='miss'/></a>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="part.xml" xpointer="element(hit)"/>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let included: Vec<_> = out.root().children()
            .filter(|c| c.kind == NodeKind::Element)
            .collect();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].name(), "b");
        // The returned <b id='hit'> has its <inner/> child.
        let has_inner = included[0].children()
            .any(|c| c.is_element() && c.name() == "inner");
        assert!(has_inner);
    }

    /// Bare-name xpointer is shorthand for fragment-id lookup.
    #[test]
    fn xinclude_xpointer_bare_name_is_id_lookup() {
        let mut docs = HashMap::new();
        docs.insert(
            "part.xml".to_string(),
            b"<a><b id='target'>hit</b><b id='other'/></a>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="part.xml" xpointer="target"/>
        </root>"#;
        let doc = parse(xml);
        let out = process_xincludes(&doc, &opts_with_resolver(docs)).unwrap();
        let included: Vec<_> = out.root().children()
            .filter(|c| c.kind == NodeKind::Element)
            .collect();
        assert_eq!(included.len(), 1);
        assert_eq!(included[0].text_content(), Some("hit"));
    }

    /// An xpointer that matches nothing surfaces an error (caller
    /// can wrap in `xi:fallback` for graceful degradation).
    #[test]
    fn xinclude_xpointer_no_match_is_error() {
        let mut docs = HashMap::new();
        docs.insert(
            "part.xml".to_string(),
            b"<a><b/></a>".to_vec(),
        );
        let xml = r#"<root xmlns:xi="http://www.w3.org/2001/XInclude">
            <xi:include href="part.xml" xpointer="xpointer(/no-such-node)"/>
        </root>"#;
        let doc = parse(xml);
        let result = process_xincludes(&doc, &opts_with_resolver(docs));
        assert!(result.is_err(),
            "xpointer matching nothing should error so fallback can fire");
    }
}
