#![forbid(unsafe_code)]

//! Canonical XML serialization (W3C C14N) for the arena DOM.
//!
//! Arena-backed counterpart to [`crate::canonical`].  Same semantics, same
//! options, same byte-for-byte output — but operates on
//! [`sup_xml_tree::dom::Document`] / [`sup_xml_tree::dom::Node`].
//!
//! Two algorithms are supported:
//!
//! - **Canonical XML 1.0** — [W3C `xml-c14n`](https://www.w3.org/TR/xml-c14n).
//! - **Exclusive Canonical XML 1.0** — [W3C `xml-exc-c14n`](https://www.w3.org/TR/xml-exc-c14n).
//!
//! See the legacy [`crate::canonical`] module docs for the full background and
//! known divergences from libxml2.

use std::collections::HashSet;
use std::io::{self, Write};

use sup_xml_tree::dom::{Attribute, Document, Node, NodeKind};

// ── option types ─────────────────────────────────────────────────────────────

/// Options controlling the canonicalization algorithm.
#[derive(Debug, Clone)]
pub struct CanonicalizeOptions {
    /// Which canonicalization algorithm to use.
    pub mode: C14nMode,
    /// When `true`, comment nodes are included in the output (per
    /// the `#WithComments` variant of each algorithm).  Default
    /// `false` — comments are omitted, matching the most common
    /// signature workflows.
    pub with_comments: bool,
}

impl Default for CanonicalizeOptions {
    fn default() -> Self {
        Self {
            mode: C14nMode::C14n10,
            with_comments: false,
        }
    }
}

/// Selects which W3C canonicalization algorithm to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C14nMode {
    /// Canonical XML 1.0 (`http://www.w3.org/TR/2001/REC-xml-c14n-20010315`).
    /// Renders every namespace declaration in scope at the
    /// canonicalization root.  Use for whole-document
    /// canonicalization or fragments that should preserve their
    /// inherited namespace context.
    C14n10,
    /// Exclusive Canonical XML 1.0
    /// (`http://www.w3.org/2001/10/xml-exc-c14n#`).  Only renders
    /// namespace declarations *visibly used* by the canonicalized
    /// subtree, plus any prefixes in `inclusive_prefixes`.
    /// Required by SAML, WS-Security, XAdES.
    ExcC14n10 {
        /// Namespace prefixes to render even when not visibly used —
        /// the `InclusiveNamespaces PrefixList` from the spec.
        /// Empty list (the default in most uses) means "exclusive
        /// strictly per the algorithm."  Use the empty string `""`
        /// to force the default namespace into the inclusive set.
        inclusive_prefixes: Vec<String>,
    },
}

// ── public surface ───────────────────────────────────────────────────────────

/// What the [`canonicalize_with`] visibility predicate is being asked about.
///
/// Mirrors the targets libxml2's `xmlC14NIsVisibleCallback` is invoked
/// against: element/text/comment/PI nodes, and individual attributes.
/// Namespace declarations are emitted per the C14N algorithm rules and
/// are not surfaced through this predicate.
pub enum VisitTarget<'a, 'doc> {
    /// An element, text, CDATA, comment, PI, or entity-reference node.
    /// Returning `false` for an element causes the entire subtree to be
    /// skipped (XML-DSig "subtree exclusion" semantics); returning
    /// `false` for a non-element node skips just that node.
    Node(&'a Node<'doc>),
    /// An individual attribute on an element.  Returning `false`
    /// causes that attribute to be omitted from the element's
    /// canonical serialization.
    Attribute(&'a Attribute<'doc>),
}

/// Predicate that includes every visited target.  The default used by
/// [`canonicalize_to_bytes`] and [`canonicalize_node_to_bytes`].
#[inline]
pub fn include_all(_: VisitTarget<'_, '_>) -> bool {
    true
}

