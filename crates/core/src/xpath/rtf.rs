//! XSLT result-tree-fragment storage for XPath navigation.
//!
//! When XSLT 2.0 binds a body-form `xsl:variable`, the spec models
//! the variable's value as a temporary document tree.  XPath
//! expressions like `$rtf/foo` need to navigate into that tree just
//! like a source document — children, attributes, predicates, the
//! works.
//!
//! Our [`super::context::DocIndex`] is built once from the source
//! and held as `&DocIndex` everywhere afterward, so RTFs can't be
//! grafted into its primary node table mid-evaluation.  Instead
//! each RTF is built complete-then-frozen as an [`RtfIndex`]
//! (owned-string copies of element / attribute / text content) and
//! pushed into a [`elsa::FrozenVec`] hosted by `DocIndex`.  The
//! FrozenVec lets us append via shared `&self` while every
//! `&RtfIndex` we hand out keeps its heap address stable for the
//! lifetime of the surrounding evaluation — so the
//! [`DocIndexLike`](super::index::DocIndexLike) accessors on
//! `DocIndex` can route to the right RTF and return `&[NodeId]`
//! slices directly from its internal table.
//!
//! ## NodeId encoding
//!
//! RTF node-ids carry a marker bit so the dispatch is a single
//! mask test on every accessor.  The layout (on 64-bit `usize`):
//!
//! ```text
//!  63        62      32             0
//!   ┌────────┬───────┬───────────────┐
//!   │ synth  │  RTF  │   rtf-index   │   ← bits 32..62 = which RTF
//!   ├────────┴───────┴───────────────┤
//!   │           local node id        │   ← bits 0..32   = node within
//!   └────────────────────────────────┘
//! ```
//!
//! `synth` is the existing EXSLT synthetic-text marker (bit 63);
//! `RTF` is bit 62.  The two are mutually exclusive: an id is
//! either synthetic-text, an RTF node, or a real source-tree node.

use std::ops::Range;

use super::index::XPathNodeKind;
use super::NodeId;

/// Bit 62 — set on every [`NodeId`] that addresses a node in an
/// [`RtfIndex`].  Mutually exclusive with the synthetic-text
/// marker (bit 63) so a single mask test on each accessor
/// dispatches the three storage regions.
pub const RTF_BASE: NodeId = 1_usize << (usize::BITS - 2);

/// Number of low bits reserved for the per-RTF local node id.
/// 32 bits is enough for 4-billion nodes inside a single RTF —
/// orders of magnitude more than any real XSLT temporary tree.
const RTF_LOCAL_BITS:  u32   = 32;
const RTF_LOCAL_MASK:  NodeId = (1_usize << RTF_LOCAL_BITS) - 1;
/// Bits 32..62 carry the RTF's index in the host
/// [`super::context::DocIndex::rtfs`] vector — 30 bits, room for
/// a billion RTFs per transformation.
const RTF_INDEX_MASK:  NodeId = !(RTF_BASE | super::context::SYNTHETIC_TEXT_BASE | RTF_LOCAL_MASK);

/// True iff `id` addresses an [`RtfIndex`] node (bit 62 set).
#[inline(always)]
pub fn is_rtf_id(id: NodeId) -> bool {
    id & RTF_BASE != 0 && id & super::context::SYNTHETIC_TEXT_BASE == 0
}

/// Decompose an RTF node-id into `(host-vector index, local id)`.
#[inline(always)]
pub fn decode_rtf_id(id: NodeId) -> (usize, NodeId) {
    let local = id & RTF_LOCAL_MASK;
    let rtf_i = (id & RTF_INDEX_MASK) >> RTF_LOCAL_BITS;
    (rtf_i, local)
}

