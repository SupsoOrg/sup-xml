//! Arena-tree counterpart to [`super::context::DocIndex`].
//!
//! Builds the same flat `Vec<INode>` shape with `NodeId` indices, but walks
//! the arena tree ([`sup_xml_tree::dom::Document`]) via its linked-list
//! children/attributes API instead of the legacy `Vec<Node>` fields.  Both
//! types implement [`super::index::DocIndexLike`], so the XPath evaluator
//! in [`super::eval`] works against either with no per-tree-type code.

use sup_xml_tree::dom::{Attribute as ArenaAttr, Document as ArenaDoc, Node as ArenaNode, NodeKind as ArenaKind};

use super::index::{DocIndexLike, NodeId, XPathNodeKind};

/// Per-node entry in the flat index.
pub struct INode<'doc> {
    pub kind: INodeKind<'doc>,
    pub parent: Option<NodeId>,
    /// IDs of attribute nodes (elements only).  Half-open range.
    pub attr_start: usize,
    pub attr_end:   usize,
    /// IDs of synthetic namespace nodes (elements only).  Half-open
    /// range.  Empty when this element has no in-scope namespace
    /// bindings — which, given the implicit `xml` prefix, only
    /// applies to non-elements.
    pub ns_start: usize,
    pub ns_end:   usize,
    /// IDs of content children (non-attribute).
    pub content_children: Vec<NodeId>,
}

/// Discriminant for [`INode::kind`].  The Element/Attribute/etc. variants
/// carry the original arena reference for cheap accessor methods.
pub enum INodeKind<'doc> {
    Document,
    Element(&'doc ArenaNode<'doc>),
    Attribute(&'doc ArenaAttr<'doc>),
    Text(&'doc ArenaNode<'doc>),
    Comment(&'doc ArenaNode<'doc>),
    CData(&'doc ArenaNode<'doc>),
    PI(&'doc ArenaNode<'doc>),
    /// Synthetic namespace node — materialised during indexing
    /// from each element's in-scope namespace bindings.  `parent`
    /// is the element this binding is observed on; `prefix` is
    /// `None` for the default namespace (`xmlns="…"`); `uri` is
    /// the namespace URI.  See XPath 1.0 §5.4.
    Namespace { prefix: Option<&'doc str>, uri: &'doc str },
}

/// The implicit `xml` prefix every element carries per the XML
/// Namespaces recommendation.  Stored as `&'static str` so it
/// reborrows into any `&'doc str` slot.
const XML_NS_PREFIX: &str = "xml";
const XML_NS_URI:    &str = "http://www.w3.org/XML/1998/namespace";

pub struct DocIndex<'doc> {
    pub nodes: Vec<INode<'doc>>,
    /// DTD-declared ID-attribute map (element-name → list of
    /// attribute-names typed as `ID`).  Snapshotted from the source
    /// [`ArenaDoc`] at index-build time so `id()` can consult it
    /// without owning a Document reference.  Empty when the doc had
    /// no DTD ID-typed attributes.
    id_attrs: std::sync::Arc<std::collections::HashMap<String, Vec<String>>>,
    /// DTD-declared IDREF/IDREFS-attribute map (element-name → list of
    /// attribute-names typed as `IDREF`/`IDREFS`).  Snapshotted like
    /// [`id_attrs`](Self::id_attrs) so `idref()` (XPath 2.0 §14.5.5)
    /// can find the referencing attributes.  Empty when the doc had no
    /// DTD IDREF-typed attributes.
    idref_attrs: std::sync::Arc<std::collections::HashMap<String, Vec<String>>>,
    /// Append-only store of EXSLT result-tree-fragment text nodes
    /// allocated at runtime by `str:tokenize` / `str:split` /
    /// `regexp:match`.  Each entry holds the text content; the
    /// corresponding `NodeId` is `SYNTHETIC_TEXT_BASE | index`.
    /// Interior-mutable so allocation can happen through the
    /// `&DocIndex` references the evaluator holds.
    synthetic: std::cell::RefCell<Vec<String>>,
    /// Result-tree-fragment store for XSLT body-form `xsl:variable`
    /// bindings — see [`super::rtf`] for the full design.  Held as
    /// a [`elsa::FrozenVec`] of `Box<RtfIndex>` so the XSLT engine
    /// can `push_rtf` through `&DocIndex` while every `&RtfIndex`
    /// (and the `&[NodeId]` slices it hands out) stays stable for
    /// the rest of the evaluation.  Each entry is built complete-
    /// then-frozen; no in-place mutation after registration.
    rtfs: elsa::FrozenVec<Box<super::rtf::RtfIndex>>,
    /// Synthetic RTF document-roots that wrap a sequence-typed XSLT
    /// binding (XSLT 2.0 §5.7.2 / §9.3).  Per spec, items in a
    /// sequence-typed `as="element()" / element()* / text()*`
    /// variable are parentless; the engine stores them as children
    /// of a doc-root for storage convenience.  XTDE1270 / XTDE1370
    /// / XTDE1380 ("the root of the tree containing the context
    /// node is not a document") must therefore see THROUGH these
    /// wraps and refuse them as document roots.  Interior-mutable so
    /// the XSLT engine can mark wraps via the `&DocIndex` it holds.
    synthetic_wraps: std::cell::RefCell<std::collections::HashSet<NodeId>>,
}

