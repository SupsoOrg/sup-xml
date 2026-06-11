//! Lightweight intermediate result-tree representation.
//!
//! The XSLT evaluator builds a [`ResultTree`] in memory; the
//! output serialisers in [`crate::output`] consume it and emit
//! XML / HTML / text bytes per the effective `<xsl:output>` settings.
//!
//! We deliberately don't reuse `sup_xml_tree::dom` here — that's
//! tuned for libxml2 ABI shape and arena allocation, and XSLT
//! result trees are typically short-lived and ad-hoc.  A
//! straightforward `Vec`-backed tree is easier to build
//! incrementally (the eval engine pushes nodes into a "current
//! container" stack) and easier to serialise.

use crate::ast::QName;

/// One node in an in-progress result tree.  Element children
/// recurse via the `children` vec; attributes hang off the
/// element separately because XPath / serialisation treat them
/// differently from content children.
#[derive(Clone, Debug)]
pub enum ResultNode {
    Element {
        name:       QName,
        /// (prefix, URI) namespace declarations to emit on this
        /// element.  May be empty — by default we inherit from the
        /// parent and only emit declarations that introduce
        /// changes.
        namespaces: Vec<(Option<String>, String)>,
        attributes: Vec<(QName, String)>,
        children:   Vec<ResultNode>,
        /// Schema-aware: the expanded name `(ns, local)` of the type
        /// this element was constructed as — from a `type=` / `xsl:type=`
        /// attribute or a `validation=` mode.  `None` for the ordinary
        /// untyped case.  Carried through RTF indexing so a constructed
        /// node's typed value is recoverable by `data()` /
        /// `instance of` (XSLT 2.0 §5.7.1, annotation-for-constructed-
        /// element).
        schema_type: Option<Box<(String, String)>>,
    },
    Text {
        content: String,
        /// `disable-output-escaping` — when `true`, the serialiser
        /// emits the content verbatim (no `&amp;` / `&lt;`).  Used
        /// by `<xsl:text disable-output-escaping="yes"/>` and
        /// `<xsl:value-of disable-output-escaping="yes"/>`.
        dose:    bool,
    },
    Comment(String),
    ProcessingInstruction { target: String, data: String },
    /// A parentless attribute node — the value of an `xsl:attribute`
    /// evaluated with no element under construction (XSLT 2.0 §5.7.1
    /// sequence constructors, e.g. an `as="attribute()*"` variable).
    /// `xsl:copy-of` / `xsl:apply-templates` consume it; it never
    /// appears among an element's children (element attributes live in
    /// the `attributes` vec).
    Attribute { name: QName, value: String },
}

/// The complete result of a transformation.  Carries the
/// serialised top-level children plus the effective
/// `<xsl:output>` settings the serialiser will honour.
#[derive(Clone, Debug, Default)]
pub struct ResultTree {
    pub children: Vec<ResultNode>,
    pub output:   crate::ast::OutputSpec,
    /// Flattened `xsl:character-map` substitutions selected by
    /// `xsl:output use-character-maps="…"` (XSLT 2.0 §20).  Empty
    /// for stylesheets that don't use character maps; populated at
    /// apply time after composing every referenced map.  The
    /// serializer consults this list per emitted character.
    pub character_map: Vec<(char, String)>,
    /// Secondary result documents written by `xsl:result-document
    /// href="…"` (XSLT 2.0 §19.1): `(resolved-href, document)`.  Empty
    /// unless the stylesheet produced secondary output.
    pub secondary: Vec<(String, ResultTree)>,
}

