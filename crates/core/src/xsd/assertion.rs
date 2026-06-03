//! XSD 1.1 `xs:assert` / `xs:assertion` evaluation.
//!
//! The validator captures a [`ChildSnapshot`](super::validate::ChildSnapshot)
//! tree for any element whose type carries assertions; we turn that tree
//! into a [`SnapshotIndex`] implementing [`DocIndexLike`] and feed it to
//! the existing XPath evaluator.
//!
//! Two evaluation contexts:
//!
//! * `xs:assert` (complex type) — context node is the element being
//!   validated; XPath expression sees its attributes and descendants.
//! * `xs:assertion` (simple-type facet) — `$value` is bound to the
//!   parsed atomic value; the context node is a synthetic root.
//!
//! Notes:
//!
//! * The expression is reparsed at each evaluation today (no caching).
//!   For schemas with many instances each carrying assertions this would
//!   be the obvious follow-up — store `Arc<Expr>` on each
//!   [`Assertion`](super::schema::Assertion) at compile time.
//! * Namespace prefixes in the test expression resolve against the
//!   snapshot captured at parse time (see
//!   [`Assertion::namespaces`](super::schema::Assertion::namespaces)).

use std::ops::Range;

use crate::xpath::{eval::Value, parse_xpath, DocIndexLike, NodeId, XPathNodeKind};
use crate::xpath::eval::{ForeignNodePtr, XPathBindings};

use super::schema::{Assertion, QName};
use super::validate::ChildSnapshot;

/// Flat-node [`DocIndexLike`] over an [`ChildSnapshot`] subtree.
///
/// Layout (post-`build`):
///   * id 0  — synthetic Document
///   * id 1  — root element (the element being validated)
///   * id 2..N+1  — root's attributes (contiguous)
///   * id N+1..  — descendant elements + their attrs + text nodes
///                  in document order (each element's `attr_range` is
///                  contiguous starting immediately after itself)
pub(super) struct SnapshotIndex {
    nodes: Vec<INode>,
    children_of: Vec<Vec<NodeId>>,
    attr_start:  Vec<NodeId>,
    attr_end:    Vec<NodeId>,
}

#[derive(Debug, Clone)]
enum NodeData {
    Document,
    Element { qname: QName },
    Attribute { qname: QName, value: String },
    Text(String),
}

#[derive(Debug, Clone)]
struct INode {
    parent: Option<NodeId>,
    data:   NodeData,
}

impl SnapshotIndex {
    /// Build a synthetic index over a single root element snapshot.
    /// Returns the index plus the `NodeId` of the root element (the
    /// XPath context node for `xs:assert` evaluation).
    pub fn build(root: &ChildSnapshot) -> (Self, NodeId) {
        let mut idx = Self {
            nodes:        Vec::new(),
            children_of:  Vec::new(),
            attr_start:   Vec::new(),
            attr_end:     Vec::new(),
        };
        // id 0: synthetic Document
        idx.push_node(NodeData::Document, None);
        // id 1: root element
        let root_id = idx.add_element(root, /*parent=*/ 0);
        // Document's children = [root]
        idx.children_of[0].push(root_id);
        (idx, root_id)
    }

    fn push_node(&mut self, data: NodeData, parent: Option<NodeId>) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(INode { parent, data });
        self.children_of.push(Vec::new());
        self.attr_start.push(0);
        self.attr_end.push(0);
        id
    }

    fn add_element(&mut self, el: &ChildSnapshot, parent: NodeId) -> NodeId {
        let id = self.push_node(NodeData::Element { qname: el.name.clone() }, Some(parent));
        // Attributes: contiguous block immediately after this element.
        let a_start = self.nodes.len();
        for (aname, avalue) in &el.attrs {
            self.push_node(
                NodeData::Attribute { qname: aname.clone(), value: avalue.clone() },
                Some(id),
            );
        }
        let a_end = self.nodes.len();
        self.attr_start[id] = a_start;
        self.attr_end[id]   = a_end;
        // Text child — single concatenated text node if non-empty.
        if !el.text.is_empty() {
            let t = self.push_node(NodeData::Text(el.text.clone()), Some(id));
            self.children_of[id].push(t);
        }
        // Recurse into child elements in document order.
        for c in &el.children {
            let cid = self.add_element(c, id);
            self.children_of[id].push(cid);
        }
        id
    }
}

