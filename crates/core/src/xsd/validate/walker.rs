//! Arena-DOM event source for the XSD validator.
//!
//! Walks a [`Document`](sup_xml_tree::dom::Document) and emits the
//! same event stream that `XmlReader` produces from raw XML bytes,
//! so [`Schema::validate_doc`](super::super::schema::Schema)
//! reuses the streaming validator's state machine unchanged.
//!
//! The walker is deliberately minimal: a single LIFO task stack
//! drives traversal in document order, so each `next_into` call is
//! a constant-time pop + emit.  No recursion, no allocator beyond
//! the task vector.

use std::borrow::Cow;

use sup_xml_tree::dom::{Document, Node, NodeKind};

use crate::reader::{Attr, EventInto};

use super::XsdEventSource;

/// Pending traversal work.  The stack is built up-front by [`new`]
/// and consumed by [`next_into`]; nodes are visited in document
/// order with `Leave` markers interleaved so [`EventInto::EndElement`]
/// events are emitted after the element's subtree.
enum Task<'doc> {
    /// Enter this element: emit `StartElement`, populate attrs,
    /// then queue the subtree's children + a matching `Leave`.
    Enter(&'doc Node<'doc>),
    /// Emit `EndElement` for the named element.  Pushed onto the
    /// stack right after `Enter`'s children so it fires after the
    /// subtree has been visited.
    Leave(&'doc str),
    /// Leaf node — emits exactly one `Text` / `CData` / `Comment` /
    /// `Pi` event.  Document-level comments and PIs the validator
    /// skips also flow through here; the validator's main loop is
    /// already prepared to ignore them.
    Leaf(&'doc Node<'doc>),
}

pub(crate) struct DocumentEventSource<'doc> {
    /// LIFO stack of work.  `pop`'d entries drive each `next_into`
    /// call.  Empty → emit `Eof`.
    tasks: Vec<Task<'doc>>,
    /// The document being walked — its arena backs the synthesized
    /// `xmlns:prefix` attribute names emitted for declarations that
    /// live on the `ns_def` chain (see `next_into`).
    doc: &'doc Document,
    /// The element whose `StartElement` was most recently emitted.  The
    /// validator calls `fill_default_attr` against it to apply schema
    /// attribute value-constraints to the live tree (`apply_attribute_defaults`).
    current_elem: std::cell::Cell<Option<&'doc Node<'doc>>>,
}

impl<'doc> DocumentEventSource<'doc> {
    pub(crate) fn new(doc: &'doc Document) -> Self {
        let mut tasks: Vec<Task<'doc>> = Vec::new();
        // Push document-level children in reverse so the head pops
        // first.  `first_sibling()` returns the first node in the
        // document-order chain (prolog comments/PIs, root, epilogue
        // comments/PIs), matching what `XmlReader` emits.
        let mut nodes: Vec<&'doc Node<'doc>> = Vec::new();
        let mut cur: Option<&'doc Node<'doc>> = Some(doc.first_sibling());
        while let Some(n) = cur {
            nodes.push(n);
            cur = n.next_sibling.get();
        }
        for n in nodes.into_iter().rev() {
            push_task_for(&mut tasks, n);
        }
        Self { tasks, doc, current_elem: std::cell::Cell::new(None) }
    }
}

/// Classify a node and queue the appropriate task.  Elements get an
/// `Enter` (which expands at pop time into StartElement + children +
/// Leave); everything else is a leaf event.
fn push_task_for<'doc>(tasks: &mut Vec<Task<'doc>>, n: &'doc Node<'doc>) {
    match n.kind {
        NodeKind::Element => tasks.push(Task::Enter(n)),
        // Other kinds: Text, CData, Comment, PI, EntityRef.  All are
        // single-event leaves.  Attribute/Document never appear on a
        // real Node in either build; treat defensively as a skipped
        // leaf rather than panicking.
        _ => tasks.push(Task::Leaf(n)),
    }
}