/// Builder helper — maintains the "currently being built" element
/// stack so the evaluator can emit nodes one at a time without
/// constructing the tree bottom-up.
#[derive(Debug, Default)]
pub struct ResultBuilder {
    /// Stack of in-progress elements.  Each frame is the
    /// element under construction; closing it pops the frame and
    /// pushes the completed element as a child of the next frame
    /// (or into the top-level if the stack is now empty).
    stack:    Vec<ResultNode>,
    /// Top-level result nodes accumulated so far.
    pub top:  Vec<ResultNode>,
    /// XSLT 2.0 §5.7.2 sequence normalisation merges adjacent
    /// text fragments when building the result tree of a template
    /// or a no-`as` RTF — that's the default behaviour.  Variable
    /// bodies declared `as="item()*"` (or any sequence type) skip
    /// normalisation per §9.3: each `xsl:text` contributes its
    /// own text-node item, three of them stay three items, and
    /// `count($var)` answers `3`.  Setting this flag disables the
    /// merge.
    pub no_text_merge: bool,
    /// First sequence-construction error observed (XSLT 2.0 §5.7.1 —
    /// e.g. XTDE0410, an attribute or namespace node landing in an
    /// element's content after a non-attribute/non-namespace node).
    /// Stashed here because the affected builder methods are infallible
    /// at their call sites; the surrounding `apply` consults this
    /// after the construction unwinds and surfaces it as a real error.
    pub deferred_error: Option<String>,
    /// True only for the builder that constructs the *principal* result
    /// document.  Sub-builders that materialise variable bodies / RTFs
    /// / temp trees stay `false`, because XSLT 2.0 §5.7.1 permits a
    /// parentless attribute or namespace node in a sequence-constructor
    /// context (e.g. `as="attribute()*"`).  Only on the principal
    /// builder does an attribute / namespace at the top level violate
    /// XTDE0420 (a document node's content sequence is forbidden from
    /// containing one).
    pub is_principal_document: bool,
    /// XSLT 2.0 §5.7.2 sequence-normalisation step 2: adjacent atomic
    /// values in a sequence constructor get a single space inserted
    /// between them.  `last_was_atomic` records whether the most
    /// recent emission was an atomic-derived text; it resets on
    /// any non-atomic emit (literal text, element, comment, …) and
    /// at element open/close (each element body is its own
    /// sequence constructor).
    pub last_was_atomic: bool,
}

impl ResultBuilder {
    pub fn new() -> Self { Self::default() }

    /// Open a new element.  Until the matching [`close_element`]
    /// call, every emitted child/attribute attaches to this one.
    ///
    /// XSLT 1.0 namespace fixup: when the new element is in no
    /// namespace but the inherited default namespace is non-empty,
    /// record an `xmlns=""` undeclaration so the serialiser doesn't
    /// silently fold the element back into the inherited default.
    pub fn open_element(&mut self, name: QName) {
        let needs_default_undecl = name.uri.is_empty()
            && name.prefix.is_none()
            && self.inherited_default_namespace()
                .is_some_and(|d| !d.is_empty());
        self.stack.push(ResultNode::Element {
            name,
            namespaces: if needs_default_undecl {
                vec![(None, String::new())]
            } else {
                Vec::new()
            },
            attributes: Vec::new(),
            children:   Vec::new(),
            schema_type: None,
        });
        // A new element body opens a fresh sequence-constructor scope
        // for atomic-separator purposes.
        self.last_was_atomic = false;
    }

    /// Record the schema type `(ns, local)` of the element currently
    /// under construction (the top of the build stack) — set from an
    /// `xsl:type=` / `type=` attribute (XSLT 2.0 §5.7.1).  No-op when
    /// no element is open.
    pub fn set_current_element_type(&mut self, ty: (String, String)) {
        if let Some(ResultNode::Element { schema_type, .. }) = self.stack.last_mut() {
            *schema_type = Some(Box::new(ty));
        }
    }

    /// The default namespace currently in scope, walking outwards
    /// from the innermost open element.  `None` when no default
    /// declaration has been emitted on any ancestor.
    fn inherited_default_namespace(&self) -> Option<&str> {
        for n in self.stack.iter().rev() {
            if let ResultNode::Element { namespaces, .. } = n {
                if let Some((_, u)) = namespaces.iter().find(|(p, _)| p.is_none()) {
                    return Some(u);
                }
            }
        }
        None
    }