/// Compose an RTF node-id from its host-vector index and the
/// per-RTF local id.  Panics in debug mode when either component
/// exceeds its bit budget.
#[inline(always)]
pub fn encode_rtf_id(rtf_i: usize, local: NodeId) -> NodeId {
    debug_assert!(local <= RTF_LOCAL_MASK,
        "RTF local id {local} exceeds 32-bit budget");
    debug_assert!(rtf_i <= (RTF_INDEX_MASK >> RTF_LOCAL_BITS),
        "RTF index {rtf_i} exceeds 30-bit budget");
    RTF_BASE | (rtf_i << RTF_LOCAL_BITS) | local
}

// ── per-RTF node table ─────────────────────────────────────────────

/// One node inside an [`RtfIndex`].  Strings are owned (boxed) so
/// the index can stand alone after the source [`crate::xpath::eval`]
/// result-tree fragment has been built — XSLT bind_variable
/// constructs the index then drops the intermediate.
///
/// All NodeId fields (`parent`, `children`, attribute /
/// namespace ranges) are stored **already encoded with the RTF
/// marker bits** (via [`encode_rtf_id`]), so accessor methods
/// can hand out `&[NodeId]` slices directly.  This costs the
/// builder one bit-shift per id at construction time and saves
/// the evaluator a per-access Vec allocation.
#[derive(Debug)]
pub struct RtfNode {
    pub kind:     RtfNodeKind,
    pub parent:   Option<NodeId>,
    pub children: Vec<NodeId>,
    pub attr_start: NodeId,
    pub attr_end:   NodeId,
    pub ns_start:   NodeId,
    pub ns_end:     NodeId,
}

#[derive(Debug)]
pub enum RtfNodeKind {
    Document,
    Element {
        name: Box<str>,
        local_name: Box<str>,
        prefix: Option<Box<str>>,
        namespace_uri: Box<str>,
    },
    Attribute {
        name: Box<str>,
        local_name: Box<str>,
        prefix: Option<Box<str>>,
        namespace_uri: Box<str>,
        value: Box<str>,
    },
    Text(Box<str>),
    Comment(Box<str>),
    PI { target: Box<str>, data: Box<str> },
    Namespace { prefix: Option<Box<str>>, uri: Box<str> },
}

/// One result-tree fragment indexed for XPath navigation.  Built
/// complete-then-frozen — once a `RtfIndex` is pushed into the
/// host [`elsa::FrozenVec`], its `nodes` table never mutates.
#[derive(Debug)]
pub struct RtfIndex {
    /// LOCAL ids — node 0 is the synthetic Document node, the
    /// rest are its descendants in document order.  Callers
    /// outside this module always see the IDs encoded with the
    /// `RTF_BASE` marker (via [`encode_rtf_id`]); the local Vec
    /// is the implementation detail.
    pub nodes: Vec<RtfNode>,
    /// Self-index inside the host `DocIndex::rtfs` vector,
    /// captured at registration time so accessors can re-encode
    /// child / parent ids back to the marker form callers expect.
    pub host_index: usize,
}

impl RtfIndex {
    /// Children of `local_id` — already-encoded global ids so the
    /// outer dispatcher can return the slice directly.
    pub fn children(&self, local_id: NodeId) -> &[NodeId] {
        &self.nodes[local_id].children
    }

    /// Parent of `local_id`, globally encoded.  Returns `None` for
    /// the document root (LOCAL id 0).
    pub fn parent(&self, local_id: NodeId) -> Option<NodeId> {
        self.nodes[local_id].parent
    }

    pub fn attr_range(&self, local_id: NodeId) -> Range<NodeId> {
        let n = &self.nodes[local_id];
        n.attr_start..n.attr_end
    }

    pub fn ns_range(&self, local_id: NodeId) -> Range<NodeId> {
        let n = &self.nodes[local_id];
        n.ns_start..n.ns_end
    }

