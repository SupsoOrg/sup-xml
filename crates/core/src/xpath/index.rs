//! Tree-agnostic abstraction over [`super::context::DocIndex`] (legacy tree)
//! and [`super::context::DocIndex`] (arena tree).
//!
//! XPath evaluation in [`super::eval`] is generic over this trait so we have
//! a single evaluator that works against both tree representations.  The
//! public entry points ([`super::XPathContext`] and
//! [`super::XPathContext`]) remain separate parallel types — only the
//! evaluator internals are shared.

use std::ops::Range;

/// Index into a flat per-document node table.  Zero is the synthetic Document
/// node; every other index points to the corresponding entry in the per-tree
/// implementation's `nodes` vector.
pub type NodeId = usize;

/// Simple, tree-agnostic enum.  Used by eval for node-test matching.  The
/// per-tree `INodeKind` / `INodeKind` types map onto this via
/// [`DocIndexLike::kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XPathNodeKind {
    Document,
    Element,
    Attribute,
    Text,
    Comment,
    CData,
    PI,
    /// Synthetic namespace node — one per in-scope namespace
    /// binding for an element, materialized during indexing.
    /// XPath 1.0 §5.4.
    Namespace,
}

/// Methods the XPath evaluator needs from any backing tree representation.
///
/// Both [`super::context::DocIndex`] and [`super::context::DocIndex`]
/// implement this trait so eval can run against either.
pub trait DocIndexLike {
    /// Graft a parsed [`sup_xml_tree::dom::Document`] into the index
    /// at runtime, returning the node id of its synthetic document
    /// root.  Concrete `DocIndex` implementations use the same
    /// append-only RTF storage that XSLT result-tree fragments use,
    /// so the call needs only `&self`; stub implementations (test
    /// shims) return `None` to fall back to the legacy
    /// "URI not pre-loaded" error path in `document_fn`.
    fn graft_dynamic_document(
        &self,
        _doc: &sup_xml_tree::dom::Document,
    ) -> Option<NodeId> {
        None
    }

    /// Children in document order (content children only — attributes are
    /// addressed separately via [`attr_range`](Self::attr_range)).
    fn children(&self, id: NodeId) -> &[NodeId];

    /// Parent node id, or `None` for the synthetic Document node (id 0).
    fn parent(&self, id: NodeId) -> Option<NodeId>;

    /// Attribute-node id range for an element (empty range for non-elements).
    fn attr_range(&self, id: NodeId) -> Range<NodeId>;

    /// Namespace-node id range for an element — the synthetic
    /// namespace nodes that XPath's `namespace::` axis traverses.
    /// One per distinct in-scope prefix (own + inherited, with
    /// closer declarations shadowing) plus the implicit `xml`
    /// prefix.  Empty for non-elements.  Default impl returns an
    /// empty range; the arena index materialises these eagerly
    /// during build.
    fn ns_range(&self, _id: NodeId) -> Range<NodeId> { 0..0 }

    /// Tree-agnostic node-kind discriminant.
    fn kind(&self, id: NodeId) -> XPathNodeKind;

    /// PI target — only meaningful for [`XPathNodeKind::PI`]; returns `""`
    /// for other kinds.
    fn pi_target(&self, id: NodeId) -> &str;

    /// XPath 1.0 § 5 string value of the node.
    fn string_value(&self, id: NodeId) -> String;

    /// Qualified name (`prefix:local`) — element / attribute / PI; `""` otherwise.
    fn node_name(&self, id: NodeId) -> &str;

    /// Local part of the QName (everything after the colon, or the whole name).
    fn local_name(&self, id: NodeId) -> &str;

    /// Namespace URI bound to the element/attribute's prefix; `""` otherwise.
    fn namespace_uri(&self, id: NodeId) -> &str;

    /// Namespace prefix declared on this element/attribute (e.g. `"dc"`),
    /// or `None` when the node has no namespace or is the default
    /// namespace.  Default impl returns `None`; types that carry
    /// namespace info (the arena index) override.
    fn namespace_prefix(&self, _id: NodeId) -> Option<&str> { None }