    /// Close the current element and attach it to its parent (or
    /// to the top-level if there's no parent).  Panics if no
    /// element is open — the evaluator must balance these calls.
    pub fn close_element(&mut self) {
        let done = self.stack.pop().expect("close_element with empty stack");
        self.push_node(done);
        // The closed element itself counts as a non-atomic emission
        // in the parent's sequence constructor.
        self.last_was_atomic = false;
    }

    /// Append an attribute to the current element.  Per XSLT 1.0
    /// §7.1.3, emitting `xsl:attribute` after content children is
    /// legal — we just ignore the spec's "should be before children"
    /// note and accept any order.  If an attribute of the same name
    /// already exists, the later one wins (also per spec).
    ///
    /// XSLT 1.0 namespace fixup for attributes: an attribute with a
    /// non-empty namespace URI but no prefix can't be serialised as
    /// XML (the default namespace doesn't apply to attributes — XML
    /// Names §6.2).  Synthesize a fresh prefix (`ns0`, `ns1`, …) and
    /// declare it on the owning element so the serialiser produces
    /// well-formed output.
    pub fn push_attribute(&mut self, mut name: QName, value: String) {
        // Parentless attribute: no element is under construction (the
        // attribute is being produced directly into a sequence /
        // variable body).  Emit it as a standalone node rather than
        // dropping it — copy-of / apply-templates will consume it.
        if !matches!(self.stack.last(), Some(ResultNode::Element { .. })) {
            // XSLT 2.0 §5.7.1 / XTDE0420 — on the *principal* result
            // document, the content sequence may not contain attribute
            // (or namespace) nodes.  Sub-builders for variable bodies
            // legitimately collect parentless attributes.
            if self.is_principal_document && self.deferred_error.is_none() {
                self.deferred_error = Some(format!(
                    "result document content contains attribute '{}' \
                     (XTDE0420)", name.local));
            }
            self.top.push(ResultNode::Attribute { name, value });
            return;
        }
        // XSLT 2.0 §5.7.1 / XTDE0410 — once non-attribute / non-
        // namespace content has been emitted into an element, no
        // further attributes may be added to it.
        if matches!(self.stack.last(),
            Some(ResultNode::Element { children, .. }) if !children.is_empty())
            && self.deferred_error.is_none()
        {
            self.deferred_error = Some(format!(
                "result sequence places attribute '{}' after element \
                 content (XTDE0410)", name.local));
        }
        if name.prefix.is_none() && !name.uri.is_empty() {
            let chosen = self.synthesize_prefix_for(&name.uri);
            self.push_namespace_decl(Some(chosen.clone()), name.uri.clone());
            name.prefix = Some(chosen);
        } else if let Some(prefix) = &name.prefix {
            // XML Names §6.2: a prefixed attribute's prefix MUST be
            // in scope on its owning element.  When the attribute is
            // being copied onto an element constructed by
            // `xsl:element` / `xsl:copy` that doesn't itself declare
            // the prefix (or worse, shadows it with a different
            // URI), emit the missing `xmlns:prefix` here.
            if !name.uri.is_empty() && prefix != "xml" {
                let in_scope: Option<String> = self.stack.iter().rev().find_map(|n| match n {
                    ResultNode::Element { namespaces, .. } => namespaces
                        .iter()
                        .find(|(p, _)| p.as_deref() == Some(prefix.as_str()))
                        .map(|(_, u)| u.clone()),
                    _ => None,
                });
                match in_scope.as_deref() {
                    Some(u) if u == name.uri => {} // already bound, nothing to do
                    None => {
                        // Prefix unused — bring it into scope.
                        self.push_namespace_decl(Some(prefix.clone()), name.uri.clone());
                    }
                    Some(_) => {
                        // XSLT 2.0 §5.7.3 namespace-fixup: the requested
                        // prefix is bound to a *different* URI here, so
                        // we can't re-bind it without breaking the
                        // colliding declaration.  Mint a `prefix_N` that
                        // isn't taken on this element and use it instead.
                        let fresh = self.synthesize_prefix_like(prefix, &name.uri);
                        self.push_namespace_decl(Some(fresh.clone()), name.uri.clone());
                        name.prefix = Some(fresh);
                    }
                }
            }
        }
        let Some(ResultNode::Element { attributes, .. }) = self.stack.last_mut() else {
            // Attribute outside any element — XSLT says this is an
            // error, but we tolerate it by silently dropping.  The
            // engine catches structural cases earlier.
            return;
        };
        // Replace existing attribute with same expanded name.
        if let Some(slot) = attributes.iter_mut()
            .find(|(n, _)| n.uri == name.uri && n.local == name.local)
        {
            slot.1 = value;
        } else {
            attributes.push((name, value));
        }
    }

