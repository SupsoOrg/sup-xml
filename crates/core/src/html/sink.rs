#![allow(unsafe_code)] // see module docs

//! `TreeSink` that builds a [`sup_xml_tree::dom::Document`] directly.
//!
//! Mirror of [`super::sink`] but writes nodes into a bumpalo arena via
//! [`DocumentBuilder`] rather than an intermediate `Vec<SinkNode>`.  Per-node
//! allocation cost drops to a pointer bump; drop is free per node.
//!
//! # Why pragmatic `unsafe`
//!
//! [`TreeSink::Handle`] has no lifetime parameter — html5ever stores handles in
//! its open-elements stack as plain values, with only `Clone` as a bound.  Arena
//! nodes are `&'doc Node<'doc>`, which carry a lifetime, so we can't put
//! them straight into the Handle slot.
//!
//! We use type-erased raw pointers (`*const ()`) as the Handle.  Bumpalo never
//! relocates allocations, so addresses are stable for the life of the builder
//! / Document.  The same trick is used by [`crate::parser`] — see its
//! `ErasedNodePtr` for the canonical safety argument.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};

use html5ever::interface::tree_builder::{
    ElementFlags, NodeOrText, QuirksMode as H5QuirksMode, TreeSink,
};
use html5ever::tendril::StrTendril;
use html5ever::{Attribute as H5Attribute, QualName};
use markup5ever::interface::tree_builder::ElemName;
use markup5ever::{LocalName, Namespace as H5Namespace};

use sup_xml_tree::dom::{Document, DocumentBuilder, Node};
use sup_xml_tree::{HtmlDoctype, HtmlMeta, QuirksMode as TreeQuirksMode};

use crate::error::{ErrorDomain, ErrorLevel, XmlError};

use super::options::HtmlParseOptions;

// ── erased pointer helpers ──────────────────────────────────────────────────

/// Type-erased pointer to either a `Node` (real allocation) or the synthetic
/// document sentinel.  Stable across html5ever's stack churn because bumpalo
/// allocations don't move.
type ErasedHandle = *const ();

/// Sentinel handle value for the document root.  Distinct from any real
/// allocation: bumpalo never returns address 0.  Doctype handles use a second
/// sentinel — html5ever never inspects them after `append_doctype_to_document`.
const DOCUMENT_SENTINEL: ErasedHandle = 0x1 as *const ();
const DOCTYPE_SENTINEL:  ErasedHandle = 0x2 as *const ();

#[inline]
fn erase(n: &Node<'_>) -> ErasedHandle {
    n as *const Node<'_> as *const ()
}

/// # Safety
///
/// `p` must have been produced by [`erase`] on a node still alive in the
/// builder's arena.  The arena outlives every call to this function — it
/// lives in `BatchSinkArena::builder` until [`finalize_arena`] consumes the
/// sink.
#[inline]
unsafe fn unerase<'a>(p: ErasedHandle) -> &'a Node<'a> {
    debug_assert!(!is_sentinel(p), "unerase called on sentinel handle");
    unsafe { &*(p as *const Node<'a>) }
}

#[inline]
fn is_document(p: ErasedHandle) -> bool { p == DOCUMENT_SENTINEL }
#[inline]
fn is_doctype(p: ErasedHandle)  -> bool { p == DOCTYPE_SENTINEL  }
#[inline]
fn is_sentinel(p: ErasedHandle) -> bool { is_document(p) || is_doctype(p) }

// ── owned elem-name ─────────────────────────────────────────────────────────

/// Owned form of an element's qualified name returned from
/// [`TreeSink::elem_name`].  Owns clones of the atoms (refcount bumps; cheap).
pub struct OwnedElemName {
    inner: QualName,
}

impl std::fmt::Debug for OwnedElemName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &H5Namespace { &self.inner.ns }
    fn local_name(&self) -> &LocalName { &self.inner.local }
}

// ── per-element qname store ─────────────────────────────────────────────────

/// We need to answer [`TreeSink::elem_name`] for any element handle, but the
/// arena node only stores the local name as `&'doc str` — not the original
/// `QualName` with its namespace atom.  Keep a parallel map keyed by the
/// node pointer.  Cheap: only entries for real elements, populated on
/// `create_element`.
type QNameMap = RefCell<rustc_hash::FxHashMap<ErasedHandle, QualName>>;

// ── pending-text accumulator ────────────────────────────────────────────────