    /// Convenience: `kind(id) == Element`.
    fn is_element(&self, id: NodeId) -> bool {
        matches!(self.kind(id), XPathNodeKind::Element)
    }

    /// True iff `attr_id` is a DTD-declared ID-type attribute or
    /// matches the `xml:id` convention.  Default returns `true` for
    /// any attribute whose local name is `id` (libxml2's de facto
    /// behaviour); indexes built from documents with DTDs override
    /// to consult the parsed `<!ATTLIST … ID>` declarations.
    fn is_id_attribute(&self, attr_id: NodeId) -> bool {
        if !matches!(self.kind(attr_id), XPathNodeKind::Attribute) { return false; }
        let n = self.node_name(attr_id);
        n == "xml:id" || self.local_name(attr_id) == "id"
    }

    /// True iff `attr_id` is a DTD-declared `IDREF`/`IDREFS`-type
    /// attribute.  Unlike ID there is no DTD-less convention, so the
    /// default returns `false`; indexes built from documents with DTDs
    /// override to consult the parsed `<!ATTLIST … IDREF>` declarations.
    /// Consulted by XPath 2.0 §14.5.5's `idref()`.
    fn is_idref_attribute(&self, _attr_id: NodeId) -> bool { false }

    /// Allocate one synthetic text node per supplied string and
    /// return their `NodeId`s.  Used by EXSLT functions
    /// (`str:tokenize`, `str:split`, `regexp:match`) that need to
    /// return a node-set of computed strings without an XSLT-engine
    /// RTF arena to host them.  Default returns `None` — the index
    /// implementation doesn't support runtime allocation.  Indexes
    /// that do (the arena `DocIndex`) override to return the new
    /// IDs in document order.
    fn allocate_rtf_text_nodes(&self, _values: Vec<String>) -> Option<Vec<NodeId>> { None }

    /// Start an append-only RTF subtree the caller can build into via
    /// the returned [`RtfBuilder`].  The builder's
    /// `add_document` / `add_element` / `add_text` / `add_attribute`
    /// API populates a fresh tree; callers hand it back to
    /// [`finish_rtf`](Self::finish_rtf) to publish the resulting
    /// `RtfIndex` into the arena.  Default returns `None` — indexes
    /// without arena storage (test shims, foreign-doc wrappers) can
    /// signal "no RTF construction support" here.  The concrete arena
    /// `DocIndex` overrides to return a real builder.
    fn rtf_builder(&self) -> Option<super::rtf::RtfBuilder> { None }

    /// Publish a populated [`RtfBuilder`] into the arena, returning
    /// the global id of the synthetic document root.  Mirrors
    /// [`rtf_builder`](Self::rtf_builder); default returns `None`.
    fn finish_rtf(&self, _builder: super::rtf::RtfBuilder) -> Option<NodeId> { None }

    /// Schema-aware: governing type `(ns, local)` of a constructed RTF
    /// node built with a `type=` / `xsl:type=` annotation, if any.
    /// Default `None` — indexes without RTF type tracking report every
    /// constructed node as untyped.
    fn rtf_node_type(&self, _id: NodeId) -> Option<(String, String)> { None }

    /// True when `id` is a synthetic RTF doc-wrap that holds the
    /// items of a sequence-typed XSLT binding rather than a real
    /// XML / XSLT document tree.  Used by XSLT's XTDE1270 / XTDE1370
    /// / XTDE1380 to refuse the wrap as a document root when the
    /// spec asks for the "root of the tree containing the context
    /// node".  Default `false` — index shims without sequence-typed
    /// binding support never produce such wraps.
    fn is_synthetic_wrap(&self, _id: NodeId) -> bool { false }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `DocIndexLike` that overrides only the required methods,
    /// so the trait's default impls (`ns_range`, `namespace_prefix`,
    /// `is_element`) are the ones under test.
    ///
    /// Layout: id 0 is the synthetic Document, id 1 is an Element child,
    /// id 2 is a Text child of the element.
    struct StubIndex;