    /// Find a prefix that already maps to `uri` anywhere on the
    /// open-element stack, or coin a fresh `nsN` that doesn't
    /// collide with any in-scope prefix.
    fn synthesize_prefix_for(&self, uri: &str) -> String {
        // Reuse an existing binding when one is in scope.
        for n in self.stack.iter().rev() {
            if let ResultNode::Element { namespaces, .. } = n {
                for (p, u) in namespaces {
                    if let Some(p) = p {
                        if u == uri { return p.clone(); }
                    }
                }
            }
        }
        // Coin a fresh prefix that isn't already declared in scope.
        let used: std::collections::HashSet<String> = self.stack.iter()
            .filter_map(|n| match n {
                ResultNode::Element { namespaces, .. } => Some(namespaces),
                _ => None,
            })
            .flat_map(|ns| ns.iter().filter_map(|(p, _)| p.clone()))
            .collect();
        (0..)
            .map(|i| format!("ns{i}"))
            .find(|p| !used.contains(p))
            .expect("infinite iterator finds an unused prefix")
    }

    /// Like [`synthesize_prefix_for`] but derives the candidate from a
    /// user-supplied `hint` — `prefix_1`, `prefix_2`, … — so the new
    /// binding stays visually related to the prefix the stylesheet
    /// asked for.  Used by namespace-fixup when an `xsl:attribute`
    /// requests a prefix already bound to a different URI: we can't
    /// re-bind without conflict, so we mint a fresh `hint_N`.
    fn synthesize_prefix_like(&self, hint: &str, uri: &str) -> String {
        // Reuse an existing binding when one is in scope (same logic
        // as synthesize_prefix_for — a prefix already mapping to this
        // URI is the cheapest valid choice).
        for n in self.stack.iter().rev() {
            if let ResultNode::Element { namespaces, .. } = n {
                for (p, u) in namespaces {
                    if let Some(p) = p {
                        if u == uri { return p.clone(); }
                    }
                }
            }
        }
        let used: std::collections::HashSet<String> = self.stack.iter()
            .filter_map(|n| match n {
                ResultNode::Element { namespaces, .. } => Some(namespaces),
                _ => None,
            })
            .flat_map(|ns| ns.iter().filter_map(|(p, _)| p.clone()))
            .collect();
        (1..)
            .map(|i| format!("{hint}_{i}"))
            .find(|p| !used.contains(p))
            .expect("infinite iterator finds an unused prefix")
    }

    /// Declare a namespace on the current element.  Inherited
    /// bindings — those whose nearest ancestor declaration is
    /// already this same `(prefix, uri)` pair — are skipped so the
    /// serialiser doesn't repeat them on every child.  A closer
    /// declaration with a *different* URI shadows the outer one, in
    /// which case this binding has to be re-emitted to undo the
    /// shadowing.
    /// Namespace URI of the element currently being built (the innermost
    /// open element), or `None` when no element is open.  Used to detect
    /// a default-namespace declaration on a no-namespace element
    /// (XTDE0440).
    pub fn current_element_uri(&self) -> Option<&str> {
        self.stack.iter().rev().find_map(|n| match n {
            ResultNode::Element { name, .. } => Some(name.uri.as_str()),
            _ => None,
        })
    }