    pub fn kind(&self, local_id: NodeId) -> XPathNodeKind {
        match self.nodes[local_id].kind {
            RtfNodeKind::Document      => XPathNodeKind::Document,
            RtfNodeKind::Element { .. } => XPathNodeKind::Element,
            RtfNodeKind::Attribute { .. } => XPathNodeKind::Attribute,
            RtfNodeKind::Text(_)       => XPathNodeKind::Text,
            RtfNodeKind::Comment(_)    => XPathNodeKind::Comment,
            RtfNodeKind::PI { .. }     => XPathNodeKind::PI,
            RtfNodeKind::Namespace { .. } => XPathNodeKind::Namespace,
        }
    }

    pub fn pi_target(&self, local_id: NodeId) -> &str {
        match &self.nodes[local_id].kind {
            RtfNodeKind::PI { target, .. } => target,
            _ => "",
        }
    }

    pub fn node_name(&self, local_id: NodeId) -> &str {
        match &self.nodes[local_id].kind {
            RtfNodeKind::Element { name, .. }
                | RtfNodeKind::Attribute { name, .. } => name,
            RtfNodeKind::PI { target, .. } => target,
            // XPath 2.0 §2.5.4 — a namespace node's "node name" is
            // its prefix (or the empty string for the default
            // namespace binding).
            RtfNodeKind::Namespace { prefix, .. } =>
                prefix.as_deref().unwrap_or(""),
            _ => "",
        }
    }

    pub fn local_name(&self, local_id: NodeId) -> &str {
        match &self.nodes[local_id].kind {
            RtfNodeKind::Element { local_name, .. }
                | RtfNodeKind::Attribute { local_name, .. } => local_name,
            RtfNodeKind::PI { target, .. } => target,
            RtfNodeKind::Namespace { prefix, .. } =>
                prefix.as_deref().unwrap_or(""),
            _ => "",
        }
    }

    pub fn namespace_uri(&self, local_id: NodeId) -> &str {
        match &self.nodes[local_id].kind {
            RtfNodeKind::Element { namespace_uri, .. }
                | RtfNodeKind::Attribute { namespace_uri, .. } => namespace_uri,
            _ => "",
        }
    }

    pub fn namespace_prefix(&self, local_id: NodeId) -> Option<&str> {
        match &self.nodes[local_id].kind {
            RtfNodeKind::Element { prefix, .. }
                | RtfNodeKind::Attribute { prefix, .. } => prefix.as_deref(),
            _ => None,
        }
    }

    /// Recursive descendant text concatenation — XPath 1.0 §5
    /// string-value semantics for element / document nodes.
    /// Atomic kinds return their stored content directly.
    pub fn string_value(&self, local_id: NodeId) -> String {
        match &self.nodes[local_id].kind {
            RtfNodeKind::Text(s)    => s.to_string(),
            RtfNodeKind::Comment(s) => s.to_string(),
            RtfNodeKind::PI { data, .. } => data.to_string(),
            RtfNodeKind::Attribute { value, .. } => value.to_string(),
            RtfNodeKind::Namespace { uri, .. } => uri.to_string(),
            RtfNodeKind::Element { .. } | RtfNodeKind::Document => {
                let mut out = String::new();
                self.append_text(local_id, &mut out);
                out
            }
        }
    }

    fn append_text(&self, local_id: NodeId, out: &mut String) {
        // Children are stored already-encoded (global ids).
        // string_value's recursion stays within one RTF, so we
        // strip the marker bits back to the local index for
        // each child.
        for &child in &self.nodes[local_id].children {
            let child_local = child & RTF_LOCAL_MASK;
            match &self.nodes[child_local].kind {
                RtfNodeKind::Text(s) => out.push_str(s),
                RtfNodeKind::Element { .. } | RtfNodeKind::Document =>
                    self.append_text(child_local, out),
                _ => {}
            }
        }
    }
}


// ── builder ────────────────────────────────────────────────────────