impl DocIndexLike for SnapshotIndex {
    fn children(&self, id: NodeId) -> &[NodeId] {
        self.children_of.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }
    fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id).and_then(|n| n.parent)
    }
    fn attr_range(&self, id: NodeId) -> Range<NodeId> {
        self.attr_start.get(id).copied().unwrap_or(0)
            .. self.attr_end.get(id).copied().unwrap_or(0)
    }
    fn kind(&self, id: NodeId) -> XPathNodeKind {
        match self.nodes.get(id).map(|n| &n.data) {
            Some(NodeData::Document)        => XPathNodeKind::Document,
            Some(NodeData::Element { .. })  => XPathNodeKind::Element,
            Some(NodeData::Attribute { .. })=> XPathNodeKind::Attribute,
            Some(NodeData::Text(_))         => XPathNodeKind::Text,
            None                            => XPathNodeKind::Text,
        }
    }
    fn pi_target(&self, _id: NodeId) -> &str { "" }
    fn string_value(&self, id: NodeId) -> String {
        match self.nodes.get(id).map(|n| &n.data) {
            Some(NodeData::Attribute { value, .. }) => value.clone(),
            Some(NodeData::Text(s))                 => s.clone(),
            Some(NodeData::Element { .. })          => self.element_string_value(id),
            Some(NodeData::Document)                => self.children(id).iter()
                .map(|&c| self.string_value(c)).collect(),
            None => String::new(),
        }
    }
    fn node_name(&self, id: NodeId) -> &str {
        match self.nodes.get(id).map(|n| &n.data) {
            Some(NodeData::Element { qname })       => qname.local.as_ref(),
            Some(NodeData::Attribute { qname, .. }) => qname.local.as_ref(),
            _ => "",
        }
    }
    fn local_name(&self, id: NodeId) -> &str {
        // QName::local is the local part already.
        self.node_name(id)
    }
    fn namespace_uri(&self, id: NodeId) -> &str {
        match self.nodes.get(id).map(|n| &n.data) {
            Some(NodeData::Element { qname })       => qname.namespace.as_deref().unwrap_or(""),
            Some(NodeData::Attribute { qname, .. }) => qname.namespace.as_deref().unwrap_or(""),
            _ => "",
        }
    }
}

impl SnapshotIndex {
    fn element_string_value(&self, id: NodeId) -> String {
        // XPath 1.0 § 5.2 string-value of an element: concatenation of
        // every descendant text node in document order.
        let mut out = String::new();
        self.collect_text(id, &mut out);
        out
    }
    fn collect_text(&self, id: NodeId, out: &mut String) {
        for &c in self.children(id) {
            match self.nodes.get(c).map(|n| &n.data) {
                Some(NodeData::Text(s)) => out.push_str(s),
                Some(NodeData::Element { .. }) => self.collect_text(c, out),
                _ => {}
            }
        }
    }
}

// ── evaluation ────────────────────────────────────────────────────────────

/// Bindings for `xs:assert` / `xs:assertion` evaluation.
///
/// * `value` — set for simpleType `xs:assertion` so the XPath
///   reference `$value` resolves to the parsed atomic value.
/// * `namespaces` — the snapshot captured at schema-parse time.
struct AssertBindings<'a> {
    value:      Option<Value>,
    namespaces: &'a [(Option<String>, String)],
}

impl<'a> XPathBindings for AssertBindings<'a> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        for (p, uri) in self.namespaces {
            if p.as_deref() == Some(prefix) {
                return Some(uri.clone());
            }
        }
        None
    }
    fn variable(&self, name: &str) -> Option<Value> {
        if name == "value" { self.value.clone() } else { None }
    }
    fn foreign_string_value(&self, _p: ForeignNodePtr) -> String { String::new() }
}

/// Outcome of evaluating one assertion.
#[derive(Debug)]
pub(super) enum AssertOutcome {
    /// Assertion's test evaluated to `true`.  Validation continues.
    Pass,
    /// Assertion's test evaluated to `false`.  Validation MUST fail
    /// with `cvc-assertion`.
    Fail,
    /// The assertion's XPath couldn't be parsed or used a feature the
    /// evaluator doesn't support yet.  Treated as `Pass` by callers
    /// who prefer false-positives to false-negatives, but logged.
    Unevaluable(String),
}

/// Evaluate one `xs:assert` against `snapshot` (the element being
/// validated).  Returns whether the assertion holds.
pub(super) fn eval_complex_assert(
    a: &Assertion,
    snapshot: &ChildSnapshot,
) -> AssertOutcome {
    let (idx, root) = SnapshotIndex::build(snapshot);
    eval_with(a, &idx, root, None)
}

/// Evaluate one `xs:assertion` (simpleType facet) with `$value` bound
/// to the parsed atomic value's string form.
pub(super) fn eval_simple_assertion(
    a: &Assertion,
    value: &str,
) -> AssertOutcome {
    // Synthesise a one-element snapshot so the context node is well-
    // defined; XPath constructs that reference `.` see the empty
    // element.
    let snap = ChildSnapshot {
        name: QName { namespace: None, local: std::sync::Arc::from("") },
        attrs: Vec::new(),
        text:  String::new(),
        children: Vec::new(),
    };
    let (idx, _root) = SnapshotIndex::build(&snap);
    // Context for simple-type assertions is the document root, not the
    // empty synthetic element — there is no "current node" for a value-
    // type validation.  XPath references to `.` evaluate to the
    // document and stringify to empty.
    eval_with(a, &idx, 0, Some(Value::String(value.to_string())))
}