    pub fn push_namespace_decl(&mut self, prefix: Option<String>, uri: String) {
        // Find the nearest in-scope binding for `prefix`; the
        // declaration is redundant only when that nearest binding
        // already matches `uri`.
        let nearest: Option<&str> = self.stack.iter().rev().find_map(|n| match n {
            ResultNode::Element { namespaces, .. } =>
                namespaces.iter().find(|(p, _)| *p == prefix).map(|(_, u)| u.as_str()),
            _ => None,
        });
        if nearest == Some(uri.as_str()) {
            return;
        }
        // XSLT 2.0 §5.7.1 / XTDE0410 — once non-attribute / non-
        // namespace content has been emitted, no further namespace
        // declarations may be added.  Same condition as push_attribute.
        if matches!(self.stack.last(),
            Some(ResultNode::Element { children, .. }) if !children.is_empty())
            && self.deferred_error.is_none()
        {
            let pref_disp = prefix.as_deref().unwrap_or("");
            self.deferred_error = Some(format!(
                "result sequence places namespace declaration '{pref_disp}' \
                 after element content (XTDE0410)"));
        }
        let Some(ResultNode::Element { namespaces, .. }) = self.stack.last_mut() else {
            return;
        };
        // A prefix already bound to a *different* URI is left alone —
        // the implicit-binding path (e.g. xsl:attribute fixing up its
        // own prefix for an explicit namespace=) relies on the runtime
        // synthesising a unique prefix elsewhere.  The strict XTDE0430
        // check lives on `push_namespace_decl_explicit` below, which
        // only xsl:namespace calls.
        if namespaces.iter().any(|(p, _)| *p == prefix) {
            return;
        }
        namespaces.push((prefix, uri));
    }

    /// As [`push_namespace_decl`], but enforces XSLT 2.0 §5.7.3 /
    /// XTDE0430 — binding the same prefix to two different URIs on
    /// one element is a dynamic error.  Called from the xsl:namespace
    /// instruction so explicit user declarations are validated, while
    /// the implicit fixups push_attribute performs (which may collide
    /// with an in-scope prefix by design) stay non-fatal.
    pub fn push_namespace_decl_explicit(
        &mut self, prefix: Option<String>, uri: String,
    ) {
        let conflict = matches!(self.stack.last(),
            Some(ResultNode::Element { namespaces, .. })
                if namespaces.iter().any(|(p, u)| *p == prefix && *u != uri));
        if conflict && self.deferred_error.is_none() {
            let pref_disp = prefix.as_deref().unwrap_or("");
            self.deferred_error = Some(format!(
                "result sequence binds the namespace prefix '{pref_disp}' \
                 to two different URIs on one element (XTDE0430)"));
            return;
        }
        self.push_namespace_decl(prefix, uri);
    }

    /// Emit a text node.  Adjacent text nodes are merged
    /// automatically — XSLT 1.0 §7.2 says contiguous character data
    /// in the result tree is treated as a single text node.
    pub fn push_text(&mut self, content: String, dose: bool) {
        if content.is_empty() { return; }
        // `no_text_merge` only suppresses merging at the OUTER level
        // (no element open) — inside an element body, XSLT 2.0
        // §5.7.2's text-node merging still applies (an LRE inside
        // an `as="element()*"` variable body normalises its own
        // content), so we always merge under an open element.
        let allow_merge = !self.no_text_merge || !self.stack.is_empty();
        if allow_merge {
            if let Some(slot) = self.current_children_mut() {
                if let Some(ResultNode::Text { content: last, dose: last_dose }) = slot.last_mut() {
                    if *last_dose == dose {
                        last.push_str(&content);
                        // Any literal/copied text breaks the
                        // atomic-adjacency rule (text node, not value).
                        self.last_was_atomic = false;
                        return;
                    }
                }
            }
        }
        self.last_was_atomic = false;
        self.push_node(ResultNode::Text { content, dose });
    }