/// Lazy text-accumulation buffer for an in-progress run of `append_text`
/// chunks targeting the same text node.
///
/// html5ever delivers a single logical text node (e.g. the body of a
/// `<script>` or `<style>` raw-text element, or any long run of `Text`
/// tokens) as a *sequence* of [`StrTendril`] chunks.  Without buffering,
/// each chunk would read the existing arena content out, build a fresh
/// `String` of `cur + chunk`, and copy that back into the arena —
/// O(N²) work *and* O(N²) arena footprint in the number of chunks.
///
/// Instead, the merge path appends into [`PendingText::buf`] (amortized
/// linear via `String`'s growth strategy) and defers the single arena
/// `alloc_str` until [`BatchSinkArena::flush_pending_text`] runs.
struct PendingText {
    /// Text node being accumulated into.
    handle: ErasedHandle,
    /// Full accumulated content for the node — flush copies this once.
    buf:    String,
}

// ── sink ────────────────────────────────────────────────────────────────────

/// Batch sink that html5ever drives, accumulating into a [`DocumentBuilder`].
pub(super) struct BatchSinkArena {
    builder:    DocumentBuilder,
    /// Children of the synthetic document node, in append order.  Real
    /// allocations only — doctype sentinels are filtered before being
    /// recorded here.
    doc_children: RefCell<Vec<ErasedHandle>>,
    /// QualName map for `elem_name`.  See [`QNameMap`].
    qnames:     QNameMap,
    /// DOCTYPE info, captured into [`HtmlMeta`] at finalize time.
    doctype:    RefCell<Option<HtmlDoctype>>,
    quirks_mode: Cell<H5QuirksMode>,
    opts:       HtmlParseOptions,
    parse_errors: RefCell<Vec<XmlError>>,
    /// Total text bytes accumulated.  Compared against
    /// [`HtmlParseOptions::max_text_bytes`].
    text_bytes: Cell<u64>,
    /// In-progress text run — see [`PendingText`].  `None` between runs;
    /// switched on the first `append_text` to a given target, switched
    /// off (and committed to the arena) by [`flush_pending_text`] when
    /// the run ends.
    pending_text: RefCell<Option<PendingText>>,
    /// Once true, further mutations become no-ops and `finalize` reports the
    /// fatal error.  html5ever can't be stopped mid-parse so we drain it
    /// quietly instead.
    aborted:    Cell<bool>,
    /// First fatal error encountered.
    fatal_error: RefCell<Option<XmlError>>,
    /// Set once the first element is created.  html5ever reports a parse
    /// error in the "initial" insertion mode for any doctype-less document
    /// (the missing-DOCTYPE error), emitted *before* the root element is
    /// built.  libxml2's HTML parser does not treat a missing DOCTYPE — or
    /// other prolog slop — as a well-formedness error, so errors that
    /// arrive before the tree starts are dropped to match it.
    tree_started: Cell<bool>,
}

impl BatchSinkArena {
    pub(super) fn new(opts: HtmlParseOptions) -> Self {
        Self::new_inner(DocumentBuilder::new(), opts)
    }

    /// Constructor that intern-allocates element/attribute names
    /// through a caller-supplied refcounted dict.  See
    /// [`DocumentBuilder::new_with_dict`] for the lifetime contract.
    #[cfg(feature = "c-abi")]
    pub(super) unsafe fn new_with_dict(
        opts: HtmlParseOptions,
        dict: *mut sup_xml_tree::dict::Dict,
    ) -> Self {
        // SAFETY: caller asserts dict is refcount-managed.
        Self::new_inner(unsafe { DocumentBuilder::new_with_dict(dict) }, opts)
    }

    /// As [`new_with_dict`] but also adopts a caller-supplied
    /// [`bumpalo::Bump`] arena (refcounted via [`std::sync::Arc`]).
    /// See [`DocumentBuilder::new_with_dict_and_arena`] for the
    /// architectural rationale (cross-doc graft safety).
    #[cfg(feature = "c-abi")]
    pub(super) unsafe fn new_with_dict_and_arena(
        opts:  HtmlParseOptions,
        dict:  *mut sup_xml_tree::dict::Dict,
        arena: std::sync::Arc<bumpalo::Bump>,
    ) -> Self {
        // SAFETY: caller asserts dict is refcount-managed.
        Self::new_inner(
            unsafe { DocumentBuilder::new_with_dict_and_arena(dict, arena) },
            opts,
        )
    }