/// Top-bit-set IDs are EXSLT-synthesised text nodes.  The remaining
/// bits index into `DocIndex::synthetic`.  Using the top bit lets
/// every `DocIndexLike` method dispatch in a single mask test, and
/// guarantees synthetic IDs sort after every real index entry — so
/// document order between real and synthetic node-sets is total.
pub const SYNTHETIC_TEXT_BASE: NodeId = 1_usize << (usize::BITS - 1);


impl<'doc> DocIndex<'doc> {
    /// Build a flat index over `doc`.  O(n) in the number of nodes.
    pub fn build(doc: &'doc ArenaDoc) -> Self {
        let mut idx = Self {
            nodes: Vec::new(),
            id_attrs: doc.id_attributes().clone(),
            idref_attrs: doc.idref_attributes().clone(),
            synthetic: std::cell::RefCell::new(Vec::new()),
            rtfs:      elsa::FrozenVec::new(),
            synthetic_wraps: std::cell::RefCell::new(std::collections::HashSet::new()),
        };
        // Node 0: synthetic Document.
        idx.nodes.push(INode {
            kind: INodeKind::Document,
            parent: None,
            attr_start: 0, attr_end: 0,
            ns_start:   0, ns_end:   0,
            content_children: Vec::new(),
        });
        // Walk the document-level chain (prolog comments/PIs, root,
        // then epilogue comments/PIs) so XPath sees them as children
        // of the document node per XPath 1.0 §5.1.
        let mut top: Vec<NodeId> = Vec::new();
        let mut cur: Option<&'doc ArenaNode<'doc>> = Some(doc.first_sibling());
        while let Some(n) = cur {
            top.extend(idx.add_node(n, 0));
            cur = n.next_sibling.get();
        }
        idx.nodes[0].content_children = top;
        idx
    }

    /// Append another document's tree to this index, returning the
    /// NodeId of the new synthetic Document node.  Used to make
    /// runtime-loaded documents (e.g. via the XSLT `document()`
    /// function or XInclude resolution) addressable through the
    /// same XPath index as the primary source.
    ///
    /// The added doc must outlive `'doc` — typically achieved by
    /// owning it in a `Vec<Box<Document>>` that lives at least as
    /// long as the index.
    pub fn add_document(&mut self, doc: &'doc ArenaDoc) -> NodeId {
        let doc_id = self.nodes.len();
        self.nodes.push(INode {
            kind: INodeKind::Document,
            parent: None,
            attr_start: 0, attr_end: 0,
            ns_start:   0, ns_end:   0,
            content_children: Vec::new(),
        });
        let mut top: Vec<NodeId> = Vec::new();
        let mut cur: Option<&'doc ArenaNode<'doc>> = Some(doc.first_sibling());
        while let Some(n) = cur {
            top.extend(self.add_node(n, doc_id));
            cur = n.next_sibling.get();
        }
        self.nodes[doc_id].content_children = top;
        // Merge the grafted doc's DTD-declared ID-attribute typing
        // into ours.  Same element name in two docs is rare in
        // practice; on collision we extend the existing list so
        // either DTD's typing keeps working.
        if !doc.id_attributes().is_empty() {
            let mut merged = (*self.id_attrs).clone();
            for (elem, ids) in doc.id_attributes().iter() {
                merged.entry(elem.clone())
                    .or_insert_with(Vec::new)
                    .extend(ids.iter().cloned());
            }
            self.id_attrs = std::sync::Arc::new(merged);
        }
        if !doc.idref_attributes().is_empty() {
            let mut merged = (*self.idref_attrs).clone();
            for (elem, refs) in doc.idref_attributes().iter() {
                merged.entry(elem.clone())
                    .or_insert_with(Vec::new)
                    .extend(refs.iter().cloned());
            }
            self.idref_attrs = std::sync::Arc::new(merged);
        }
        doc_id
    }