/// Stream the canonical form of an entire [`Document`] into `out`.
///
/// Bytes are written incrementally as the walker produces them, so
/// callers wiring a hash context (XML-DSig) or a network sink see
/// each chunk without ever materializing the full canonical form.
///
/// `is_visible` filters which nodes and attributes appear in the
/// output.  See [`VisitTarget`] for semantics; pass [`include_all`]
/// to emit everything.
pub fn canonicalize_with<W, F>(
    doc: &Document,
    opts: &CanonicalizeOptions,
    out: &mut W,
    is_visible: F,
) -> io::Result<()>
where
    W: Write,
    F: Fn(VisitTarget<'_, '_>) -> bool,
{
    let mut ctx = NsContext::new();
    // Walk the document-level node chain: prolog comments/PIs, the
    // document element, then epilogue comments/PIs (linked as the
    // root's prev/next siblings).  Per the C14N spec, document-level
    // comments/PIs preceding the document element are each followed
    // by a newline, and those following it are each preceded by one;
    // comments appear only when `with_comments` is set (PIs always).
    let root = doc.root();
    let root_ptr = root as *const Node<'_>;
    let mut head = root;
    while let Some(prev) = head.prev_sibling.get() {
        head = prev;
    }
    let mut seen_root = false;
    let mut cur = Some(head);
    while let Some(n) = cur {
        if std::ptr::eq(n as *const Node<'_>, root_ptr) {
            write_node(out, n, &mut ctx, opts, &is_visible)?;
            seen_root = true;
        } else if !matches!(n.kind, NodeKind::Comment) || opts.with_comments {
            if seen_root {
                out.write_all(b"\n")?;
                write_node(out, n, &mut ctx, opts, &is_visible)?;
            } else {
                write_node(out, n, &mut ctx, opts, &is_visible)?;
                out.write_all(b"\n")?;
            }
        }
        cur = n.next_sibling.get();
    }
    Ok(())
}

/// Stream the canonical form of a single subtree into `out`.  The
/// supplied `node` is treated as the canonicalization root — no
/// inherited namespace context from its ancestors is included.
pub fn canonicalize_node_with<W, F>(
    node: &Node<'_>,
    opts: &CanonicalizeOptions,
    out: &mut W,
    is_visible: F,
) -> io::Result<()>
where
    W: Write,
    F: Fn(VisitTarget<'_, '_>) -> bool,
{
    let mut ctx = NsContext::new();
    write_node(out, node, &mut ctx, opts, &is_visible)
}

/// Canonicalize an entire arena [`Document`] into an in-memory `Vec`.
///
/// Convenience wrapper around [`canonicalize_with`] for callers that
/// want the full canonical form materialized in memory.  For streaming
/// workloads (hashing, network I/O) use [`canonicalize_with`] directly.
pub fn canonicalize_to_bytes(doc: &Document, opts: &CanonicalizeOptions) -> Vec<u8> {
    let mut buf = Vec::with_capacity(estimate_capacity(doc));
    canonicalize_with(doc, opts, &mut buf, include_all)
        .expect("writes into Vec<u8> are infallible");
    buf
}

/// Canonicalize a single arena node and its descendants into an
/// in-memory `Vec`.  The node is treated as the canonicalization
/// root — no inherited namespace context from ancestors is included.
pub fn canonicalize_node_to_bytes(node: &Node<'_>, opts: &CanonicalizeOptions) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2048);
    canonicalize_node_with(node, opts, &mut buf, include_all)
        .expect("writes into Vec<u8> are infallible");
    buf
}

// ── namespace context tracking ───────────────────────────────────────────────

/// Stack of in-scope namespace bindings + record of which bindings have
/// already been rendered in the output.  C14N de-dup works on the rendered
/// set, not the in-scope set.
struct NsContext {
    frames: Vec<NsFrame>,
}

struct NsFrame {
    /// (prefix, uri) bindings declared at this element.  `prefix == ""` means
    /// the default namespace (`xmlns="…"`).
    declared: Vec<(String, String)>,
    /// (prefix, uri) bindings actually rendered to output at this element.
    rendered: Vec<(String, String)>,
}

impl NsContext {
    fn new() -> Self {
        Self { frames: Vec::with_capacity(16) }
    }

    fn push_frame(&mut self) {
        self.frames.push(NsFrame {
            declared: Vec::new(),
            rendered: Vec::new(),
        });
    }

    fn pop_frame(&mut self) {
        self.frames.pop();
    }

    fn declare(&mut self, prefix: &str, uri: &str) {
        if let Some(frame) = self.frames.last_mut() {
            frame.declared.push((prefix.to_string(), uri.to_string()));
        }
    }

    fn record_rendered(&mut self, prefix: &str, uri: &str) {
        if let Some(frame) = self.frames.last_mut() {
            frame.rendered.push((prefix.to_string(), uri.to_string()));
        }
    }