    fn new_inner(builder: DocumentBuilder, opts: HtmlParseOptions) -> Self {
        Self {
            builder,
            doc_children: RefCell::new(Vec::new()),
            qnames: RefCell::new(rustc_hash::FxHashMap::default()),
            doctype: RefCell::new(None),
            quirks_mode: Cell::new(H5QuirksMode::NoQuirks),
            opts,
            parse_errors: RefCell::new(Vec::new()),
            text_bytes: Cell::new(0),
            pending_text: RefCell::new(None),
            aborted: Cell::new(false),
            fatal_error: RefCell::new(None),
            tree_started: Cell::new(false),
        }
    }

    fn record_error(&self, msg: impl Into<String>, level: ErrorLevel) {
        let err = XmlError::new(ErrorDomain::Html, level, msg);
        if level == ErrorLevel::Fatal && self.fatal_error.borrow().is_none() {
            *self.fatal_error.borrow_mut() = Some(err.clone());
        }
        self.parse_errors.borrow_mut().push(err);
    }

    fn abort(&self, msg: impl Into<String>) {
        if !self.aborted.get() {
            self.aborted.set(true);
            self.record_error(msg, ErrorLevel::Fatal);
        }
    }

    /// Allocate a new element in the arena and remember its `QualName`.
    fn alloc_element(&self, name: QualName, attrs: Vec<H5Attribute>) -> ErasedHandle {
        let local_name: &str = &name.local;
        let n: &Node<'_> = {
            let alloced_name = self.builder.alloc_str(local_name);
            let el = self.builder.new_element(alloced_name);
            for a in &attrs {
                let aname  = self.builder.alloc_str(&a.name.local);
                let avalue = self.builder.alloc_str(&a.value);
                let attr   = self.builder.new_attribute(aname, avalue);
                self.builder.append_attribute(el, attr);
            }
            &*el
        };
        let handle = erase(n);
        self.qnames.borrow_mut().insert(handle, name);
        handle
    }

    fn alloc_text(&self, s: &str) -> ErasedHandle {
        let content = self.builder.alloc_str(s);
        let n: &Node<'_> = self.builder.new_text(content);
        erase(n)
    }

    fn alloc_comment(&self, s: &str) -> ErasedHandle {
        let content = self.builder.alloc_str(s);
        let n: &Node<'_> = self.builder.new_comment(content);
        erase(n)
    }

    /// Compute depth of `node` by walking parent pointers.  Bounded by the
    /// depth limit itself.
    ///
    /// # Safety
    ///
    /// `handle` must be a live arena pointer (not a sentinel).
    unsafe fn depth_of(&self, handle: ErasedHandle) -> u32 {
        if is_sentinel(handle) {
            return 0;
        }
        let mut d = 0u32;
        let mut cur = unsafe { unerase(handle) };
        while let Some(p) = cur.parent.get() {
            d += 1;
            cur = p;
        }
        d
    }

    /// Detach a node from its current parent (if any).  Works for nodes
    /// living at the document-children list too.
    fn detach_from_anywhere(&self, handle: ErasedHandle) {
        if is_sentinel(handle) { return; }
        // SAFETY: handle is a real arena pointer; arena is alive.
        let n = unsafe { unerase(handle) };
        if n.parent.get().is_some() {
            self.builder.detach(n);
            return;
        }
        // Maybe in doc_children.
        self.doc_children.borrow_mut().retain(|&p| p != handle);
    }

    /// Append `child` as the last child of `parent` (which may be the
    /// document sentinel).
    fn link_append(&self, parent: ErasedHandle, child: ErasedHandle) {
        self.detach_from_anywhere(child);
        if is_document(parent) {
            self.doc_children.borrow_mut().push(child);
            return;
        }
        // SAFETY: parent is a real arena element; child is a real arena node.
        let p = unsafe { unerase(parent) };
        let c = unsafe { unerase(child) };
        self.builder.append_child(p, c);
    }

    /// Insert `new_child` immediately before `sibling` in `sibling.parent`.
    /// `sibling` must have a parent (asserted by html5ever).
    fn link_before(&self, sibling: ErasedHandle, new_child: ErasedHandle) {
        debug_assert!(!is_sentinel(sibling));
        self.detach_from_anywhere(new_child);
        // SAFETY: sibling is a live arena node.
        let s = unsafe { unerase(sibling) };
        let parent = match s.parent.get() {
            Some(p) => p,
            None => return,
        };
        // SAFETY: new_child is a live arena node.
        let nc = unsafe { unerase(new_child) };
        // Splice into linked list.
        let prev = s.prev_sibling.get();
        nc.parent.set(Some(parent));
        nc.prev_sibling.set(prev);
        nc.next_sibling.set(Some(s));
        s.prev_sibling.set(Some(nc));
        match prev {
            Some(p) => p.next_sibling.set(Some(nc)),
            None    => parent.first_child.set(Some(nc)),
        }
    }

    /// Last child of `parent` (document sentinel or real element).
    fn last_child(&self, parent: ErasedHandle) -> Option<ErasedHandle> {
        if is_document(parent) {
            return self.doc_children.borrow().last().copied();
        }
        // SAFETY: parent is a real arena element.
        let p = unsafe { unerase(parent) };
        p.last_child.get().map(erase)
    }

    /// Commit any in-progress [`PendingText`] run to the arena.
    ///
    /// Idempotent — clears `pending_text` after writing.  Must be called
    /// before any consumer observes the document (i.e. before
    /// [`finalize_arena`] returns), and before any merge path needs to
    /// read the *committed* content of a different text node than the
    /// current pending target.
    fn flush_pending_text(&self) {
        let pending = self.pending_text.borrow_mut().take();
        if let Some(p) = pending {
            let content = self.builder.alloc_str(&p.buf);
            // SAFETY: `handle` was recorded by append_text / insert_text_before,
            // both of which only stash live arena text nodes.  The arena is
            // still alive (we're inside the sink).
            let n = unsafe { unerase(p.handle) };
            n.set_content(&self.builder, content);
        }
    }

    /// Append text to `parent`, merging with a trailing text child where
    /// possible.  Enforces the byte budget and depth limit.
    ///
    /// Fast path: when several consecutive `append_text` calls target
    /// the same text node (the common case for `<script>`/`<style>`
    /// bodies and long runs of `Text` tokens), the chunks accumulate
    /// into a [`PendingText`] buffer.  Without this we'd copy the
    /// growing string into the arena on every chunk — O(N²) work and
    /// O(N²) bumpalo footprint.  See [`flush_pending_text`].
    fn append_text(&self, parent: ErasedHandle, text: &str) {
        let added = text.len() as u64;
        // Try merge with previous-text-target first.
        let merge_target = self.last_child(parent).filter(|&h| {
            if is_sentinel(h) { return false; }
            // SAFETY: real arena node.
            unsafe { unerase(h) }.is_text()
        });
        if let Some(tgt) = merge_target {
            if self.text_bytes.get().saturating_add(added) > self.opts.max_text_bytes {
                self.abort(format!(
                    "max_text_bytes ({}) exceeded during text merge",
                    self.opts.max_text_bytes
                ));
                return;
            }
            self.text_bytes.set(self.text_bytes.get() + added);

            // Hot path: same target as the in-progress run — just grow
            // the in-memory buffer.  Amortized linear via `String`'s
            // doubling growth.
            {
                let mut slot = self.pending_text.borrow_mut();
                if let Some(p) = slot.as_mut() {
                    if p.handle == tgt {
                        p.buf.push_str(text);
                        return;
                    }
                }
            }
            // Cold path: different target than any in-progress run.
            // Flush the prior run (which writes its accumulated buffer
            // to a *different* node), then seed a new pending entry
            // from the current committed content of `tgt`.
            self.flush_pending_text();
            // SAFETY: real text node — filter above guarantees it.
            let n = unsafe { unerase(tgt) };
            let cur = n.content();
            let mut buf = String::with_capacity(cur.len() + text.len());
            buf.push_str(cur);
            buf.push_str(text);
            *self.pending_text.borrow_mut() = Some(PendingText { handle: tgt, buf });
            return;
        }
        if self.text_bytes.get().saturating_add(added) > self.opts.max_text_bytes {
            self.abort(format!(
                "max_text_bytes ({}) exceeded",
                self.opts.max_text_bytes
            ));
            return;
        }
        self.text_bytes.set(self.text_bytes.get() + added);

        // SAFETY: parent is either the document sentinel or a real element.
        let new_depth = unsafe { self.depth_of(parent) } + 1;
        if new_depth > self.opts.max_element_depth {
            self.abort(format!(
                "max_element_depth ({}) exceeded",
                self.opts.max_element_depth
            ));
            return;
        }
        // No merge target — flush any prior run (different parent),
        // then commit this chunk directly to the arena.  Pending is
        // *not* started yet: the common case is a single-chunk text
        // node (button labels, attribute-poor inline text), where the
        // empty-alloc-then-flush pattern would just double the
        // allocator work.  If a second chunk arrives targeting this
        // node, the merge branch above will switch us into pending
        // mode then.
        self.flush_pending_text();
        let t = self.alloc_text(text);
        self.link_append(parent, t);
    }

    /// Like [`append_text`] but inserts before `sibling` (its parent is
    /// the effective anchor).  Mirrors [`append_before_sibling`].
    ///
    /// Uses the same [`PendingText`] fast path as [`append_text`] when
    /// the merge target is the in-progress text node.
    fn insert_text_before(&self, sibling: ErasedHandle, text: &str) {
        debug_assert!(!is_sentinel(sibling));
        // SAFETY: real arena node.
        let s = unsafe { unerase(sibling) };
        let parent = match s.parent.get() {
            Some(p) => p,
            None => return,
        };
        let added = text.len() as u64;
        // If the previous sibling is text, merge into it.
        let prev = s.prev_sibling.get();
        if let Some(prev_node) = prev {
            if prev_node.is_text() {
                if self.text_bytes.get().saturating_add(added) > self.opts.max_text_bytes {
                    self.abort(format!(
                        "max_text_bytes ({}) exceeded during text merge",
                        self.opts.max_text_bytes
                    ));
                    return;
                }
                self.text_bytes.set(self.text_bytes.get() + added);

                let tgt = erase(prev_node);
                // Hot path: same target as in-progress run.
                {
                    let mut slot = self.pending_text.borrow_mut();
                    if let Some(p) = slot.as_mut() {
                        if p.handle == tgt {
                            p.buf.push_str(text);
                            return;
                        }
                    }
                }
                // Cold path: flush prior run, seed new pending from
                // committed content of `prev_node`.
                self.flush_pending_text();
                let cur = prev_node.content();
                let mut buf = String::with_capacity(cur.len() + text.len());
                buf.push_str(cur);
                buf.push_str(text);
                *self.pending_text.borrow_mut() = Some(PendingText { handle: tgt, buf });
                return;
            }
        }
        if self.text_bytes.get().saturating_add(added) > self.opts.max_text_bytes {
            self.abort(format!(
                "max_text_bytes ({}) exceeded",
                self.opts.max_text_bytes
            ));
            return;
        }
        self.text_bytes.set(self.text_bytes.get() + added);

        // Depth check based on parent.
        let new_depth = unsafe { self.depth_of(erase(parent)) } + 1;
        if new_depth > self.opts.max_element_depth {
            self.abort(format!(
                "max_element_depth ({}) exceeded",
                self.opts.max_element_depth
            ));
            return;
        }
        // No mergeable prev sibling — flush prior run, commit this
        // chunk directly to the arena.  Pending only kicks in if a
        // subsequent chunk targets this same node (see [`append_text`]
        // for the rationale).
        self.flush_pending_text();
        let t = self.alloc_text(text);
        self.link_before(sibling, t);
    }
}