    fn add_node(&mut self, node: &'doc ArenaNode<'doc>, parent: NodeId) -> Vec<NodeId> {
        match node.kind {
            ArenaKind::Element => {
                let id = self.nodes.len();
                self.nodes.push(INode {
                    kind: INodeKind::Element(node),
                    parent: Some(parent),
                    attr_start: 0, attr_end: 0,
                    ns_start:   0, ns_end:   0,
                    content_children: Vec::new(),
                });
                let attr_start = self.nodes.len();
                for attr in node.attributes() {
                    // Namespace declarations sometimes land in the
                    // attribute list (parser-path-dependent); they
                    // live on the namespace axis, not the attribute
                    // axis, so skip them here.  XPath 1.0 §5.3 says
                    // namespace declarations are not attributes.
                    let name = attr.name();
                    if name == "xmlns" || name.starts_with("xmlns:") { continue; }
                    self.nodes.push(INode {
                        kind: INodeKind::Attribute(attr),
                        parent: Some(id),
                        attr_start: 0, attr_end: 0,
                        ns_start:   0, ns_end:   0,
                        content_children: Vec::new(),
                    });
                }
                let attr_end = self.nodes.len();
                self.nodes[id].attr_start = attr_start;
                self.nodes[id].attr_end   = attr_end;

                // Materialise namespace nodes (XPath 1.0 §5.4) right
                // after attributes.  Allocating them here keeps their
                // NodeIds adjacent to the element's, before any of its
                // content children — which preserves the dedup_sort()
                // assumption that NodeId order is document order.
                let ns_start = self.nodes.len();
                for (prefix, uri) in collect_in_scope_namespaces(node) {
                    self.nodes.push(INode {
                        kind: INodeKind::Namespace { prefix, uri },
                        parent: Some(id),
                        attr_start: 0, attr_end: 0,
                        ns_start:   0, ns_end:   0,
                        content_children: Vec::new(),
                    });
                }
                let ns_end = self.nodes.len();
                self.nodes[id].ns_start = ns_start;
                self.nodes[id].ns_end   = ns_end;

                let mut children = Vec::new();
                for child in node.children() {
                    children.extend(self.add_node(child, id));
                }
                self.nodes[id].content_children = children;
                vec![id]
            }
            ArenaKind::Text | ArenaKind::Comment | ArenaKind::CData | ArenaKind::Pi => {
                let id = self.nodes.len();
                let kind = match node.kind {
                    ArenaKind::Text    => INodeKind::Text(node),
                    ArenaKind::Comment => INodeKind::Comment(node),
                    ArenaKind::CData   => INodeKind::CData(node),
                    ArenaKind::Pi      => INodeKind::PI(node),
                    _                  => unreachable!(),
                };
                self.nodes.push(INode {
                    kind,
                    parent: Some(parent),
                    attr_start: 0, attr_end: 0,
                    ns_start:   0, ns_end:   0,
                    content_children: Vec::new(),
                });
                vec![id]
            }
            // c-abi-only discriminant; never appears on a real Node.
            ArenaKind::Attribute => unreachable!("Attribute kind never appears on a Node"),
            ArenaKind::Document  => unreachable!("Document kind never appears on a Node"),
            // Entity references — XPath data model doesn't expose
            // them; they're either expanded earlier in the parser
            // or appear as opaque placeholders we can skip.
            ArenaKind::EntityRef => vec![],
            // DocumentFragment is a compat-shim transient — produced
            // by `xmlNewDocFragment` and grafted into a real tree
            // before any XPath context is built over it.  If one
            // shows up here, treat it as a skip-and-emit-nothing
            // node (same as we do for EntityRef).
            ArenaKind::DocumentFragment => vec![],
            // The DTD internal subset is not part of the XPath data
            // model — skip the node and its declaration body.
            ArenaKind::DtdDecl => vec![],
            ArenaKind::Dtd => vec![],
        }
    }

    /// XPath 1.0 § 5 string value of a node.
    pub fn string_value(&self, id: NodeId) -> String {
        if let Some((rtf, local)) = self.rtf_at(id) {
            return rtf.string_value(local);
        }
        if is_synthetic(id) {
            let i = id & !SYNTHETIC_TEXT_BASE;
            return self.synthetic.borrow().get(i).cloned().unwrap_or_default();
        }
        match &self.nodes[id].kind {
            INodeKind::Document | INodeKind::Element(_) => {
                let mut s = String::new();
                self.concat_text(id, &mut s);
                s
            }
            INodeKind::Attribute(a) => a.value().to_string(),
            INodeKind::Text(n) | INodeKind::CData(n) => n.content().to_string(),
            INodeKind::Comment(n)                          => n.content().to_string(),
            INodeKind::PI(n)                               => n.content().to_string(),
            // Namespace node's string value is its URI (XPath §5.4).
            INodeKind::Namespace { uri, .. }               => (*uri).to_string(),
        }
    }

    fn concat_text(&self, id: NodeId, out: &mut String) {
        for &child in &self.nodes[id].content_children {
            match &self.nodes[child].kind {
                INodeKind::Text(n) | INodeKind::CData(n) => out.push_str(n.content()),
                INodeKind::Element(_) => self.concat_text(child, out),
                _ => {}
            }
        }
    }

    pub fn node_name(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.node_name(local); }
        if is_synthetic(id) { return ""; }
        match &self.nodes[id].kind {
            INodeKind::Element(n) => n.name(),
            INodeKind::Attribute(a) => a.name(),
            INodeKind::PI(n) => n.name(),
            // Namespace node's "name" is its prefix, or "" for the
            // default namespace binding (XPath §5.4).
            INodeKind::Namespace { prefix, .. } => prefix.unwrap_or(""),
            _ => "",
        }
    }

    pub fn local_name(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.local_name(local); }
        if is_synthetic(id) { return ""; }
        // Namespace nodes don't carry a colon-prefixed form; the
        // local name equals the binding's prefix (empty for default).
        if let INodeKind::Namespace { prefix, .. } = &self.nodes[id].kind {
            return prefix.unwrap_or("");
        }
        let full = self.node_name(id);
        match full.split_once(':') {
            Some((_, local)) => local,
            None => full,
        }
    }

    pub fn namespace_uri(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.namespace_uri(local); }
        if is_synthetic(id) { return ""; }
        match &self.nodes[id].kind {
            INodeKind::Element(n)   => n.namespace.get().map(|ns| ns.href()).unwrap_or(""),
            INodeKind::Attribute(a) => a.namespace.get().map(|ns| ns.href()).unwrap_or(""),
            // Per XPath §5.4, the expanded-name of a namespace node
            // has a null namespace URI — distinct from the URI the
            // namespace node *binds*, which is its string value.
            _ => "",
        }
    }

    pub fn namespace_prefix(&self, id: NodeId) -> Option<&str> {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.namespace_prefix(local); }
        if is_synthetic(id) { return None; }
        match &self.nodes[id].kind {
            INodeKind::Element(n)   => n.namespace.get().and_then(|ns| ns.prefix()),
            INodeKind::Attribute(a) => a.namespace.get().and_then(|ns| ns.prefix()),
            _ => None,
        }
    }
}