fn eval_with<I: DocIndexLike>(
    a: &Assertion,
    idx: &I,
    context_node: NodeId,
    value: Option<Value>,
) -> AssertOutcome {
    let expr = match parse_xpath(&a.test) {
        Ok(e) => e,
        Err(e) => return AssertOutcome::Unevaluable(format!("parse: {e}")),
    };
    let bindings = AssertBindings {
        value,
        namespaces: &a.namespaces,
    };
    match crate::xpath::eval::eval_to_bool(&expr, idx, context_node, &bindings) {
        Ok(b)  => if b { AssertOutcome::Pass } else { AssertOutcome::Fail },
        Err(e) => AssertOutcome::Unevaluable(format!("eval: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::xsd::{Schema, SchemaOptions};
    use crate::xsd::schema::SchemaVersion;

    fn xsd11(xsd: &str) -> Schema {
        let mut opts = SchemaOptions::default();
        opts.version = SchemaVersion::Xsd11;
        Schema::compile_str_with_options(xsd, opts).expect("schema compile")
    }

    #[test]
    fn complex_assert_passes_when_test_true() {
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="X" type="XType"/>
              <xs:complexType name="XType">
                <xs:attribute name="min" type="xs:int"/>
                <xs:attribute name="max" type="xs:int"/>
                <xs:assert test="@min le @max"/>
              </xs:complexType>
            </xs:schema>"#);
        assert!(s.validate_str(r#"<X min="1" max="10"/>"#).is_ok());
    }

    #[test]
    fn complex_assert_fails_when_test_false() {
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="X" type="XType"/>
              <xs:complexType name="XType">
                <xs:attribute name="min" type="xs:int"/>
                <xs:attribute name="max" type="xs:int"/>
                <xs:assert test="@min le @max"/>
              </xs:complexType>
            </xs:schema>"#);
        let err = s.validate_str(r#"<X min="20" max="10"/>"#);
        assert!(err.is_err(), "expected assertion failure, got {err:?}");
    }

    #[test]
    fn complex_assert_ends_with() {
        // XPath 2.0 ends-with on a child element.
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="X" type="XType"/>
              <xs:complexType name="XType">
                <xs:sequence>
                  <xs:element name="suffix" type="xs:string"/>
                </xs:sequence>
                <xs:assert test="ends-with(suffix, 'end')"/>
              </xs:complexType>
            </xs:schema>"#);
        assert!(s.validate_str(r#"<X><suffix>the end</suffix></X>"#).is_ok());
        assert!(s.validate_str(r#"<X><suffix>the start</suffix></X>"#).is_err());
    }

    #[test]
    fn complex_assert_exists_and_empty() {
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="X" type="XType"/>
              <xs:complexType name="XType">
                <xs:sequence>
                  <xs:element name="opt" type="xs:string" minOccurs="0"/>
                </xs:sequence>
                <xs:attribute name="flag" type="xs:string"/>
                <xs:assert test="exists(opt) or empty(@flag)"/>
              </xs:complexType>
            </xs:schema>"#);
        // exists(opt): true → passes.
        assert!(s.validate_str(r#"<X><opt>a</opt></X>"#).is_ok());
        // exists(opt)=false but empty(@flag)=true → passes.
        assert!(s.validate_str(r#"<X/>"#).is_ok());
        // exists(opt)=false and empty(@flag)=false → fails.
        assert!(s.validate_str(r#"<X flag="set"/>"#).is_err());
    }

    #[test]
    fn simple_assertion_with_value_var() {
        // `$value` is bound to the parsed atomic value.
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="N">
                <xs:simpleType>
                  <xs:restriction base="xs:int">
                    <xs:assertion test="$value mod 2 = 0"/>
                  </xs:restriction>
                </xs:simpleType>
              </xs:element>
            </xs:schema>"#);
        assert!(s.validate_str(r#"<N>4</N>"#).is_ok());
        let bad = s.validate_str(r#"<N>3</N>"#);
        assert!(bad.is_err(), "expected failure on odd value, got {bad:?}");
    }

    #[test]
    fn complex_assert_string_length_on_child() {
        // count() over a child element accessed through the snapshot.
        let s = xsd11(r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
              <xs:element name="X" type="XType"/>
              <xs:complexType name="XType">
                <xs:sequence>
                  <xs:element name="msg" type="xs:string"/>
                </xs:sequence>
                <xs:assert test="string-length(msg) le 5"/>
              </xs:complexType>
            </xs:schema>"#);
        assert!(s.validate_str(r#"<X><msg>hi</msg></X>"#).is_ok());
        let bad = s.validate_str(r#"<X><msg>too long</msg></X>"#);
        assert!(bad.is_err(), "expected failure on string-length > 5, got {bad:?}");
    }
}