/// Helper for populating an [`RtfIndex`] one node at a time.  The
/// builder owns the slot index it'll be pushed into so it can
/// encode child / parent / attribute ids directly to global form
/// during construction; no second pass needed.
///
/// Typical usage from the XSLT engine:
///
/// ```ignore
/// let mut b = idx.start_rtf();
/// let root = b.add_document();
/// let elem = b.add_element(root, "foo", "", None, &[]);
/// b.add_text(elem, "hello");
/// let root_id = idx.finish_rtf(b);   // returns encoded root NodeId
/// ```
pub struct RtfBuilder {
    /// Host-vector slot this RTF will occupy.  Captured up front
    /// so child / parent / attr ids encode to global form during
    /// construction.
    pub(crate) host_index: usize,
    pub(crate) nodes:      Vec<RtfNode>,
    /// Schema-aware: `(encoded NodeId, (type-ns, type-local))` for each
    /// constructed node carrying a `type=` / `xsl:type=` annotation.
    /// Collected during construction (the builder already encodes ids
    /// to global form) and drained into the host index's PSVI table by
    /// [`DocIndex::finish_rtf`](super::context::DocIndex).
    pub typed_nodes: Vec<(NodeId, Box<(String, String)>)>,
}

impl RtfBuilder {
    #[allow(dead_code)] // wired through DocIndex::start_rtf in the next step
    pub(crate) fn new(host_index: usize) -> Self {
        Self { host_index, nodes: Vec::new(), typed_nodes: Vec::new() }
    }

    #[inline]
    fn glob(&self, local: NodeId) -> NodeId {
        encode_rtf_id(self.host_index, local)
    }

    fn push(&mut self, kind: RtfNodeKind, parent: Option<NodeId>) -> NodeId {
        let local_id = self.nodes.len();
        self.nodes.push(RtfNode {
            kind,
            parent,
            children:  Vec::new(),
            attr_start: 0, attr_end: 0,
            ns_start:   0, ns_end:   0,
        });
        self.glob(local_id)
    }

    fn local_of(global: NodeId) -> NodeId {
        global & RTF_LOCAL_MASK
    }

    /// Add the synthetic Document node.  Must be the first call
    /// — the index conventionally treats local id 0 as the
    /// document root.  Returns the encoded global id.
    pub fn add_document(&mut self) -> NodeId {
        assert!(self.nodes.is_empty(),
            "RtfBuilder::add_document must be the first call");
        self.push(RtfNodeKind::Document, None)
    }

    /// Add an element child of `parent`.  `parent` is a global
    /// id (the value previously returned by `add_document` or
    /// another `add_element`).
    pub fn add_element(
        &mut self, parent: NodeId,
        qname: &str, namespace_uri: &str, prefix: Option<&str>,
    ) -> NodeId {
        let (local_name, _) = qname.rsplit_once(':')
            .map(|(_, l)| (l, true))
            .unwrap_or((qname, false));
        let id = self.push(RtfNodeKind::Element {
            name:          qname.into(),
            local_name:    local_name.into(),
            prefix:        prefix.map(Into::into),
            namespace_uri: namespace_uri.into(),
        }, Some(parent));
        let parent_local = Self::local_of(parent);
        self.nodes[parent_local].children.push(id);
        id
    }

    pub fn add_text(&mut self, parent: NodeId, content: &str) -> NodeId {
        let id = self.push(RtfNodeKind::Text(content.into()), Some(parent));
        let parent_local = Self::local_of(parent);
        self.nodes[parent_local].children.push(id);
        id
    }

    pub fn add_comment(&mut self, parent: NodeId, content: &str) -> NodeId {
        let id = self.push(RtfNodeKind::Comment(content.into()), Some(parent));
        let parent_local = Self::local_of(parent);
        self.nodes[parent_local].children.push(id);
        id
    }

    pub fn add_pi(&mut self, parent: NodeId, target: &str, data: &str) -> NodeId {
        let id = self.push(RtfNodeKind::PI {
            target: target.into(), data: data.into(),
        }, Some(parent));
        let parent_local = Self::local_of(parent);
        self.nodes[parent_local].children.push(id);
        id
    }