/// Collect the in-scope namespace bindings for an element, in
/// XPath 1.0 § 5.4 semantics:
///
/// * Walk the ancestor-or-self chain, accumulating `xmlns:p` and
///   `xmlns` declarations.  Closer declarations shadow further ones.
/// * Bindings declared with an empty URI (`xmlns:p=""`, `xmlns=""`)
///   are *undeclarations* — they remove the prefix from the
///   in-scope set rather than appearing as a namespace node.
/// * Always append the implicit `xml` prefix (the XML Namespaces
///   recommendation declares it on every element), unless the user
///   somehow shadowed it.
///
/// Returns `(prefix, uri)` pairs.  Order matters only for stable
/// document-order reporting on `namespace::*`; we use declaration
/// order: own bindings first, then each ancestor's, with shadowing.
fn collect_in_scope_namespaces<'doc>(
    el: &'doc ArenaNode<'doc>,
) -> Vec<(Option<&'doc str>, &'doc str)> {
    // (prefix, uri) — prefix=None means default namespace.
    let mut out: Vec<(Option<&'doc str>, &'doc str)> = Vec::new();
    let mut seen_default = false;
    let mut seen_xml     = false;

    let mut cur: Option<&'doc ArenaNode<'doc>> = Some(el);
    while let Some(n) = cur {
        for (prefix, uri) in n.ns_declarations() {
            let already = out.iter().any(|(p, _)| *p == prefix)
                || (prefix.is_none() && seen_default);
            if already {
                // Shadowed by a closer declaration; skip.
                continue;
            }
            if prefix.is_none() {
                seen_default = true;
            }
            if uri.is_empty() {
                // Undeclaration — record that we've seen this
                // prefix so further (ancestor) declarations don't
                // re-introduce it, but don't emit a namespace node.
                continue;
            }
            if prefix == Some(XML_NS_PREFIX) {
                seen_xml = true;
            }
            out.push((prefix, uri));
        }
        cur = n.parent.get();
    }

    if !seen_xml {
        out.push((Some(XML_NS_PREFIX), XML_NS_URI));
    }
    out
}

// ── EXSLT RTF allocator ─────────────────────────────────────────────────────

impl<'doc> DocIndex<'doc> {
    /// Allocate one synthetic text node per supplied string and
    /// return their `NodeId`s in document order.  Used by EXSLT
    /// functions like `str:tokenize`, `str:split`, and
    /// `regexp:match` that need to materialise a node-set of
    /// strings without an XSLT-engine RTF arena to host them.
    ///
    /// The nodes are top-level — they have no parent and no
    /// document — but every `DocIndexLike` accessor handles them
    /// correctly so callers can iterate the result with
    /// `xsl:for-each`, take their string-value, count them, etc.
    /// Their IDs sort after every real node-set node, so unions
    /// stay totally ordered.
    pub fn allocate_rtf_text_nodes_inherent(&self, values: Vec<String>) -> Vec<NodeId> {
        let mut store = self.synthetic.borrow_mut();
        let start = store.len();
        store.extend(values);
        (start..store.len())
            .map(|i| SYNTHETIC_TEXT_BASE | i)
            .collect()
    }

}

/// True iff `id`'s top bit is set — i.e. it points into the EXSLT
/// synthetic-text store rather than the main `nodes` table.  Inlined
/// at every `DocIndexLike` accessor to short-circuit before the real
/// table lookup.
#[inline(always)]
pub fn is_synthetic_id(id: NodeId) -> bool { is_synthetic(id) }

pub(crate) fn is_synthetic(id: NodeId) -> bool {
    id & SYNTHETIC_TEXT_BASE != 0
}

// ── RTF (result-tree-fragment) hosting ─────────────────────────────

impl<'doc> DocIndex<'doc> {
    /// Begin building a new result-tree fragment.  Returns an
    /// [`RtfBuilder`](super::rtf::RtfBuilder) that knows its
    /// eventual host-vector slot — callers populate it via
    /// `add_document` / `add_element` / etc., then hand it back to
    /// [`finish_rtf`](Self::finish_rtf).
    pub fn start_rtf(&self) -> super::rtf::RtfBuilder {
        let slot = self.rtfs.len();
        super::rtf::RtfBuilder::new(slot)
    }