// ── conversion ──────────────────────────────────────────────────────────────

fn convert_quirks(q: H5QuirksMode) -> TreeQuirksMode {
    match q {
        H5QuirksMode::NoQuirks      => TreeQuirksMode::NoQuirks,
        H5QuirksMode::LimitedQuirks => TreeQuirksMode::LimitedQuirks,
        H5QuirksMode::Quirks        => TreeQuirksMode::Quirks,
    }
}

// ── TreeSink impl ───────────────────────────────────────────────────────────

impl TreeSink for BatchSinkArena {
    type Handle  = ErasedHandle;
    type Output  = Self;
    type ElemName<'a>
        = OwnedElemName
    where
        Self: 'a;

    fn finish(self) -> Self { self }

    fn parse_error(&self, msg: Cow<'static, str>) {
        // In lenient mode, prolog-phase errors (most notably html5ever's
        // missing-DOCTYPE error, and its "DOCTYPE name is not 'html'"
        // error) predate the root element and are not well-formedness
        // violations to libxml2's recovering HTML parser — drop them so
        // `recover=False` consumers don't raise on otherwise-clean input.
        // Strict mode keeps them: it exists precisely to surface such
        // prolog/DOCTYPE defects.
        if self.opts.recovery_mode && !self.tree_started.get() {
            return;
        }
        let level = if self.opts.recovery_mode {
            ErrorLevel::Error
        } else if self.fatal_error.borrow().is_none() {
            ErrorLevel::Fatal
        } else {
            ErrorLevel::Error
        };
        self.record_error(msg.into_owned(), level);
    }