    /// Walk frame stack newest-to-oldest looking up `prefix`.
    fn lookup(&self, prefix: &str) -> Option<&str> {
        for frame in self.frames.iter().rev() {
            for (p, u) in frame.declared.iter().rev() {
                if p == prefix {
                    return Some(u);
                }
            }
        }
        // Built-in: xml prefix.
        if prefix == "xml" {
            return Some("http://www.w3.org/XML/1998/namespace");
        }
        None
    }

    /// True if (prefix, uri) was already rendered above this element.
    fn already_rendered(&self, prefix: &str, uri: &str) -> bool {
        let last = self.frames.len();
        if last == 0 {
            return false;
        }
        for frame in &self.frames[..last - 1] {
            for (p, u) in &frame.rendered {
                if p == prefix && u == uri {
                    return true;
                }
            }
        }
        false
    }

    /// True if `prefix` was rendered with a *different* URI in some ancestor —
    /// the current binding overrides it and must be rendered.  C14N 1.0 § 2.3.
    fn ancestor_rendered_different(&self, prefix: &str, uri: &str) -> bool {
        let last = self.frames.len();
        if last == 0 {
            return false;
        }
        for frame in self.frames[..last - 1].iter().rev() {
            for (p, u) in frame.rendered.iter().rev() {
                if p == prefix {
                    return u != uri;
                }
            }
        }
        false
    }
}

// ── walker ───────────────────────────────────────────────────────────────────

fn write_node(
    out: &mut dyn Write,
    node: &Node<'_>,
    ctx: &mut NsContext,
    opts: &CanonicalizeOptions,
    is_visible: &dyn Fn(VisitTarget<'_, '_>) -> bool,
) -> io::Result<()> {
    if !is_visible(VisitTarget::Node(node)) {
        // Subtree skip for elements; single-node skip for everything else.
        return Ok(());
    }
    match node.kind {
        NodeKind::Element => write_element(out, node, ctx, opts, is_visible)?,
        NodeKind::Text => write_text_canonical(out, node.content())?,
        NodeKind::CData => {
            // CDATA sections become regular text in canonical form.
            write_text_canonical(out, node.content())?;
        }
        NodeKind::Comment => {
            if opts.with_comments {
                out.write_all(b"<!--")?;
                out.write_all(node.content().as_bytes())?;
                out.write_all(b"-->")?;
            }
        }
        // NodeKind::Attribute is the discriminant used on
        // Attribute<'_>::kind (c-abi build) to satisfy libxml2's
        // generic xmlNode/xmlAttr cross-casting.  It never appears
        // as a real Node::kind — Attributes are walked through
        // node.attributes(), not as children.
        // The DTD (internal subset) is not part of the C14N document
        // subset — neither the node itself nor its declarations are
        // emitted in canonical form.
        NodeKind::DtdDecl => {}
        NodeKind::Dtd => {}
        NodeKind::Attribute => unreachable!("Attribute kind never appears on a Node"),
        NodeKind::Document  => unreachable!("Document kind never appears on a Node"),
        // DocumentFragment is a transient container only produced by
        // `xmlNewDocFragment` in the compat shim.  It never appears as
        // an attached node in a real canonicalization target — if
        // someone asks us to c14n it, walking its children directly is
        // the closest sensible behaviour.
        NodeKind::DocumentFragment => {
            for c in node.children() {
                write_node(out, c, ctx, opts, is_visible)?;
            }
        }
        NodeKind::EntityRef => {
            // C14N § 2.3 says entity references SHOULD be replaced by
            // their replacement text before canonicalization (the
            // canonicalization input is a post-expansion XPath data
            // model).  If a doc reaches the canonicalizer with
            // EntityRef nodes still in the tree (parsed with
            // resolve_entities=false), emit the literal `&name;`
            // form — best-effort byte-stable round-trip.  Callers
            // who need spec-conformant C14N should re-parse with
            // resolve_entities=true.
            out.write_all(node.content().as_bytes())?;
        }
        NodeKind::Pi => {
            out.write_all(b"<?")?;
            out.write_all(node.name().as_bytes())?;
            let content = node.content();
            if !content.is_empty() {
                out.write_all(b" ")?;
                out.write_all(content.as_bytes())?;
            }
            out.write_all(b"?>")?;
        }
    }
    Ok(())
}