    /// Set the contiguous attribute-id range for an element.
    /// Attributes are appended via `add_attribute` AFTER the
    /// element and BEFORE any further content children — caller
    /// is responsible for ordering and for calling `finish_attrs`
    /// to lock in the range.
    pub fn start_attrs(&mut self, elem_global: NodeId) {
        let elem_local = Self::local_of(elem_global);
        let start_local = self.nodes.len();
        self.nodes[elem_local].attr_start = self.glob(start_local);
        self.nodes[elem_local].attr_end   = self.glob(start_local);
    }

    pub fn add_attribute(
        &mut self, elem_global: NodeId,
        qname: &str, namespace_uri: &str, prefix: Option<&str>,
        value: &str,
    ) -> NodeId {
        let (local_name, _) = qname.rsplit_once(':')
            .map(|(_, l)| (l, true))
            .unwrap_or((qname, false));
        let id = self.push(RtfNodeKind::Attribute {
            name:          qname.into(),
            local_name:    local_name.into(),
            prefix:        prefix.map(Into::into),
            namespace_uri: namespace_uri.into(),
            value:         value.into(),
        }, Some(elem_global));
        let elem_local = Self::local_of(elem_global);
        // Extend the half-open range to cover this new attr.
        let new_end_local = Self::local_of(id) + 1;
        self.nodes[elem_local].attr_end = self.glob(new_end_local);
        id
    }

    /// Open the namespace-node range for `elem_global`.  Subsequent
    /// [`add_namespace_node`] calls populate the contiguous slab;
    /// the range covers them automatically.  Callers must invoke
    /// this *after* the attribute slab (`start_attrs` /
    /// `add_attribute`) and *before* the first child, so attribute
    /// and namespace nodes don't interleave with element children.
    pub fn start_ns(&mut self, elem_global: NodeId) {
        let elem_local = Self::local_of(elem_global);
        let start_local = self.nodes.len();
        self.nodes[elem_local].ns_start = self.glob(start_local);
        self.nodes[elem_local].ns_end   = self.glob(start_local);
    }

    /// Add an in-scope namespace node to `elem_global`.  `prefix` is
    /// `None` for the default-namespace binding and `Some("")` is
    /// treated identically; otherwise pass the prefix without colons.
    /// Returns the globally-encoded id of the new namespace node so
    /// callers can index it directly (rarely needed — the typical
    /// access path is `ns_range(elem)`).
    pub fn add_namespace_node(
        &mut self, elem_global: NodeId,
        prefix: Option<&str>, uri: &str,
    ) -> NodeId {
        let prefix = prefix.filter(|p| !p.is_empty());
        let id = self.push(
            RtfNodeKind::Namespace {
                prefix: prefix.map(Into::into),
                uri:    uri.into(),
            },
            Some(elem_global),
        );
        let elem_local = Self::local_of(elem_global);
        let new_end_local = Self::local_of(id) + 1;
        self.nodes[elem_local].ns_end = self.glob(new_end_local);
        id
    }

    /// Consume the builder into the populated [`RtfIndex`].
    pub fn build(self) -> RtfIndex {
        RtfIndex { nodes: self.nodes, host_index: self.host_index }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtf_id_round_trips() {
        let id = encode_rtf_id(7, 42);
        assert!(is_rtf_id(id));
        assert_eq!(decode_rtf_id(id), (7, 42));
    }

    #[test]
    fn rtf_marker_is_disjoint_from_synthetic() {
        let s = super::super::context::SYNTHETIC_TEXT_BASE;
        let r = encode_rtf_id(0, 0);
        assert!(!is_rtf_id(s),
            "synthetic ids must not look like RTF ids");
        assert!(r & s == 0,
            "RTF and synthetic markers must be distinct bits");
    }
}