    fn get_document(&self) -> ErasedHandle { DOCUMENT_SENTINEL }

    fn elem_name<'a>(&'a self, target: &'a ErasedHandle) -> OwnedElemName {
        let qnames = self.qnames.borrow();
        let q = qnames.get(target).expect("elem_name on non-element handle");
        OwnedElemName { inner: q.clone() }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<H5Attribute>,
        _flags: ElementFlags,
    ) -> ErasedHandle {
        if self.aborted.get() {
            // Still return something — html5ever will keep calling into us.
            // Allocate the element so the handle is well-formed, but the
            // appends will no-op on the abort flag.
        }
        self.tree_started.set(true);
        self.alloc_element(name, attrs)
    }

    fn create_comment(&self, text: StrTendril) -> ErasedHandle {
        self.alloc_comment(&text)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> ErasedHandle {
        // HTML5 produces PIs only as bogus comments.  Match the legacy sink.
        self.alloc_comment("")
    }

    fn append(&self, parent: &ErasedHandle, child: NodeOrText<ErasedHandle>) {
        if self.aborted.get() { return; }
        let parent = *parent;
        match child {
            NodeOrText::AppendText(t) => self.append_text(parent, &t),
            NodeOrText::AppendNode(n) => {
                // Depth check.
                let new_depth = unsafe { self.depth_of(parent) } + 1;
                if new_depth > self.opts.max_element_depth {
                    self.abort(format!(
                        "max_element_depth ({}) exceeded",
                        self.opts.max_element_depth
                    ));
                    return;
                }
                self.link_append(parent, n);
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &ErasedHandle,
        prev_element: &ErasedHandle,
        child: NodeOrText<ErasedHandle>,
    ) {
        if self.aborted.get() { return; }
        let element = *element;
        let element_has_parent = !is_sentinel(element) && {
            // SAFETY: real arena node when not sentinel.
            unsafe { unerase(element) }.parent.get().is_some()
        };
        if element_has_parent {
            self.append_before_sibling(&element, child);
        } else {
            self.append(prev_element, child);
        }
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        *self.doctype.borrow_mut() = Some(HtmlDoctype {
            name:      name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
        // Doctype handle isn't actually used by html5ever for tree
        // construction; we don't record it in doc_children since it
        // doesn't appear in the final tree (it lives in html_metadata).
    }

    fn get_template_contents(&self, target: &ErasedHandle) -> ErasedHandle {
        // v1 doesn't model template-contents isolation — return the
        // template element itself.  See sink.rs for the rationale.
        *target
    }

    fn same_node(&self, x: &ErasedHandle, y: &ErasedHandle) -> bool { x == y }

    fn set_quirks_mode(&self, mode: H5QuirksMode) {
        self.quirks_mode.set(mode);
    }

    fn append_before_sibling(
        &self,
        sibling: &ErasedHandle,
        new_node: NodeOrText<ErasedHandle>,
    ) {
        if self.aborted.get() { return; }
        let sibling = *sibling;
        if is_sentinel(sibling) { return; }
        match new_node {
            NodeOrText::AppendText(t) => self.insert_text_before(sibling, &t),
            NodeOrText::AppendNode(n) => {
                // Depth check based on sibling's parent.
                // SAFETY: real arena node when not sentinel.
                let parent = match unsafe { unerase(sibling) }.parent.get() {
                    Some(p) => p,
                    None => return,
                };
                let new_depth = unsafe { self.depth_of(erase(parent)) } + 1;
                if new_depth > self.opts.max_element_depth {
                    self.abort(format!(
                        "max_element_depth ({}) exceeded",
                        self.opts.max_element_depth
                    ));
                    return;
                }
                self.link_before(sibling, n);
            }
        }
    }

    fn add_attrs_if_missing(&self, target: &ErasedHandle, attrs: Vec<H5Attribute>) {
        if self.aborted.get() { return; }
        if is_sentinel(*target) { return; }
        // SAFETY: real arena element.
        let el = unsafe { unerase(*target) };
        if !el.is_element() { return; }
        for a in attrs {
            // Walk the existing list to check for duplicates by local name.
            let local: &str = &a.name.local;
            let mut cur = el.first_attribute.get();
            let mut have = false;
            while let Some(attr) = cur {
                if attr.name() == local { have = true; break; }
                cur = attr.next.get();
            }
            if !have {
                let aname  = self.builder.alloc_str(local);
                let avalue = self.builder.alloc_str(&a.value);
                let attr   = self.builder.new_attribute(aname, avalue);
                self.builder.append_attribute(el, attr);
            }
        }
    }

    fn remove_from_parent(&self, target: &ErasedHandle) {
        let target = *target;
        if is_sentinel(target) { return; }
        // SAFETY: real arena node.
        let n = unsafe { unerase(target) };
        if n.parent.get().is_some() {
            self.builder.detach(n);
            return;
        }
        // Maybe in doc_children.
        self.doc_children.borrow_mut().retain(|&h| h != target);
    }

    fn reparent_children(&self, node: &ErasedHandle, new_parent: &ErasedHandle) {
        let node = *node;
        let new_parent = *new_parent;
        if is_sentinel(node) {
            // Move doc_children into new_parent.
            let moved: Vec<ErasedHandle> =
                std::mem::take(&mut *self.doc_children.borrow_mut());
            for c in moved {
                self.link_append(new_parent, c);
            }
            return;
        }
        // SAFETY: real arena node.
        let src = unsafe { unerase(node) };
        // Collect children first to avoid mutating-while-iterating issues.
        let mut moved: Vec<ErasedHandle> = Vec::new();
        let mut cur = src.first_child.get();
        while let Some(c) = cur {
            moved.push(erase(c));
            cur = c.next_sibling.get();
        }
        for c in moved {
            self.link_append(new_parent, c);
        }
    }
}

// Doctype sentinel kept as a const but used nowhere as a real handle; allow
// the harmless dead-code lint.
#[allow(dead_code)]
const _DOCTYPE_SENTINEL_REF: ErasedHandle = DOCTYPE_SENTINEL;

// ── finalize ────────────────────────────────────────────────────────────────

/// Walk the document children, pick the first element as the arena root,
/// stash HTML metadata, and build the Document.  Discards any non-element
/// document-level children (comment-before-html / comment-after-html) — a
/// rare corner case in real HTML and dropped by both this sink and the
/// legacy one.
///
/// Returns `(Document, recovered_errors, fatal_error)`.  When no `<html>`
/// element was produced (impossible from html5ever in normal use), we
/// synthesize an empty `<html>` element so the arena Document is buildable
/// — its `set_root` is non-optional.
pub(super) fn finalize_arena(
    sink: BatchSinkArena,
) -> (Document, Vec<XmlError>, Option<XmlError>) {
    // Commit any in-progress text run to the arena.  After this point
    // every text node carries its full content — no consumer ever sees
    // an unflushed `PendingText` buffer.
    sink.flush_pending_text();

    let fatal = sink.fatal_error.borrow().clone();
    let errors = std::mem::take(&mut *sink.parse_errors.borrow_mut());

    // Capture HTML metadata before consuming the sink.
    let meta = HtmlMeta {
        quirks_mode: convert_quirks(sink.quirks_mode.get()),
        doctype:     sink.doctype.borrow_mut().take(),
    };
    sink.builder.set_html_metadata(Some(meta));

    // Pick the root: first element child of the document.
    let doc_children = sink.doc_children.borrow();
    let root_handle = doc_children.iter().copied().find(|&h| {
        if is_sentinel(h) { return false; }
        // SAFETY: doc_children only holds real arena pointers; arena alive.
        unsafe { unerase(h) }.is_element()
    });

    let root_ptr = match root_handle {
        Some(h) => h,
        None => {
            // No <html> element — synthesize one to keep the builder happy.
            let synth_name = sink.builder.alloc_str("html");
            let synth: &Node<'_> = sink.builder.new_element(synth_name);
            erase(synth)
        }
    };
    drop(doc_children);

    // SAFETY: root_ptr was erased from a node allocated in `sink.builder`,
    // which we're about to consume into the resulting Document.
    let root_ref: &Node<'_> = unsafe { unerase(root_ptr) };
    sink.builder.set_root(root_ref);
    let doc = sink.builder.build();
    (doc, errors, fatal)
}