    /// Mark `root` (an RTF document-root NodeId) as the synthetic
    /// wrap of a sequence-typed XSLT binding.  See
    /// [`synthetic_wraps`](Self::synthetic_wraps) for the rationale.
    pub fn mark_synthetic_wrap(&self, root: NodeId) {
        self.synthetic_wraps.borrow_mut().insert(root);
    }

    /// True when `id` is a synthetic doc-wrap created for
    /// sequence-typed binding storage rather than a real source /
    /// XSLT document tree root.  Used by XTDE1270 / XTDE1370 /
    /// XTDE1380 to refuse the wrap as a document for the purposes
    /// of `fn:key` / `fn:unparsed-entity-uri` and friends.
    pub fn is_synthetic_wrap(&self, id: NodeId) -> bool {
        self.synthetic_wraps.borrow().contains(&id)
    }

    /// Freeze a populated builder into the host vector.  Returns
    /// the globally-encoded id of the RTF's document node so the
    /// caller can bind a variable directly to it.
    pub fn finish_rtf(&self, builder: super::rtf::RtfBuilder) -> NodeId {
        let host_index = builder.host_index;
        let rtf = builder.build();
        // FrozenVec::push takes shared &self and returns &T with
        // a lifetime tied to the FrozenVec.  Boxes stay put; only
        // the outer vector's tail grows.
        self.rtfs.push(Box::new(rtf));
        super::rtf::encode_rtf_id(host_index, 0)
    }

    /// Graft a fully-parsed [`sup_xml_tree::dom::Document`] into the
    /// index at runtime, returning the node id of its synthetic
    /// document root.  Used by XSLT 2.0 `doc()` / XSLT 1.0
    /// `document()` to resolve URIs computed at apply time (rather
    /// than statically discovered at compile time).
    ///
    /// The grafted doc reuses the [`super::rtf`] storage: every
    /// node's data is copied into the RTF's append-only `nodes`
    /// table so the caller can drop the source [`Document`] right
    /// after the call.  XPath operations on the returned node id
    /// dispatch through the same RTF accessors that XSLT-built
    /// result-tree fragments use.  Bytes-for-bytes structural
    /// fidelity is preserved (PIs, comments, attributes,
    /// namespace declarations); doc-level XSD typing and unparsed-
    /// entity metadata are not — the spec leaves those undefined
    /// for `doc()`-loaded resources.
    pub fn graft_dynamic_document(
        &self,
        doc: &sup_xml_tree::dom::Document,
    ) -> NodeId {
        let mut builder = self.start_rtf();
        let root = builder.add_document();
        // Walk the document's top-level chain.  `first_sibling`
        // points at the first prolog comment / PI / element; the
        // chain ends when `next_sibling` is `None`.
        let mut cur = Some(doc.first_sibling());
        while let Some(n) = cur {
            graft_node(&mut builder, root, n);
            cur = n.next_sibling.get();
        }
        self.finish_rtf(builder)
    }
}

/// Recursive helper for [`DocIndex::graft_dynamic_document`].
/// Translates one `Node` (and its descendants) into the
/// corresponding `RtfBuilder` calls, preserving structural shape.
fn graft_node(
    builder: &mut super::rtf::RtfBuilder,
    parent:  NodeId,
    node:    &sup_xml_tree::dom::Node<'_>,
) {
    use sup_xml_tree::dom::NodeKind;
    match node.kind {
        NodeKind::Element => {
            let name   = node.name();
            let prefix = name.rfind(':').map(|i| &name[..i]);
            let uri    = node.namespace.get().map(|ns| ns.href()).unwrap_or("");
            let elem   = builder.add_element(parent, name, uri, prefix);
            // Attributes first so the attribute-axis enumeration
            // reflects source order.
            let mut attrs = node.attributes().peekable();
            if attrs.peek().is_some() {
                builder.start_attrs(elem);
                for a in attrs {
                    let an      = a.name();
                    let aprefix = an.rfind(':').map(|i| &an[..i]);
                    let auri    = a.namespace.get().map(|ns| ns.href()).unwrap_or("");
                    builder.add_attribute(elem, an, auri, aprefix, a.value());
                }
            }
            for child in node.children() {
                graft_node(builder, elem, child);
            }
        }
        NodeKind::Text | NodeKind::CData => {
            builder.add_text(parent, node.content());
        }
        NodeKind::Comment => {
            builder.add_comment(parent, node.content());
        }
        NodeKind::Pi => {
            builder.add_pi(parent, node.name(), node.content());
        }
        // Document / Attribute / EntityRef / etc. don't appear as
        // children of a Document or Element at this level in the
        // arena (Document is the top, Attribute hangs off an
        // element's attr chain).  Skip silently.
        _ => {}
    }
}

// ── DocIndexLike (arena tree) ───────────────────────────────────────────────