    /// Emit a text node whose content is an atomised XPath value
    /// (number, string, boolean, typed atomic).  XSLT 2.0 §5.7.2
    /// sequence normalisation inserts a single space between
    /// adjacent atomic values in a sequence constructor — copies of
    /// real text/element nodes and literal `xsl:text` don't count.
    /// Tracking the "last emit was atomic" flag on the builder lets
    /// us slot that space in without reshaping the value model.
    pub fn push_atomic_text(&mut self, content: String) {
        if content.is_empty() {
            // An empty atomic still counts — `("", "")` should
            // normalise to a single space — so flip the flag.
            self.last_was_atomic = true;
            return;
        }
        if self.last_was_atomic {
            // Same merging rules as a literal text node, but
            // prefixed with a single space.
            self.push_text(format!(" {content}"), false);
        } else {
            self.push_text(content, false);
        }
        self.last_was_atomic = true;
    }

    pub fn push_comment(&mut self, content: String) {
        self.push_node(ResultNode::Comment(content));
    }

    pub fn push_pi(&mut self, target: String, data: String) {
        self.push_node(ResultNode::ProcessingInstruction { target, data });
    }

    fn push_node(&mut self, node: ResultNode) {
        match self.stack.last_mut() {
            Some(ResultNode::Element { children, .. }) => children.push(node),
            _ => self.top.push(node),
        }
    }

    /// Splice an already-built result node into the output at the
    /// current position (respecting any open element).  Used to replay
    /// a captured sub-tree — e.g. the retained content of
    /// `xsl:where-populated`.
    pub fn push_built_node(&mut self, node: ResultNode) {
        self.push_node(node);
    }

    fn current_children_mut(&mut self) -> Option<&mut Vec<ResultNode>> {
        match self.stack.last_mut() {
            Some(ResultNode::Element { children, .. }) => Some(children),
            _ => Some(&mut self.top),
        }
    }

    /// Consume the builder, returning the final list of top-level
    /// result-tree children.  Any still-open elements are silently
    /// closed — the evaluator should never leave an unclosed element
    /// on a normal-completion path; this guard prevents data loss
    /// on the error paths.
    pub fn finish(mut self) -> Vec<ResultNode> {
        while !self.stack.is_empty() {
            self.close_element();
        }
        self.top
    }