fn write_element(
    out: &mut dyn Write,
    el: &Node<'_>,
    ctx: &mut NsContext,
    opts: &CanonicalizeOptions,
    is_visible: &dyn Fn(VisitTarget<'_, '_>) -> bool,
) -> io::Result<()> {
    ctx.push_frame();

    // Scan namespace declarations (always in scope) and regular
    // attributes (filtered through is_visible).  In the c-abi build
    // namespace declarations live on the element's `ns_def` chain
    // (libxml2 convention) rather than mixed into the attribute list,
    // so we read them from there too.
    // Each entry: (ns_prefix, name, effective_prefix, local, value).
    // `ns_prefix` is the namespace-carried prefix prepended to a local
    // `name` (Some only in the c-abi/compat representation); `name` is
    // `attr.name()` (local there, full QName otherwise); `effective_prefix`
    // is for namespace lookup/visibility; `local` is the sort tiebreak.
    let mut regular_attrs: Vec<(Option<&str>, &str, Option<&str>, &str, &str)> = Vec::new();
    #[cfg(feature = "c-abi")]
    {
        let mut ns_cur = el.ns_def.get();
        while let Some(ns) = ns_cur {
            match ns.prefix() {
                None    => ctx.declare("",  ns.href()),
                Some(p) => ctx.declare(p,   ns.href()),
            }
            ns_cur = ns.next.get();
        }
    }
    for attr in el.attributes() {
        let aname: &str = attr.name();
        if aname == "xmlns" {
            ctx.declare("", attr.value());
        } else if let Some(rest) = aname.strip_prefix("xmlns:") {
            ctx.declare(rest, attr.value());
        } else if is_visible(VisitTarget::Attribute(attr)) {
            #[cfg(feature = "c-abi")]
            let ns_prefix: Option<&str> = attr.namespace.get().and_then(|ns| ns.prefix());
            #[cfg(not(feature = "c-abi"))]
            let ns_prefix: Option<&str> = None;
            let (name_prefix, local) = split_qname(aname);
            let eff_prefix = ns_prefix.or(name_prefix);
            regular_attrs.push((ns_prefix, aname, eff_prefix, local, attr.value()));
        }
    }

    // Determine the element's prefix and the QName to serialize.  Two
    // representations reach here:
    //   * compat-parsed (c-abi, libxml2 convention): `name` is the local
    //     part and the prefix lives on the attached namespace.
    //   * core-parsed / non-c-abi: `name` already carries the prefix and
    //     the namespace object may be absent.
    // `ns_prefix` is the prefix to prepend to a *local* name (Some only
    // when the namespace carries it); `effective_prefix` is the prefix
    // for visible-namespace accounting, taken from the namespace or the
    // QName itself.
    let elem_name: &str = el.name();
    #[cfg(feature = "c-abi")]
    let ns_prefix: Option<&str> = el.namespace.get().and_then(|ns| ns.prefix());
    #[cfg(not(feature = "c-abi"))]
    let ns_prefix: Option<&str> = None;
    let effective_prefix = ns_prefix.or_else(|| split_qname(elem_name).0);
    let visibly_used = collect_visibly_used(effective_prefix, &regular_attrs, opts);

    out.write_all(b"<")?;
    write_qname(out, ns_prefix, elem_name)?;
    write_namespace_decls(out, ctx, opts, &visibly_used)?;
    write_attributes(out, ctx, &mut regular_attrs)?;
    out.write_all(b">")?;

    for child in el.children() {
        write_node(out, child, ctx, opts, is_visible)?;
    }

    // Canonical form always uses an explicit end tag — never `<e/>`.
    out.write_all(b"</")?;
    write_qname(out, ns_prefix, elem_name)?;
    out.write_all(b">")?;

    ctx.pop_frame();
    Ok(())
}

/// Compute the set of prefixes visibly used at this element.
fn collect_visibly_used(
    elem_prefix: Option<&str>,
    attrs: &[(Option<&str>, &str, Option<&str>, &str, &str)],
    opts: &CanonicalizeOptions,
) -> HashSet<String> {
    let mut used: HashSet<String> = HashSet::with_capacity(8);
    // Element's own prefix.  Unprefixed name → default namespace, "".
    used.insert(elem_prefix.unwrap_or("").to_string());

    // Per XML Names: unprefixed attributes are NOT in the default namespace.
    for (_ns_prefix, _name, eff_prefix, _local, _value) in attrs {
        if let Some(p) = eff_prefix {
            used.insert(p.to_string());
        }
    }

    if let C14nMode::ExcC14n10 { inclusive_prefixes } = &opts.mode {
        for p in inclusive_prefixes {
            used.insert(p.clone());
        }
    }

    used
}