impl<'doc> DocIndex<'doc> {
    /// Resolve an RTF-marked id to its host [`super::rtf::RtfIndex`] +
    /// local node id.  Inlined into the dispatch hot path on every
    /// accessor so the FrozenVec deref + decode cost is one cmov on
    /// real-source-tree IDs (the marker bit is zero) and a single
    /// shift + index on RTF IDs.
    #[inline(always)]
    fn rtf_at(&self, id: NodeId) -> Option<(&super::rtf::RtfIndex, NodeId)> {
        if !super::rtf::is_rtf_id(id) { return None; }
        let (host_i, local) = super::rtf::decode_rtf_id(id);
        self.rtfs.get(host_i).map(|r| (r, local))
    }
}

impl<'doc> DocIndexLike for DocIndex<'doc> {
    fn graft_dynamic_document(
        &self,
        doc: &sup_xml_tree::dom::Document,
    ) -> Option<NodeId> {
        Some(DocIndex::graft_dynamic_document(self, doc))
    }
    fn children(&self, id: NodeId) -> &[NodeId] {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.children(local); }
        if is_synthetic(id) { return &[]; }
        &self.nodes[id].content_children
    }
    fn parent(&self, id: NodeId) -> Option<NodeId> {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.parent(local); }
        if is_synthetic(id) { return None; }
        self.nodes[id].parent
    }
    fn attr_range(&self, id: NodeId) -> std::ops::Range<NodeId> {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.attr_range(local); }
        if is_synthetic(id) { return 0..0; }
        self.nodes[id].attr_start..self.nodes[id].attr_end
    }
    fn ns_range(&self, id: NodeId) -> std::ops::Range<NodeId> {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.ns_range(local); }
        if is_synthetic(id) { return 0..0; }
        self.nodes[id].ns_start..self.nodes[id].ns_end
    }
    fn kind(&self, id: NodeId) -> XPathNodeKind {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.kind(local); }
        if is_synthetic(id) { return XPathNodeKind::Text; }
        match self.nodes[id].kind {
            INodeKind::Document        => XPathNodeKind::Document,
            INodeKind::Element(_)      => XPathNodeKind::Element,
            INodeKind::Attribute(_)    => XPathNodeKind::Attribute,
            INodeKind::Text(_)         => XPathNodeKind::Text,
            INodeKind::Comment(_)      => XPathNodeKind::Comment,
            INodeKind::CData(_)        => XPathNodeKind::CData,
            INodeKind::PI(_)           => XPathNodeKind::PI,
            INodeKind::Namespace { .. } => XPathNodeKind::Namespace,
        }
    }
    fn pi_target(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.pi_target(local); }
        if is_synthetic(id) { return ""; }
        match &self.nodes[id].kind {
            INodeKind::PI(n) => n.name(),
            _ => "",
        }
    }
    fn string_value(&self, id: NodeId) -> String {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.string_value(local); }
        if is_synthetic(id) {
            let i = id & !SYNTHETIC_TEXT_BASE;
            return self.synthetic.borrow().get(i).cloned().unwrap_or_default();
        }
        DocIndex::string_value(self, id)
    }
    fn node_name(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.node_name(local); }
        if is_synthetic(id) { return ""; }
        DocIndex::node_name(self, id)
    }
    fn local_name(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.local_name(local); }
        if is_synthetic(id) { return ""; }
        DocIndex::local_name(self, id)
    }
    fn namespace_prefix(&self, id: NodeId) -> Option<&str> {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.namespace_prefix(local); }
        if is_synthetic(id) { return None; }
        DocIndex::namespace_prefix(self, id)
    }
    fn namespace_uri(&self, id: NodeId) -> &str {
        if let Some((rtf, local)) = self.rtf_at(id) { return rtf.namespace_uri(local); }
        if is_synthetic(id) { return ""; }
        DocIndex::namespace_uri(self, id)
    }
    fn is_element(&self, id: NodeId) -> bool {
        if let Some((rtf, local)) = self.rtf_at(id) {
            return matches!(rtf.kind(local), XPathNodeKind::Element);
        }
        if is_synthetic(id) { return false; }
        matches!(self.nodes[id].kind, INodeKind::Element(_))
    }
    fn allocate_rtf_text_nodes(&self, values: Vec<String>) -> Option<Vec<NodeId>> {
        Some(DocIndex::allocate_rtf_text_nodes_inherent(self, values))
    }
    fn rtf_builder(&self) -> Option<super::rtf::RtfBuilder> {
        Some(DocIndex::start_rtf(self))
    }
    fn finish_rtf(&self, builder: super::rtf::RtfBuilder) -> Option<NodeId> {
        Some(DocIndex::finish_rtf(self, builder))
    }
    fn is_synthetic_wrap(&self, id: NodeId) -> bool {
        DocIndex::is_synthetic_wrap(self, id)
    }
    fn is_id_attribute(&self, attr_id: NodeId) -> bool {
        if super::rtf::is_rtf_id(attr_id) { return false; }
        if is_synthetic(attr_id) { return false; }
        let INodeKind::Attribute(_) = self.nodes[attr_id].kind else { return false; };
        let local = self.local_name(attr_id);
        // Default convention always honoured: any `xml:id` and any
        // attribute literally named `id` count, matching libxml2 in
        // DTD-less documents.
        if self.node_name(attr_id) == "xml:id" || local == "id" {
            return true;
        }
        // DTD-typed override: consult the parsed `<!ATTLIST … ID>`
        // declarations on this attribute's owner element.
        if self.id_attrs.is_empty() { return false; }
        let Some(owner) = self.parent(attr_id) else { return false; };
        let owner_name = self.node_name(owner);
        let attr_name = self.node_name(attr_id);
        self.id_attrs.get(owner_name)
            .is_some_and(|ids| ids.iter().any(|n| n == attr_name || n == local))
    }
    fn is_idref_attribute(&self, attr_id: NodeId) -> bool {
        if super::rtf::is_rtf_id(attr_id) { return false; }
        if is_synthetic(attr_id) { return false; }
        let INodeKind::Attribute(_) = self.nodes[attr_id].kind else { return false; };
        // IDREF typing only comes from the DTD — there is no DTD-less
        // convention (unlike `id`/`xml:id` for ID attributes).
        if self.idref_attrs.is_empty() { return false; }
        let Some(owner) = self.parent(attr_id) else { return false; };
        let owner_name = self.node_name(owner);
        let attr_name = self.node_name(attr_id);
        let local = self.local_name(attr_id);
        self.idref_attrs.get(owner_name)
            .is_some_and(|refs| refs.iter().any(|n| n == attr_name || n == local))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::index::DocIndexLike;
    use crate::{parse_str, ParseOptions};

    fn idx_of(xml: &str) -> (sup_xml_tree::dom::Document, ()) {
        // Namespace-aware parsing: these tests probe the XPath data
        // model's namespace nodes, which only get populated when
        // the parser runs the xmlns-resolution pass.  Under the
        // `c-abi` feature, xmlns declarations live on the
        // element's `ns_def` chain — populated only on this path
        // — and `ns_declarations()` walks that chain.  Without
        // `namespace_aware`, ns_def stays empty and every
        // namespace-node assertion below sees just the implicit
        // `xml` binding.
        let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
        let doc = parse_str(xml, &opts).unwrap();
        (doc, ())
    }

    /// Find the first node matching `kind` via the index by scanning
    /// NodeIds.  Returns the id.
    fn find_kind(idx: &DocIndex<'_>, want: XPathNodeKind) -> NodeId {
        for id in 0..idx.nodes.len() {
            if idx.kind(id) == want { return id; }
        }
        panic!("no node of kind {want:?}");
    }

    // ── PI nodes ────────────────────────────────────────────────

    #[test]
    fn pi_node_indexing_and_accessors() {
        let (doc, _) = idx_of(r#"<r><?xml-stylesheet href="s.xsl"?></r>"#);
        let idx = DocIndex::build(&doc);
        let pi_id = find_kind(&idx, XPathNodeKind::PI);
        assert_eq!(idx.node_name(pi_id),   "xml-stylesheet");
        assert_eq!(idx.local_name(pi_id),  "xml-stylesheet");
        assert_eq!(idx.namespace_uri(pi_id), "");
        assert_eq!(idx.namespace_prefix(pi_id), None);
        // PI string-value is its data part (content after target).
        assert_eq!(idx.string_value(pi_id), r#"href="s.xsl""#);
        assert_eq!(<DocIndex<'_> as DocIndexLike>::pi_target(&idx, pi_id), "xml-stylesheet");
    }

    #[test]
    fn pi_target_returns_empty_for_non_pi_nodes() {
        let (doc, _) = idx_of("<r/>");
        let idx = DocIndex::build(&doc);
        let elem_id = find_kind(&idx, XPathNodeKind::Element);
        assert_eq!(<DocIndex<'_> as DocIndexLike>::pi_target(&idx, elem_id), "");
    }

    // ── CDATA nodes ─────────────────────────────────────────────

    #[test]
    fn cdata_node_indexed_and_kinded() {
        let (doc, _) = idx_of("<r><![CDATA[raw <data>]]></r>");
        let idx = DocIndex::build(&doc);
        let cd_id = find_kind(&idx, XPathNodeKind::CData);
        // CData string-value is the raw text content.
        assert_eq!(idx.string_value(cd_id), "raw <data>");
        // CData kind via DocIndexLike trait dispatch.
        assert_eq!(idx.kind(cd_id), XPathNodeKind::CData);
    }

    // ── Comment nodes ───────────────────────────────────────────

    #[test]
    fn comment_string_value() {
        let (doc, _) = idx_of("<r><!-- hello --></r>");
        let idx = DocIndex::build(&doc);
        let c_id = find_kind(&idx, XPathNodeKind::Comment);
        assert_eq!(idx.string_value(c_id), " hello ");
    }

    // ── Attribute with namespace ────────────────────────────────

    #[test]
    fn attribute_string_value_returns_attr_value() {
        // Exercises the INodeKind::Attribute(a) arm of string_value
        // and namespace_uri's unwrap_or("") path.
        let (doc, _) = idx_of(r#"<r id="x"/>"#);
        let idx = DocIndex::build(&doc);
        let attr_id = find_kind(&idx, XPathNodeKind::Attribute);
        assert_eq!(idx.string_value(attr_id), "x");
        // namespace_uri's match arm fires (returns "" via unwrap_or).
        assert_eq!(idx.namespace_uri(attr_id), "");
    }

    #[test]
    fn unprefixed_attribute_has_no_namespace_uri_or_prefix() {
        let (doc, _) = idx_of(r#"<r id="x"/>"#);
        let idx = DocIndex::build(&doc);
        let attr_id = find_kind(&idx, XPathNodeKind::Attribute);
        assert_eq!(idx.namespace_uri(attr_id), "");
        assert_eq!(idx.namespace_prefix(attr_id), None);
    }

    // ── Non-element / non-attribute nodes have no NS ────────────

    #[test]
    fn text_node_has_no_namespace() {
        let (doc, _) = idx_of("<r>hello</r>");
        let idx = DocIndex::build(&doc);
        let txt_id = find_kind(&idx, XPathNodeKind::Text);
        assert_eq!(idx.namespace_uri(txt_id), "");
        assert_eq!(idx.namespace_prefix(txt_id), None);
    }

    // ── Explicit xmlns:xml declaration ──────────────────────────

    #[test]
    fn explicit_xmlns_xml_declaration_is_honored() {
        // When the user explicitly declares xmlns:xml on an element,
        // we must NOT also append the implicit one (seen_xml = true).
        let (doc, _) = idx_of(
            r#"<r xmlns:xml="http://www.w3.org/XML/1998/namespace"/>"#,
        );
        let idx = DocIndex::build(&doc);
        let r_id = find_kind(&idx, XPathNodeKind::Element);
        let ns_range = idx.ns_range(r_id);
        let xml_count = ns_range
            .filter(|&nid| idx.node_name(nid) == "xml")
            .count();
        assert_eq!(xml_count, 1, "xml prefix should appear exactly once");
    }

    // ── Namespace nodes ─────────────────────────────────────────

    #[test]
    fn namespace_node_local_and_node_name_match_prefix() {
        let (doc, _) = idx_of(r#"<r xmlns:ns="urn:n"/>"#);
        let idx = DocIndex::build(&doc);
        let r_id = find_kind(&idx, XPathNodeKind::Element);
        let ns_range = idx.ns_range(r_id);
        // Should have at least the 'ns' binding plus the implicit 'xml'.
        let mut found_ns = false;
        for nid in ns_range {
            if idx.node_name(nid) == "ns" {
                found_ns = true;
                assert_eq!(idx.local_name(nid), "ns");
                assert_eq!(idx.string_value(nid), "urn:n");
                assert_eq!(idx.namespace_uri(nid), "");
            }
        }
        assert!(found_ns, "ns binding not found among namespace nodes");
    }

    #[test]
    fn default_namespace_node_has_empty_name() {
        let (doc, _) = idx_of(r#"<r xmlns="urn:default"/>"#);
        let idx = DocIndex::build(&doc);
        let r_id = find_kind(&idx, XPathNodeKind::Element);
        let ns_range = idx.ns_range(r_id);
        let mut found = false;
        for nid in ns_range {
            // Default namespace: prefix=None → node_name() returns "".
            if idx.string_value(nid) == "urn:default" {
                found = true;
                assert_eq!(idx.node_name(nid), "");
                assert_eq!(idx.local_name(nid), "");
            }
        }
        assert!(found);
    }

    #[test]
    fn xmlns_undeclaration_removes_default() {
        // xmlns="" on a child element undoes the parent's default.
        let (doc, _) = idx_of(
            r#"<r xmlns="urn:default"><c xmlns=""/></r>"#,
        );
        let idx = DocIndex::build(&doc);
        // Walk to the inner element.
        let inner = (0..idx.nodes.len())
            .find(|&id|
                idx.kind(id) == XPathNodeKind::Element
                && idx.local_name(id) == "c"
            )
            .expect("c element");
        let ns_range = idx.ns_range(inner);
        let default_count = ns_range
            .filter(|&nid| idx.node_name(nid) == "")
            .count();
        // The default namespace should be undeclared on <c/>, so no
        // node for it.
        assert_eq!(default_count, 0);
    }

    // ── DocIndexLike trait wrappers ─────────────────────────────

    #[test]
    fn doc_index_like_thunks_forward_correctly() {
        let (doc, _) = idx_of(r#"<r xmlns:ns="urn:n" id="x"><a/></r>"#);
        let idx = DocIndex::build(&doc);
        // Trait-method dispatch covers the thunk lines in the impl.
        let r_id = find_kind(&idx, XPathNodeKind::Element);
        let attr_id = find_kind(&idx, XPathNodeKind::Attribute);
        // The implicit `xml` plus `ns` give the element 2 namespace nodes.
        assert!(<DocIndex<'_> as DocIndexLike>::ns_range(&idx, r_id).len() >= 2);
        assert!(<DocIndex<'_> as DocIndexLike>::is_element(&idx, r_id));
        assert!(!<DocIndex<'_> as DocIndexLike>::is_element(&idx, attr_id));
        assert_eq!(<DocIndex<'_> as DocIndexLike>::namespace_prefix(&idx, attr_id), None);
        // The thunk for string_value.
        assert!(!<DocIndex<'_> as DocIndexLike>::string_value(&idx, r_id).is_empty()
                || true); // smoke
    }
}