    /// True when nothing has been written to this builder yet — no
    /// completed top-level nodes and no element currently open.  Used to
    /// decide whether an `xsl:result-document` targeting the principal
    /// URI is the sole writer of that destination.
    pub fn is_empty(&self) -> bool {
        self.top.is_empty() && self.stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::QName;

    fn qn(local: &str) -> QName {
        QName { prefix: None, local: local.to_string(), uri: String::new() }
    }

    #[test]
    fn empty_builder_finishes_with_no_nodes() {
        let b = ResultBuilder::new();
        assert!(b.finish().is_empty());
    }

    #[test]
    fn nested_elements_attach_to_parent() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("outer"));
        b.open_element(qn("inner"));
        b.close_element();
        b.close_element();
        let nodes = b.finish();
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            ResultNode::Element { children, .. } => assert_eq!(children.len(), 1),
            _ => panic!(),
        }
    }

    #[test]
    fn adjacent_text_merges() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("p"));
        b.push_text("Hello, ".into(), false);
        b.push_text("world!".into(), false);
        b.close_element();
        let nodes = b.finish();
        match &nodes[0] {
            ResultNode::Element { children, .. } => {
                assert_eq!(children.len(), 1, "should have merged two texts");
                if let ResultNode::Text { content, .. } = &children[0] {
                    assert_eq!(content, "Hello, world!");
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn unbalanced_open_is_recovered_in_finish() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("oops"));
        let nodes = b.finish();
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn push_attribute_outside_element_emits_standalone_node() {
        let mut b = ResultBuilder::new();
        // No open element — the attribute becomes a parentless
        // attribute node (consumed later by copy-of / apply-templates)
        // rather than being dropped.
        b.push_attribute(qn("id"), "x".into());
        let top = b.finish();
        assert!(matches!(top.as_slice(),
            [ResultNode::Attribute { name, value }]
                if name.local == "id" && value == "x"));
    }

    #[test]
    fn push_attribute_replaces_same_expanded_name() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("r"));
        b.push_attribute(qn("id"), "first".into());
        b.push_attribute(qn("id"), "second".into());
        b.close_element();
        let nodes = b.finish();
        match &nodes[0] {
            ResultNode::Element { attributes, .. } => {
                assert_eq!(attributes.len(), 1, "duplicate names should merge");
                assert_eq!(attributes[0].1, "second", "later attribute wins");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn push_namespace_decl_outside_element_is_silent_noop() {
        let mut b = ResultBuilder::new();
        b.push_namespace_decl(Some("foo".into()), "urn:foo".into());
        assert!(b.finish().is_empty());
    }

    #[test]
    fn push_namespace_decl_dedupes_same_prefix() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("r"));
        b.push_namespace_decl(Some("ns".into()), "urn:n1".into());
        // Same prefix again → should be deduped (first declaration wins).
        b.push_namespace_decl(Some("ns".into()), "urn:n2".into());
        b.close_element();
        let nodes = b.finish();
        match &nodes[0] {
            ResultNode::Element { namespaces, .. } => {
                assert_eq!(namespaces.len(), 1);
                assert_eq!(namespaces[0].1, "urn:n1");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn push_text_empty_is_noop() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("r"));
        b.push_text(String::new(), false);
        b.close_element();
        match &b.finish()[0] {
            ResultNode::Element { children, .. } => assert!(children.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn push_text_with_different_dose_does_not_merge() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("r"));
        b.push_text("a".into(), false);
        b.push_text("b".into(), true);  // different dose → don't merge
        b.close_element();
        match &b.finish()[0] {
            ResultNode::Element { children, .. } => assert_eq!(children.len(), 2),
            _ => panic!(),
        }
    }

    #[test]
    fn push_text_at_top_level_when_no_element_open() {
        // current_children_mut() returns &mut self.top when stack is empty.
        let mut b = ResultBuilder::new();
        b.push_text("hello".into(), false);
        b.push_text(" world".into(), false);
        let nodes = b.finish();
        // Two texts at top level should merge (same dose) — verifies the
        // current_children_mut top-level branch is taken.
        assert_eq!(nodes.len(), 1);
        match &nodes[0] {
            ResultNode::Text { content, .. } => assert_eq!(content, "hello world"),
            _ => panic!(),
        }
    }

    #[test]
    fn push_comment_at_top_level() {
        let mut b = ResultBuilder::new();
        b.push_comment(" hi ".into());
        let nodes = b.finish();
        match &nodes[0] {
            ResultNode::Comment(s) => assert_eq!(s, " hi "),
            _ => panic!(),
        }
    }

    #[test]
    fn push_pi_at_top_level() {
        let mut b = ResultBuilder::new();
        b.push_pi("target".into(), "data".into());
        let nodes = b.finish();
        match &nodes[0] {
            ResultNode::ProcessingInstruction { target, data } => {
                assert_eq!(target, "target");
                assert_eq!(data, "data");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn push_pi_inside_element() {
        let mut b = ResultBuilder::new();
        b.open_element(qn("r"));
        b.push_pi("php".into(), "echo".into());
        b.close_element();
        match &b.finish()[0] {
            ResultNode::Element { children, .. } => {
                assert!(matches!(&children[0],
                    ResultNode::ProcessingInstruction { target, .. } if target == "php"));
            }
            _ => panic!(),
        }
    }
}