/// Render namespace declarations.  C14N 1.0: every in-scope binding not yet
/// rendered (or overridden).  Exc-c14n: only visibly-used prefixes.
fn write_namespace_decls(
    out: &mut dyn Write,
    ctx: &mut NsContext,
    opts: &CanonicalizeOptions,
    visibly_used: &HashSet<String>,
) -> io::Result<()> {
    let mut to_render: Vec<(String, String)> = Vec::new();

    match &opts.mode {
        C14nMode::C14n10 => {
            // C14N § 2.3: walk in-scope namespaces (most recent binding per
            // prefix), render iff not already rendered above, or if it overrides.
            let mut seen_prefixes: HashSet<String> = HashSet::new();
            for frame in ctx.frames.iter().rev() {
                for (prefix, uri) in &frame.declared {
                    if seen_prefixes.insert(prefix.clone()) {
                        if uri.is_empty() && prefix.is_empty() {
                            // xmlns="" — only render if it overrides a non-empty
                            // default rendered above.
                            if ctx.ancestor_rendered_different(prefix, uri) {
                                to_render.push((prefix.clone(), uri.clone()));
                            }
                        } else if !ctx.already_rendered(prefix, uri) {
                            to_render.push((prefix.clone(), uri.clone()));
                        }
                    }
                }
            }
        }
        C14nMode::ExcC14n10 { .. } => {
            // exc-c14n § 3: for each visibly-used prefix, render its current
            // binding iff not already rendered above (or with a different URI).
            for prefix in visibly_used {
                let uri = ctx.lookup(prefix).unwrap_or("");
                if prefix.is_empty() && uri.is_empty() {
                    if ctx.ancestor_rendered_different(prefix, uri) {
                        to_render.push((prefix.clone(), String::new()));
                    }
                    continue;
                }
                // Skip the built-in `xml` prefix.
                if prefix == "xml" && uri == "http://www.w3.org/XML/1998/namespace" {
                    continue;
                }
                if !ctx.already_rendered(prefix, uri) {
                    to_render.push((prefix.clone(), uri.to_string()));
                }
            }
        }
    }

    // Sort: default namespace ("") first, then by prefix lexicographically.
    to_render.sort_by(|a, b| a.0.cmp(&b.0));

    for (prefix, uri) in &to_render {
        out.write_all(b" ")?;
        if prefix.is_empty() {
            out.write_all(b"xmlns=\"")?;
        } else {
            out.write_all(b"xmlns:")?;
            out.write_all(prefix.as_bytes())?;
            out.write_all(b"=\"")?;
        }
        write_attr_value_canonical(out, uri)?;
        out.write_all(b"\"")?;
        ctx.record_rendered(prefix, uri);
    }
    Ok(())
}

/// Render regular (non-xmlns) attributes in canonical sort order: namespace
/// URI (empty first), then local name.
fn write_attributes(
    out: &mut dyn Write,
    ctx: &NsContext,
    attrs: &mut [(Option<&str>, &str, Option<&str>, &str, &str)],
) -> io::Result<()> {
    // C14N sorts attributes by (namespace URI, local name); attributes
    // in no namespace (effective prefix None) sort before namespaced
    // ones, matching an empty URI.
    attrs.sort_by(|a, b| {
        let a_ns = a.2.and_then(|p| ctx.lookup(p)).unwrap_or("");
        let b_ns = b.2.and_then(|p| ctx.lookup(p)).unwrap_or("");
        match a_ns.cmp(b_ns) {
            std::cmp::Ordering::Equal => a.3.cmp(b.3),
            ord => ord,
        }
    });

    for (ns_prefix, name, _eff_prefix, _local, value) in attrs {
        out.write_all(b" ")?;
        write_qname(out, *ns_prefix, name)?;
        out.write_all(b"=\"")?;
        write_attr_value_canonical(out, value)?;
        out.write_all(b"\"")?;
    }
    Ok(())
}

/// Write a qualified name.  `ns_prefix` is the prefix carried by the
/// attached namespace and is prepended to a *local* `name`; callers pass
/// `None` when `name` already includes any prefix (core / non-c-abi).
fn write_qname(out: &mut dyn Write, ns_prefix: Option<&str>, name: &str) -> io::Result<()> {
    if let Some(p) = ns_prefix {
        out.write_all(p.as_bytes())?;
        out.write_all(b":")?;
    }
    out.write_all(name.as_bytes())
}

// ── canonical character escaping ─────────────────────────────────────────────