impl<'doc> XsdEventSource<'doc> for DocumentEventSource<'doc> {
    fn next_into(
        &mut self, attr_buf: &mut Vec<Attr<'doc>>,
    ) -> crate::error::Result<EventInto<'doc>> {
        attr_buf.clear();
        loop {
            let Some(task) = self.tasks.pop() else {
                return Ok(EventInto::Eof);
            };
            match task {
                Task::Enter(elem) => {
                    // Schedule the matching Leave, then push children
                    // in reverse so they pop in document order.
                    self.tasks.push(Task::Leave(elem.name()));
                    let mut children: Vec<&'doc Node<'doc>> = elem.children().collect();
                    while let Some(c) = children.pop() {
                        push_task_for(&mut self.tasks, c);
                    }
                    // Populate attrs.  The validator's `push_ns_scope`
                    // expects xmlns declarations to arrive as `xmlns`/
                    // `xmlns:prefix` attributes, the way a streaming
                    // parse surfaces them.  Where those declarations are
                    // stored depends on build and parse mode (attribute
                    // list vs. the `ns_def` chain), so read real
                    // attributes and namespace declarations through their
                    // respective build-independent accessors and merge
                    // them into the one view the validator consumes.
                    for a in elem.attributes() {
                        let n = a.name();
                        if n == "xmlns" || n.starts_with("xmlns:") { continue; }
                        attr_buf.push(Attr {
                            name:  n,
                            value: Cow::Borrowed(a.value()),
                        });
                    }
                    for (prefix, href) in elem.ns_declarations() {
                        let name: &'doc str = match prefix {
                            None    => "xmlns",
                            // `ns_def` keeps only the bare prefix, so
                            // rebuild the `xmlns:prefix` lexical form in
                            // the document arena.
                            Some(p) => self.doc.bump().alloc_str(&format!("xmlns:{p}")),
                        };
                        attr_buf.push(Attr { name, value: Cow::Borrowed(href) });
                    }
                    self.current_elem.set(Some(elem));
                    return Ok(EventInto::StartElement {
                        name: Cow::Borrowed(elem.name()),
                    });
                }
                Task::Leave(name) => {
                    return Ok(EventInto::EndElement {
                        name: Cow::Borrowed(name),
                    });
                }
                Task::Leaf(n) => match n.kind {
                    NodeKind::Text =>
                        return Ok(EventInto::Text(Cow::Borrowed(n.content()))),
                    NodeKind::CData =>
                        return Ok(EventInto::CData(Cow::Borrowed(n.content()))),
                    NodeKind::Comment =>
                        return Ok(EventInto::Comment(Cow::Borrowed(n.content()))),
                    NodeKind::Pi =>
                        return Ok(EventInto::Pi {
                            target:  Cow::Borrowed(n.name()),
                            content: Cow::Borrowed(n.content()),
                        }),
                    NodeKind::EntityRef =>
                        return Ok(EventInto::EntityRef {
                            name: Cow::Borrowed(n.name()),
                        }),
                    // Defensive: Element shouldn't reach here (it
                    // becomes Enter in push_task_for); Attribute /
                    // Document don't appear on real Nodes.  Skip
                    // and continue the loop.
                    _ => continue,
                },
            }
        }
    }

    /// DOM walker has no byte offsets — the source was parsed
    /// earlier and re-serialising for offsets isn't worth the cost.
    /// Diagnostics from `validate_doc` are reported with
    /// `(line, col) = (0, 0)`.
    fn last_start_offset(&self) -> Option<usize> { None }
    fn src_offset(&self) -> usize { 0 }
    fn line_col_at(&self, _offset: usize) -> (u32, u32) { (0, 0) }

    fn current_node_key(&self) -> Option<usize> {
        self.current_elem.get().map(|n| n as *const Node<'doc> as usize)
    }

    fn fill_default_attr(&self, name: &str, value: &str) {
        let Some(elem) = self.current_elem.get() else { return };
        // Copy name/value into the document arena so the new attribute's
        // strings share the tree's lifetime, then link it onto the element.
        let n = self.doc.bump().alloc_str(name);
        let v = self.doc.bump().alloc_str(value);
        let attr = self.doc.new_attribute(n, v);
        self.doc.append_attribute(elem, attr);
    }
}