    impl DocIndexLike for StubIndex {
        fn children(&self, id: NodeId) -> &[NodeId] {
            match id {
                0 => &[1],
                1 => &[2],
                _ => &[],
            }
        }
        fn parent(&self, id: NodeId) -> Option<NodeId> {
            match id {
                0 => None,
                1 => Some(0),
                2 => Some(1),
                _ => None,
            }
        }
        fn attr_range(&self, _id: NodeId) -> Range<NodeId> { 0..0 }
        fn kind(&self, id: NodeId) -> XPathNodeKind {
            match id {
                0 => XPathNodeKind::Document,
                1 => XPathNodeKind::Element,
                2 => XPathNodeKind::Text,
                _ => XPathNodeKind::Text,
            }
        }
        fn pi_target(&self, _id: NodeId) -> &str { "" }
        fn string_value(&self, _id: NodeId) -> String { String::new() }
        fn node_name(&self, _id: NodeId) -> &str { "" }
        fn local_name(&self, _id: NodeId) -> &str { "" }
        fn namespace_uri(&self, _id: NodeId) -> &str { "" }
        // ns_range, namespace_prefix, is_element intentionally NOT
        // overridden — the trait defaults are under test.
    }

    #[test]
    fn default_ns_range_is_empty() {
        let idx = StubIndex;
        // The Element node (id 1) has no namespace info in the stub, so
        // the default should return an empty range regardless of id.
        for id in 0..=2 {
            let r = idx.ns_range(id);
            assert_eq!(r, 0..0, "ns_range({id}) should default to empty");
            assert!(r.is_empty());
        }
    }

    #[test]
    fn default_namespace_prefix_is_none() {
        let idx = StubIndex;
        for id in 0..=2 {
            assert_eq!(idx.namespace_prefix(id), None);
        }
    }

    #[test]
    fn default_is_element_matches_kind() {
        let idx = StubIndex;
        assert!(!idx.is_element(0));         // Document
        assert!( idx.is_element(1));         // Element
        assert!(!idx.is_element(2));         // Text
    }

    #[test]
    fn stub_required_methods_smoke() {
        // The required methods on StubIndex are needed for the impl to
        // exist, but the defaults are what's under test in this module.
        // Touch each required method once so coverage isn't pulled down
        // by the test-only stub.
        let idx = StubIndex;
        assert_eq!(idx.children(0), &[1]);
        assert_eq!(idx.children(1), &[2]);
        assert!(idx.children(2).is_empty());
        assert_eq!(idx.parent(0), None);
        assert_eq!(idx.parent(1), Some(0));
        assert_eq!(idx.parent(2), Some(1));
        assert_eq!(idx.parent(99), None);
        assert!(idx.attr_range(1).is_empty());
        assert_eq!(idx.kind(0), XPathNodeKind::Document);
        assert_eq!(idx.kind(1), XPathNodeKind::Element);
        assert_eq!(idx.kind(2), XPathNodeKind::Text);
        assert_eq!(idx.kind(99), XPathNodeKind::Text);
        assert_eq!(idx.pi_target(0), "");
        assert_eq!(idx.string_value(0), "");
        assert_eq!(idx.node_name(0), "");
        assert_eq!(idx.local_name(0), "");
        assert_eq!(idx.namespace_uri(0), "");
    }

    #[test]
    fn xpath_node_kind_traits() {
        // Exercise the derived impls on XPathNodeKind so they aren't
        // listed as uncovered functions.
        let k = XPathNodeKind::Element;
        let copy = k;                                  // Copy
        let clone = k.clone();                         // Clone
        assert_eq!(k, copy);                           // PartialEq / Eq
        assert_eq!(k, clone);
        assert_ne!(k, XPathNodeKind::Text);
        // Debug
        let s = format!("{:?}", k);
        assert!(s.contains("Element"));
        // All variants distinct.
        let variants = [
            XPathNodeKind::Document,
            XPathNodeKind::Element,
            XPathNodeKind::Attribute,
            XPathNodeKind::Text,
            XPathNodeKind::Comment,
            XPathNodeKind::CData,
            XPathNodeKind::PI,
            XPathNodeKind::Namespace,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                assert_eq!(i == j, a == b);
            }
        }
    }
}