/// Escape text per C14N § 1.3.3: `&`, `<`, `>`, `\r`.
fn write_text_canonical(out: &mut dyn Write, s: &str) -> io::Result<()> {
    // Coalesce runs of pass-through bytes into a single write to keep
    // the per-byte virtual-call cost off the hot path.
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement: &[u8] = match b {
            b'&'  => b"&amp;",
            b'<'  => b"&lt;",
            b'>'  => b"&gt;",
            b'\r' => b"&#xD;",
            _     => continue,
        };
        if start < i {
            out.write_all(&bytes[start..i])?;
        }
        out.write_all(replacement)?;
        start = i + 1;
    }
    if start < bytes.len() {
        out.write_all(&bytes[start..])?;
    }
    Ok(())
}

/// Escape attribute value per C14N § 1.3.3: `&`, `<`, `"`, `\t`, `\n`, `\r`.
fn write_attr_value_canonical(out: &mut dyn Write, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement: &[u8] = match b {
            b'&'  => b"&amp;",
            b'<'  => b"&lt;",
            b'"'  => b"&quot;",
            b'\t' => b"&#x9;",
            b'\n' => b"&#xA;",
            b'\r' => b"&#xD;",
            _     => continue,
        };
        if start < i {
            out.write_all(&bytes[start..i])?;
        }
        out.write_all(replacement)?;
        start = i + 1;
    }
    if start < bytes.len() {
        out.write_all(&bytes[start..])?;
    }
    Ok(())
}

// ── small helpers ────────────────────────────────────────────────────────────

/// Split an XML qualified name into (prefix, local).  Doesn't validate.
fn split_qname(name: &str) -> (Option<&str>, &str) {
    match name.find(':') {
        Some(idx) => (Some(&name[..idx]), &name[idx + 1..]),
        None => (None, name),
    }
}

fn estimate_capacity(_doc: &Document) -> usize {
    // Rough heuristic — canonical form is usually 1.0–1.5× source size.
    4096
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_str;
    use crate::options::ParseOptions;

    fn c14n(xml: &str, mode: C14nMode, with_comments: bool) -> String {
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let bytes = canonicalize_to_bytes(
            &doc,
            &CanonicalizeOptions { mode, with_comments },
        );
        String::from_utf8(bytes).expect("c14n produces UTF-8")
    }

    fn c14n10(xml: &str) -> String {
        c14n(xml, C14nMode::C14n10, false)
    }

    fn exc_c14n10(xml: &str) -> String {
        c14n(xml, C14nMode::ExcC14n10 { inclusive_prefixes: vec![] }, false)
    }

    // ── basic shape ──────────────────────────────────────────────────────────

    #[test]
    fn simple_element_round_trips_to_explicit_close() {
        let out = c14n10("<r/>");
        assert_eq!(out, "<r></r>");
    }

    #[test]
    fn empty_element_with_attrs() {
        let out = c14n10(r#"<r a="1"/>"#);
        assert_eq!(out, r#"<r a="1"></r>"#);
    }

    #[test]
    fn xml_decl_dropped() {
        let out = c14n10(r#"<?xml version="1.0"?><r/>"#);
        assert!(!out.contains("<?xml"));
        assert_eq!(out, "<r></r>");
    }

    #[test]
    fn attribute_value_quotes_canonical() {
        let out = c14n10("<r a='1'/>");
        assert_eq!(out, r#"<r a="1"></r>"#);
    }

    // ── attribute escaping ───────────────────────────────────────────────────

    #[test]
    fn attribute_value_escapes_quote_and_control() {
        let out = c14n10("<r a=\"a&amp;b&lt;c&#x9;d\"/>");
        assert_eq!(out, r#"<r a="a&amp;b&lt;c&#x9;d"></r>"#);
    }

    // ── text escaping ────────────────────────────────────────────────────────

    #[test]
    fn text_escapes_amp_lt_gt() {
        let out = c14n10("<r>a&amp;b&lt;c&gt;d</r>");
        assert_eq!(out, "<r>a&amp;b&lt;c&gt;d</r>");
    }

    #[test]
    fn text_does_not_escape_tab_newline() {
        let out = c14n10("<r>a\tb\nc</r>");
        assert_eq!(out, "<r>a\tb\nc</r>");
    }

    // ── attribute sort order ─────────────────────────────────────────────────

    #[test]
    fn attributes_sorted_lexicographically_when_no_namespace() {
        let out = c14n10(r#"<r z="1" a="2" m="3"/>"#);
        assert_eq!(out, r#"<r a="2" m="3" z="1"></r>"#);
    }

    #[test]
    fn namespace_decls_come_before_attributes() {
        let out = c14n10(r#"<r a="1" xmlns:b="urn:b" b:c="2"/>"#);
        assert_eq!(out, r#"<r xmlns:b="urn:b" a="1" b:c="2"></r>"#);
    }

    #[test]
    fn default_namespace_sorts_before_prefixed_namespace() {
        let out = c14n10(r#"<r xmlns:b="urn:b" xmlns="urn:default"/>"#);
        assert_eq!(out, r#"<r xmlns="urn:default" xmlns:b="urn:b"></r>"#);
    }

    // ── namespace de-duplication ─────────────────────────────────────────────

    #[test]
    fn c14n10_does_not_repeat_inherited_namespace() {
        let out = c14n10(r#"<outer xmlns:a="urn:a"><inner/></outer>"#);
        assert_eq!(out, r#"<outer xmlns:a="urn:a"><inner></inner></outer>"#);
    }

    #[test]
    fn c14n10_renders_inherited_when_subtree_uses_prefix() {
        let out = c14n10(r#"<outer xmlns:a="urn:a"><a:inner/></outer>"#);
        assert_eq!(out, r#"<outer xmlns:a="urn:a"><a:inner></a:inner></outer>"#);
    }

    // ── exc-c14n: only renders visibly-used prefixes ─────────────────────────

    #[test]
    fn exc_c14n_omits_unused_inherited_namespace() {
        let out = exc_c14n10(r#"<a:outer xmlns:a="urn:a"><inner/></a:outer>"#);
        assert_eq!(out, r#"<a:outer xmlns:a="urn:a"><inner></inner></a:outer>"#);
    }

    #[test]
    fn exc_c14n_renders_namespace_for_used_prefix() {
        let out = exc_c14n10(r#"<outer xmlns:a="urn:a"><a:inner/></outer>"#);
        assert_eq!(out, r#"<outer><a:inner xmlns:a="urn:a"></a:inner></outer>"#);
    }

    #[test]
    fn exc_c14n_inclusive_prefix_list() {
        let doc = parse_str(
            r#"<outer xmlns:a="urn:a"><inner/></outer>"#,
            &ParseOptions::default(),
        )
        .unwrap();
        let bytes = canonicalize_to_bytes(
            &doc,
            &CanonicalizeOptions {
                mode: C14nMode::ExcC14n10 {
                    inclusive_prefixes: vec!["a".into()],
                },
                with_comments: false,
            },
        );
        let s = String::from_utf8(bytes).unwrap();
        assert_eq!(
            s,
            r#"<outer xmlns:a="urn:a"><inner></inner></outer>"#,
            "inclusive-prefixes adds `a` to the visibly-used set on both elements, but standard de-dup means inner doesn't re-render an inherited binding"
        );
    }

    // ── comments ─────────────────────────────────────────────────────────────

    #[test]
    fn comments_omitted_by_default() {
        let out = c14n10("<r><!-- hi --></r>");
        assert_eq!(out, "<r></r>");
    }

    #[test]
    fn comments_included_when_with_comments() {
        let out = c14n("<r><!-- hi --></r>", C14nMode::C14n10, true);
        assert_eq!(out, "<r><!-- hi --></r>");
    }

    // ── PIs ──────────────────────────────────────────────────────────────────

    #[test]
    fn processing_instruction_preserved() {
        let out = c14n10(r#"<r><?target value?></r>"#);
        assert_eq!(out, r#"<r><?target value?></r>"#);
    }

    // ── CDATA → text ─────────────────────────────────────────────────────────

    #[test]
    fn cdata_section_becomes_text() {
        let out = c14n10("<r><![CDATA[<raw>&]]></r>");
        assert_eq!(out, "<r>&lt;raw&gt;&amp;</r>");
    }

    // ── idempotency ──────────────────────────────────────────────────────────

    #[test]
    fn c14n_is_idempotent() {
        let xml = r#"<r xmlns:b='urn:b' a='1' z='2' b:x="hi"><child/></r>"#;
        let once = c14n10(xml);
        let twice = c14n10(&once);
        assert_eq!(once, twice, "c14n must be idempotent");
    }

    #[test]
    fn exc_c14n_is_idempotent() {
        let xml = r#"<r xmlns:b='urn:b' a='1' z='2' b:x="hi"><b:child/></r>"#;
        let once = exc_c14n10(xml);
        let twice = exc_c14n10(&once);
        assert_eq!(once, twice, "exc-c14n must be idempotent");
    }

    // ── canonicalize_node_to_bytes ─────────────────────────────────────

    #[test]
    fn canonicalize_node_works_on_subtree() {
        let doc = parse_str(
            r#"<root><target a="1"><child/></target></root>"#,
            &ParseOptions::default(),
        )
        .unwrap();
        // Find <target> (first child of root) and canonicalize just its subtree.
        let target = doc.root().first_child.get().expect("target child");
        let bytes = canonicalize_node_to_bytes(target, &CanonicalizeOptions::default());
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"<target a="1"><child></child></target>"#
        );
    }

    // ── streaming + visibility ────────────────────────────────────────

    #[test]
    fn canonicalize_with_matches_canonicalize_to_bytes() {
        // Whatever shape the one-shot variant produces, the streaming
        // variant with include_all must produce byte-identical output.
        let xml = r#"<r xmlns:b='urn:b' a='1' z='2' b:x="hi"><c/><!--note--></r>"#;
        let doc = parse_str(xml, &ParseOptions::default()).unwrap();
        let opts = CanonicalizeOptions { mode: C14nMode::C14n10, with_comments: true };

        let bulk = canonicalize_to_bytes(&doc, &opts);
        let mut streamed = Vec::new();
        canonicalize_with(&doc, &opts, &mut streamed, include_all).unwrap();
        assert_eq!(bulk, streamed);
    }

    #[test]
    fn canonicalize_with_skips_subtree_for_hidden_element() {
        let doc = parse_str(
            r#"<r><keep>x</keep><secret><nested/></secret><also/></r>"#,
            &ParseOptions::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        canonicalize_with(&doc, &CanonicalizeOptions::default(), &mut out, |t| {
            match t {
                VisitTarget::Node(n) => n.name() != "secret",
                VisitTarget::Attribute(_) => true,
            }
        })
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "<r><keep>x</keep><also></also></r>");
    }

    #[test]
    fn canonicalize_with_skips_individual_attribute() {
        let doc = parse_str(
            r#"<r keep="1" drop="2" also="3"/>"#,
            &ParseOptions::default(),
        )
        .unwrap();
        let mut out = Vec::new();
        canonicalize_with(&doc, &CanonicalizeOptions::default(), &mut out, |t| {
            match t {
                VisitTarget::Attribute(a) => a.name() != "drop",
                VisitTarget::Node(_) => true,
            }
        })
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, r#"<r also="3" keep="1"></r>"#);
    }

    #[test]
    fn canonicalize_with_streams_incrementally() {
        // Sink that counts the number of distinct write() calls — proves
        // the walker is producing bytes incrementally rather than buffering
        // the full canonical form before handing it off.
        struct CountingSink {
            buf: Vec<u8>,
            chunks: usize,
        }
        impl std::io::Write for CountingSink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if !b.is_empty() {
                    self.chunks += 1;
                    self.buf.extend_from_slice(b);
                }
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }

        let doc = parse_str(
            r#"<r><a/><b/><c/></r>"#,
            &ParseOptions::default(),
        )
        .unwrap();
        let mut sink = CountingSink { buf: Vec::new(), chunks: 0 };
        canonicalize_with(
            &doc, &CanonicalizeOptions::default(), &mut sink, include_all,
        )
        .unwrap();
        assert!(
            sink.chunks > 1,
            "expected multiple incremental writes, got {} for {:?}",
            sink.chunks,
            String::from_utf8_lossy(&sink.buf),
        );
        assert_eq!(
            String::from_utf8(sink.buf).unwrap(),
            "<r><a></a><b></b><c></c></r>",
        );
    }

    #[test]
    fn canonicalize_with_propagates_sink_errors() {
        // Sink that fails on the second write — verifies the walker
        // propagates io::Error rather than silently truncating.
        struct FlakySink { remaining: usize }
        impl std::io::Write for FlakySink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                if self.remaining == 0 {
                    return Err(std::io::Error::other("boom"));
                }
                self.remaining -= 1;
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let doc = parse_str(r#"<r><a/><b/></r>"#, &ParseOptions::default()).unwrap();
        let mut sink = FlakySink { remaining: 1 };
        let err = canonicalize_with(
            &doc, &CanonicalizeOptions::default(), &mut sink, include_all,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "boom");
    }
}
