//! Tier 4 — tree mutation.  The most-used libxml2 build-pattern
//! functions: `xmlNewDocNode`, `xmlNewDocText`, `xmlSetProp`,
//! `xmlAddChild`, `xmlUnlinkNode`, `xmlFreeNode`.  lxml's
//! `ET.Element()` / `SubElement()` / `set()` / `__setitem__` all
//! land here.
//!
//! # Arena semantics
//!
//! Our tree lives in a per-document bumpalo arena.  Bump arenas can
//! GROW any time, so post-parse allocation is fine.  But they CAN'T
//! reclaim individual allocations — `xmlFreeNode` on a detached node
//! is a logical no-op (the bytes stay in the arena until the whole
//! document is freed by `xmlFreeDoc`).  This matches libxml2's
//! `xmlMemoryDump` behavior closely enough that real consumers don't
//! notice — leaked-but-detached nodes are usually transient.
//!
//! # Cross-document operations
//!
//! `xmlAddChild` across two different documents would require deep-
//! copying the child into the parent's arena.  Not implemented yet —
//! same-document adoption only.  Cross-doc adoption emits a clean
//! error.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::error::{ErrorCode, ErrorDomain, ErrorLevel, XmlError};
use sup_xml_tree::dom::{Attribute, Namespace, Node, NodeKind, XmlDoc};

use crate::error::record_last_error;

// ── helpers ────────────────────────────────────────────────────────────────

pub(crate) unsafe fn doc_ref<'a>(doc: *mut XmlDoc) -> Option<&'a sup_xml_tree::dom::Document> {
    if doc.is_null() {
        return None;
    }
    // SAFETY: caller asserts doc came from xmlReadMemory.
    Some(unsafe { &(*doc)._doc })
}

unsafe fn cstr_to_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Cast a `*mut Node` to its arena reference.  Erases lifetime.
unsafe fn node_ref<'a>(n: *mut Node<'static>) -> Option<&'a Node<'a>> {
    if n.is_null() {
        return None;
    }
    // SAFETY: caller asserts n is a live arena pointer.
    Some(unsafe { &*(n as *const Node<'a>) })
}

unsafe fn attr_ref<'a>(a: *mut Attribute<'static>) -> Option<&'a Attribute<'a>> {
    if a.is_null() {
        return None;
    }
    unsafe { Some(&*(a as *const Attribute<'a>)) }
}

#[inline]
fn local_name(s: &str) -> &str {
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None    => s,
    }
}

/// Stamp the owning-doc pointer on a single freshly allocated node.
///
/// `Document::new_*` initialises the c-abi `doc` slot to NULL — only
/// the parser's `into_xml_doc` walk fills it in.  Post-parse
/// mutations (lxml's `_createTextNode` reads `child->doc` and feeds
/// it back into `xmlNewDocText`) need the pointer right away, so
/// each mutate-helper calls this before handing out the pointer.
#[inline]
unsafe fn stamp_doc_one(n: &Node<'_>, doc: *mut XmlDoc) {
    n.doc.set(doc as *mut std::os::raw::c_void);
}

// ── cross-doc graft handling ─────────────────────────────────────────────
//
// libxml2 consumers (lxml's `moveNodeToDocument`, `_appendChild`)
// move nodes between documents by relinking sibling pointers — the
// node memory is not copied, only the pointer moves.  Each document
// owns its arena (see `dict.rs::new_doc_arena`), and the moved node
// stays physically in its origin arena.
//
// A graft *within one thread* is safe by construction: every arena is
// held in a per-thread keep-alive until the thread exits, so the origin
// arena outlives every same-thread destination document.
//
// Across threads the origin thread may exit (freeing its arenas) while
// the destination still references the moved node.  `retain_graft_arena`
// prevents the dangle by pinning the origin arena onto the destination
// dict; it must run while `child.doc` still points at the origin
// (before the wiring below retags it).  `note_cross_doc_donation` then
// performs the retag.  lxml's `_appendChild` bypasses these API
// primitives entirely; its cross-thread moves are covered by the
// `xmlDictLookup` re-intern hook instead (see `dict.rs`).

/// Pin `child`'s origin arena onto `dst_doc`'s dict when a graft
/// crosses threads, so the moved node's memory outlives a drop of its
/// origin document.  Retention lives on the destination *dict*
/// (`Mutex`-guarded, shared across the thread), so it is safe under the
/// concurrent access lxml's threaded moves produce.
///
/// A same-thread graft shares the destination dict; the per-thread
/// keep-alive (see `dict::new_doc_arena`) already holds the origin
/// arena until thread exit, so only cross-thread grafts (distinct
/// dicts) retain.  No-op for NULL docs or when either side carries no
/// dict.  Used by the API graft primitives; lxml's `_appendChild`
/// bypasses them and is covered by the `xmlDictLookup` re-intern hook.
unsafe fn retain_graft_arena(child: &Node<'_>, dst_doc: *mut XmlDoc) {
    let src_doc = child.doc.get() as *mut XmlDoc;
    if src_doc.is_null() || dst_doc.is_null() || src_doc == dst_doc {
        return;
    }
    let (Some(src), Some(dst)) =
        (unsafe { doc_ref(src_doc) }, unsafe { doc_ref(dst_doc) })
    else {
        return;
    };
    let (src_dict, dst_dict) = (src.dict_ptr(), dst.dict_ptr());
    if src_dict.is_null() || dst_dict.is_null() || src_dict == dst_dict {
        return;
    }
    // SAFETY: dst_dict is live (its document is borrowed above).
    unsafe { (*dst_dict).retain_arena(src.bump_arc()); }
}

/// Retag `n.doc` to the destination document after a cross-doc graft
/// so consumers walking `c_node->doc` see the new home.  Arena
/// lifetime is handled separately by [`retain_graft_arena`], which must
/// already have run while `n.doc` still pointed at the origin.
fn note_cross_doc_donation(n: &Node<'_>, parent: &Node<'_>) {
    let src_doc = n.doc.get() as *mut XmlDoc;
    let dst_doc = parent.doc.get() as *mut XmlDoc;
    if dst_doc.is_null() || src_doc == dst_doc {
        return;
    }
    n.doc.set(dst_doc as *mut std::os::raw::c_void);
}

// ── document creation ─────────────────────────────────────────────────────

/// libxml2 `xmlNewDoc(version)` — allocate an empty XML document.
/// `version` is the XML declaration version (default "1.0" if NULL).
///
/// The returned doc has `children = NULL`; populate it by linking
/// elements via [`xmlDocSetRootElement`] or `xmlAddChild`.  Caller
/// releases via [`crate::parse::xmlFreeDoc`].
///
/// # Implementation detail
///
/// Our `Document` type is built by `DocumentBuilder` and expects a
/// root node.  For "empty" docs we allocate a placeholder element
/// (name=""), build the document around it, then null out the
/// `children` slot on the resulting `XmlDoc`.  The placeholder lives
/// in the arena unreferenced — it's reclaimed wholesale on
/// `xmlFreeDoc`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDoc(version: *const c_char) -> *mut XmlDoc {
    use sup_xml_tree::dom::DocumentBuilder;
    let ver = unsafe { cstr_to_str(version) }.unwrap_or("1.0");
    // Adopt the thread-local shared dict and a fresh per-document
    // arena.  The arena is registered in a per-thread keep-alive so a
    // node grafted into another same-thread document survives this
    // document's drop; names intern into the shared dict so they stay
    // pointer-equal across documents.  See `dict.rs::new_doc_arena`.
    let dict  = crate::dict::thread_dict();
    let arena = crate::dict::new_doc_arena();
    // SAFETY: thread_dict returns a live, refcount-managed Dict.
    let b = unsafe { DocumentBuilder::new_with_dict_and_arena(dict, arena) };
    b.set_version(ver);
    let placeholder = b.new_element("");
    b.set_root(placeholder);
    let doc = b.build();
    let xml_doc = doc.into_xml_doc();
    // Reset children to NULL — libxml2's xmlNewDoc returns an empty
    // doc (no root element until xmlDocSetRootElement is called).
    // Also NULL the encoding pointer at offset 112 to match
    // libxml2's "doc->encoding is NULL until explicitly set"
    // contract; serializers omit the encoding attribute on output
    // in that case.
    // SAFETY: xml_doc is a freshly allocated boxed XmlDoc.
    unsafe {
        (*xml_doc).children.set(ptr::null_mut());
        (*xml_doc).last.set(ptr::null_mut());
        let enc_ptr = (xml_doc as *mut u8).add(112) as *mut *const c_char;
        let current = *enc_ptr;
        if !current.is_null() && *(current as *const u8) == 0 {
            *enc_ptr = ptr::null();
        }
    }
    xml_doc
}

/// libxml2 `xmlDocSetRootElement(doc, root)` — set or replace the
/// document's root element.  Returns the OLD root (NULL if there
/// wasn't one).  The new root's parent is set to NULL (libxml2's
/// convention — the doc itself is the conceptual parent).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDocSetRootElement(
    doc: *mut XmlDoc,
    root: *mut Node<'static>,
) -> *mut Node<'static> {
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts doc came from xmlReadMemory/xmlNewDoc.
    unsafe {
        let old = (*doc).children.get();
        // Walk past leading non-element prelude (comments/PIs) to
        // find the OLD root element, if any.  libxml2 only returns
        // the old root element, not the first sibling.
        let mut old_root: *mut Node<'static> = ptr::null_mut();
        let mut cur = old;
        while !cur.is_null() {
            if matches!((*cur).kind, NodeKind::Element) {
                old_root = cur;
                break;
            }
            cur = (*cur).next_sibling.get()
                .map(|s| s as *const Node<'_> as *mut Node<'static>)
                .unwrap_or(ptr::null_mut());
        }
        // Detach the old root from its sibling chain (if any).
        if !old_root.is_null() {
            (*old_root).parent.set(None);
            (*old_root).prev_sibling.set(None);
            (*old_root).next_sibling.set(None);
        }
        // Wire the new root.  Parent it on the document (cast as a Node —
        // `XmlDoc` and `Node` share layout at offsets 0–64), matching
        // libxml2 (`root->parent = (xmlNode*)doc`) and our parsed-doc
        // path (`into_xml_doc`).  Consumers test
        // `node->parent->type == XML_DOCUMENT_NODE` to recognise the
        // document level; without this a *created* document's root looked
        // detached, so e.g. lxml dropped a prolog/epilogue comment's tail
        // text incorrectly and mis-serialized document-level siblings.
        if !root.is_null() {
            let doc_as_node: &Node<'static> = &*(doc as *const Node<'static>);
            (*root).parent.set(Some(doc_as_node));
            (*root).prev_sibling.set(None);
            (*root).next_sibling.set(None);
            // Stamp doc on the new root and its subtree.
            stamp_doc(root, doc);
        }
        (*doc).children.set(root);
        (*doc).last.set(root);
        // Sync the embedded `Document`'s root pointer so the
        // serializer (which walks via `Document::root()`) sees the
        // new root instead of the original placeholder.
        // SAFETY: doc is non-null per earlier check; _doc is the
        // ManuallyDrop<Document> field accessed through *doc.
        let doc_inner = &raw mut (*doc)._doc;
        // SAFETY: ManuallyDrop is repr(transparent), so a *mut
        // ManuallyDrop<Document> is also a *mut Document.
        let doc_mut: *mut sup_xml_tree::dom::Document =
            doc_inner as *mut sup_xml_tree::dom::Document;
        (*doc_mut).set_root_ptr(root as *const _);
        old_root
    }
}

/// libxml2 `xmlReconciliateNs(doc, tree)` — walk the subtree rooted
/// at `tree` and reduce duplicate namespace declarations (replace
/// redundant inner `xmlns:p=...` with references to the matching
/// outer binding), as well as moving missing declarations up the
/// chain so prefixes resolve.
///
/// Real libxml2 mutates `ns_def` chains and rewrites element/attr
/// `ns` pointers; the goal is "after a graft, every prefix used in
/// the subtree resolves to a single canonical xmlNs allocation".
///
/// Our DOM stores namespace bindings on a different model
/// ([`sup_xml_tree::dom::Namespace`] arena allocations with chain
/// pointers), and reconciliation across a graft is not yet required
/// by any consumer we exercise.  We accept the call for ABI parity
/// and return 0 (success, no changes applied).  Returns -1 only when
/// `tree` is NULL — preserving libxml2's "invalid input" convention.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReconciliateNs(
    _doc:  *mut XmlDoc,
    tree:  *mut Node<'static>,
) -> c_int {
    if tree.is_null() { -1 } else { 0 }
}

// ── doc-level metadata ────────────────────────────────────────────────────

/// libxml2 `xmlGetDocCompressMode(doc)` — read the gzip output level
/// (0–9, or -1 when no doc).  Our serializer never actually emits
/// gzip, but we preserve the field so consumers that round-trip
/// `xmlSetDocCompressMode` / `xmlGetDocCompressMode` see consistent
/// values.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDocCompressMode(doc: *const XmlDoc) -> c_int {
    if doc.is_null() { return -1; }
    // SAFETY: caller asserts doc is a valid xmlDoc allocation.
    unsafe { (*doc).compression as c_int }
}

/// libxml2 `xmlSetDocCompressMode(doc, mode)` — set the gzip output
/// level.  Clamped to 0..=9 to match libxml2's documented contract.
/// No-op when `doc` is NULL.  See [`xmlGetDocCompressMode`] for the
/// "we don't actually gzip" caveat.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetDocCompressMode(doc: *mut XmlDoc, mode: c_int) {
    if doc.is_null() { return; }
    let clamped = mode.clamp(0, 9);
    // SAFETY: caller asserts doc is uniquely owned for the call.
    unsafe { (*doc).compression = clamped; }
}

/// libxml2 `xmlSetTreeDoc(tree, doc)` — walk the subtree rooted at
/// `tree` and stamp `node->doc = doc` on every element, attribute,
/// and descendant.  Used by consumers (libxslt, lxml's
/// `_setNodeNamespaces`) to fix up `doc` pointers after grafting a
/// subtree allocated for one document into another.
///
/// Safe to call on a NULL tree (no-op).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetTreeDoc(
    tree: *mut Node<'static>,
    doc:  *mut XmlDoc,
) {
    unsafe { stamp_doc(tree, doc); }
}

/// libxml2 `xmlSetListDoc(list, doc)` — same as [`xmlSetTreeDoc`] but
/// also walks the sibling chain rooted at `list`, not just its
/// descendants.  Used when grafting a list of disconnected nodes
/// (e.g. a `DocumentFragment`'s children) into a new document.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetListDoc(
    list: *mut Node<'static>,
    doc:  *mut XmlDoc,
) {
    let mut cur = list;
    while !cur.is_null() {
        // SAFETY: cur is a non-null arena node pointer for the iteration.
        let next = unsafe { (*cur).next_sibling.get() };
        unsafe { stamp_doc(cur, doc); }
        cur = next.map(|n| n as *const _ as *mut _).unwrap_or(ptr::null_mut());
    }
}

/// Walk a subtree and stamp `node->doc = doc` on every node + attr.
/// Used by `xmlDocSetRootElement` and `xmlAddChild` cross-doc paths.
unsafe fn stamp_doc(node: *mut Node<'static>, doc: *mut XmlDoc) {
    if node.is_null() { return; }
    let doc_void = doc as *mut std::os::raw::c_void;
    let mut stack: Vec<*mut Node<'static>> = vec![node];
    while let Some(np) = stack.pop() {
        if np.is_null() { continue; }
        // SAFETY: np is a non-null arena pointer.
        unsafe {
            let n = &*np;
            n.doc.set(doc_void);
            // Attrs.
            let mut a = n.first_attribute.get();
            while let Some(attr) = a {
                attr.doc.set(doc_void);
                a = attr.next.get();
            }
            // Children.
            let mut c = n.first_child.get();
            while let Some(ch) = c {
                stack.push(ch as *const _ as *mut _);
                c = ch.next_sibling.get();
            }
        }
    }
}

// ── element creation ──────────────────────────────────────────────────────

/// libxml2 `xmlNewDocNode(doc, ns, name, content)` — create an
/// element node in `doc`'s arena.  `ns` is ignored in v0.1 (the
/// namespace machinery needs `xmlSetNs` post-creation).  `content`,
/// if non-NULL, becomes a single text child.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocNode(
    doc:     *mut XmlDoc,
    _ns:     *mut sup_xml_tree::dom::Namespace<'static>,
    name:    *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    let name_s = match unsafe { cstr_to_str(name) } {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    // Allocate the element in doc's arena.
    let el: &Node<'_> = d.new_element(name_s);
    unsafe { stamp_doc_one(el, doc); }
    // If content was supplied, create a text child and link it.
    if !content.is_null() {
        if let Some(c) = unsafe { cstr_to_str(content) } {
            let txt: &Node<'_> = d.new_text(c);
            unsafe { stamp_doc_one(txt, doc); }
            d.append_child(el, txt);
        }
    }
    el as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewDocText(doc, content)` — text node.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocText(
    doc: *mut XmlDoc,
    content: *const c_char,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    let content_s = unsafe { cstr_to_str(content) }.unwrap_or("");
    let n: &Node<'_> = d.new_text(content_s);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewDocTextLen(doc, content, len)` — text node with
/// explicit byte length (binary-safe).  Stops at the first NUL within
/// the first `len` bytes.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocTextLen(
    doc:     *mut XmlDoc,
    content: *const c_char,
    len:     c_int,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    if content.is_null() || len < 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `content` is readable for `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(content as *const u8, len as usize) };
    let stop = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = match std::str::from_utf8(&bytes[..stop]) {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    let stored: &str = d.bump().alloc_str(s);
    let n: &Node<'_> = d.new_text(stored);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewDocComment(doc, content)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocComment(
    doc: *mut XmlDoc,
    content: *const c_char,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    let content_s = unsafe { cstr_to_str(content) }.unwrap_or("");
    let n: &Node<'_> = d.new_comment(content_s);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewDocPI(doc, name, content)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocPI(
    doc: *mut XmlDoc,
    name: *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    // A NULL `content` pointer means "no data section" (serializes as
    // `<?name?>`); a non-NULL pointer — even to "" — gives `<?name ?>`.
    let content_s = unsafe { cstr_to_str(content) };
    let n: &Node<'_> = d.new_pi(name_s, content_s);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewNode(ns, name)` — create an element without an
/// owning document.  libxml2 `xmlMalloc`s a free-floating node here;
/// our arena model can't represent "no doc," so we allocate into the
/// thread-local scratch doc (the same one already used for detached
/// text/comment nodes).  The scratch doc lives until the thread exits,
/// so the node's bytes survive being grafted into a real doc via
/// `xmlAddChild`.
///
/// `ns` is accepted for API parity but ignored in this build — call
/// `xmlSetNs` post-creation if you need a namespace attached.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewNode(
    _ns:  *mut sup_xml_tree::dom::Namespace<'static>,
    name: *const c_char,
) -> *mut Node<'static> {
    if name.is_null() {
        return ptr::null_mut();
    }
    let doc = ensure_scratch_doc();
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: ensure_scratch_doc returned non-NULL → valid XmlDoc.
    // Delegate to xmlNewDocNode with NULL content for the actual
    // allocation + naming work.
    unsafe { xmlNewDocNode(doc, ptr::null_mut(), name, ptr::null()) }
}

/// libxml2 `xmlNewDocFragment(doc)` — create an empty document-
/// fragment node.  A fragment is a transparent container that holds
/// an ordered child list; grafting it into a real tree via
/// `xmlAddChild` is equivalent to splicing its children in place.
///
/// If `doc` is non-NULL, the fragment is allocated in that doc's
/// arena.  If `doc` is NULL, we use the thread-local scratch doc
/// (matching libxml2's "free-floating fragment" idiom).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocFragment(
    doc: *mut XmlDoc,
) -> *mut Node<'static> {
    let owning = if doc.is_null() { ensure_scratch_doc() } else { doc };
    let d = match unsafe { doc_ref(owning) } {
        Some(d) => d,
        None    => return ptr::null_mut(),
    };
    let n: &Node<'_> = d.new_fragment();
    unsafe { stamp_doc_one(n, owning); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewPI(name, content)` — create a processing-instruction
/// node without an owning document.  Same scratch-doc trick as
/// [`xmlNewNode`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewPI(
    name:    *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    if name.is_null() {
        return ptr::null_mut();
    }
    let doc = ensure_scratch_doc();
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: scratch doc valid.
    unsafe { xmlNewDocPI(doc, name, content) }
}

/// libxml2 `xmlNewCDataBlock(doc, content, len)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewCDataBlock(
    doc: *mut XmlDoc,
    content: *const c_char,
    len: std::os::raw::c_int,
) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } {
        Some(d) => d,
        None => return ptr::null_mut(),
    };
    let bytes: &[u8] = if content.is_null() || len <= 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(content as *const u8, len as usize) }
    };
    let s = std::str::from_utf8(bytes).unwrap_or("");
    let n: &Node<'_> = d.new_cdata(s);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewChild(parent, ns, name, content)` — create an
/// element child of `parent` (in the parent's owning doc) and append
/// it.  When `content` is non-NULL, an entity-decoded text child is
/// added.  Returns the new child; NULL on bad inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewChild(
    parent:  *mut Node<'static>,
    ns:      *mut sup_xml_tree::dom::Namespace<'static>,
    name:    *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    let p = match unsafe { node_ref(parent) } { Some(n) => n, None => return ptr::null_mut() };
    let doc_ptr = p.doc.get() as *mut XmlDoc;
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    let el: &Node<'_> = d.new_element(name_s);
    unsafe { stamp_doc_one(el, doc_ptr); }
    if !ns.is_null() {
        // SAFETY: `ns` is a non-null arena pointer per caller contract
        // (xmlNewNs / parser).
        let ns_ref = unsafe { &*(ns as *const sup_xml_tree::dom::Namespace<'_>) };
        el.namespace.set(Some(ns_ref));
    }
    if !content.is_null() {
        if let Some(c) = unsafe { cstr_to_str(content) } {
            let txt: &Node<'_> = d.new_text(c);
            unsafe { stamp_doc_one(txt, doc_ptr); }
            d.append_child(el, txt);
        }
    }
    d.append_child(p, el);
    el as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewTextChild(parent, ns, name, content)` — same as
/// [`xmlNewChild`] but `content` is treated as raw text (no entity
/// decoding).  In our implementation `xmlNewChild` already doesn't
/// re-decode entities — the `content` strings are taken verbatim —
/// so this is a synonym.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewTextChild(
    parent:  *mut Node<'static>,
    ns:      *mut sup_xml_tree::dom::Namespace<'static>,
    name:    *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    unsafe { xmlNewChild(parent, ns, name, content) }
}

/// libxml2 `xmlNewText(content)` — create a text node *not yet
/// attached to any document*.  Consumer must `xmlAddChild` it into
/// a doc, at which point the doc pointer gets stamped.  Allocated in
/// the thread-local scratch doc's arena, which lives until the thread
/// exits, so it remains valid until grafted.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewText(content: *const c_char) -> *mut Node<'static> {
    // Use the dedicated detached-node arena.  Same approach as
    // xmlNewComment / xmlNewPI below.
    let content_s = unsafe { cstr_to_str(content) }.unwrap_or("");
    detached_new_text(content_s)
}

/// libxml2 `xmlNewTextLen(content, len)` — like [`xmlNewText`] but
/// `content` is `len` bytes (not NUL-terminated).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewTextLen(
    content: *const c_char,
    len: c_int,
) -> *mut Node<'static> {
    if content.is_null() || len < 0 {
        return detached_new_text("");
    }
    // SAFETY: caller asserts `content` is readable for `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(content as *const u8, len as usize) };
    let s = std::str::from_utf8(bytes).unwrap_or("");
    detached_new_text(s)
}

/// Allocate an entity-reference node (`&name;`) in `doc`'s arena.
/// Backs `xmlNewReference`.  `name` is the bare entity name (a leading
/// `&` / trailing `;` are tolerated and stripped); the node carries the
/// name for `node->name` and the literal `&name;` as content so the
/// serializer round-trips it.
pub(crate) unsafe fn new_doc_entity_ref(doc: *mut XmlDoc, name: &str) -> *mut Node<'static> {
    let d = match unsafe { doc_ref(doc) } { Some(d) => d, None => return ptr::null_mut() };
    let bare = name.trim_start_matches('&').trim_end_matches(';');
    let name_s = d.bump().alloc_str(bare);
    let content_s = d.bump().alloc_str(&format!("&{bare};"));
    let n: &Node<'_> = d.new_entity_ref(name_s, content_s);
    unsafe { stamp_doc_one(n, doc); }
    n as *const Node<'_> as *mut Node<'static>
}

/// libxml2 `xmlNewComment(content)` — create a detached comment
/// node.  Same lifecycle as [`xmlNewText`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewComment(content: *const c_char) -> *mut Node<'static> {
    let s = unsafe { cstr_to_str(content) }.unwrap_or("");
    detached_new_comment(s)
}

/// libxml2 `xmlNewDocProp(doc, name, value)` — like [`xmlNewProp`]
/// but attached to a document rather than an element.  The returned
/// attribute is detached from any element (caller must
/// [`xmlAddChild`] it).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocProp(
    doc:   *mut XmlDoc,
    name:  *const c_char,
    value: *const c_char,
) -> *mut Attribute<'static> {
    let d = match unsafe { doc_ref(doc) } { Some(d) => d, None => return ptr::null_mut() };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    let value_s = unsafe { cstr_to_str(value) }.unwrap_or("");
    let attr: &Attribute<'_> = d.new_attribute(name_s, value_s);
    attr.doc.set(doc as *mut std::os::raw::c_void);
    attr as *const Attribute<'_> as *mut Attribute<'static>
}

/// libxml2 `xmlNewDocNodeEatName(doc, ns, name, content)` — like
/// [`xmlNewDocNode`] but the caller's `name` pointer is "consumed"
/// (semantically xmlFree-ed; libxml2's contract).  We allocate a
/// fresh copy in our arena and release the caller's pointer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocNodeEatName(
    doc:     *mut XmlDoc,
    ns:      *mut sup_xml_tree::dom::Namespace<'static>,
    name:    *mut c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    let node = unsafe { xmlNewDocNode(doc, ns, name, content) };
    // Release the caller's name buffer per the "Eat" contract.
    unsafe { crate::parse::xml_free_impl(name as *mut std::os::raw::c_void); }
    node
}

/// libxml2 `xmlNewDocRawNode(doc, ns, name, content)` — like
/// [`xmlNewDocNode`] but `content` is taken raw (no entity
/// decoding).  In our impl `xmlNewDocNode` already doesn't decode,
/// so this is a direct alias.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDocRawNode(
    doc:     *mut XmlDoc,
    ns:      *mut sup_xml_tree::dom::Namespace<'static>,
    name:    *const c_char,
    content: *const c_char,
) -> *mut Node<'static> {
    unsafe { xmlNewDocNode(doc, ns, name, content) }
}

/// Allocate a detached text node in a thread-local "scratch" doc.
///
/// libxml2 lets callers create text/comment nodes without an owning
/// document, then `xmlAddChild` them into one — at which point the
/// doc pointer gets stamped.  Until grafting, the node still needs
/// to live somewhere.  We keep a single thread-local scratch
/// `XmlDoc` and allocate detached nodes into its arena; once
/// grafted, the node's bytes survive via the thread-shared
/// `Arc<Bump>` even when the scratch doc is later freed.
fn detached_new_text(content: &str) -> *mut Node<'static> {
    let doc = ensure_scratch_doc();
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: ensure_scratch_doc returned non-NULL → valid XmlDoc.
    let cname = std::ffi::CString::new(content).unwrap_or_default();
    unsafe { xmlNewDocText(doc, cname.as_ptr()) }
}

/// Allocate a detached comment node in the thread-local scratch doc.
fn detached_new_comment(content: &str) -> *mut Node<'static> {
    let doc = ensure_scratch_doc();
    if doc.is_null() {
        return ptr::null_mut();
    }
    let cname = std::ffi::CString::new(content).unwrap_or_default();
    unsafe { xmlNewDocComment(doc, cname.as_ptr()) }
}

thread_local! {
    static SCRATCH_DOC: std::cell::Cell<*mut XmlDoc> = std::cell::Cell::new(ptr::null_mut());
}

/// Lazily construct a thread-local scratch doc that serves as the
/// home for detached nodes.  Leaks one tiny doc per thread; its arena
/// (registered in the per-thread keep-alive) lives until the thread
/// exits, so detached nodes survive being grafted into real documents.
fn ensure_scratch_doc() -> *mut XmlDoc {
    SCRATCH_DOC.with(|c| {
        let cur = c.get();
        if !cur.is_null() {
            return cur;
        }
        let d = unsafe { xmlNewDoc(ptr::null()) };
        c.set(d);
        d
    })
}

// ── attributes ────────────────────────────────────────────────────────────

/// libxml2 `xmlNewProp(node, name, value)` — create an attribute and
/// attach it to `node`.  Returns the new attribute.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewProp(
    node: *mut Node<'static>,
    name: *const c_char,
    value: *const c_char,
) -> *mut Attribute<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    let value_s = unsafe { cstr_to_str(value) }.unwrap_or("");
    // Find the doc the node belongs to (via doc field).
    let doc_ptr = n.doc.get() as *mut XmlDoc;
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let attr: &Attribute<'_> = d.new_attribute(name_s, value_s);
    d.append_attribute(n, attr);
    attr as *const Attribute<'_> as *mut Attribute<'static>
}

/// libxml2 `xmlSetProp(node, name, value)` — set or update an
/// attribute.  Returns the affected attribute (existing or new).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetProp(
    node: *mut Node<'static>,
    name: *const c_char,
    value: *const c_char,
) -> *mut Attribute<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    let value_s = unsafe { cstr_to_str(value) }.unwrap_or("");
    let _ = value_s;
    let target = local_name(name_s);
    // libxml2's xmlSetProp updates the existing attribute in place if
    // one with the same name exists.  Our Attribute::value field
    // isn't behind a Cell (it's an immutable arena-resident
    // ArenaCStr), so we can't reassign through `&Attribute`.
    // Workaround: remove the existing attribute and append a new one.
    // Pointer identity changes — but neither libxml2 nor lxml document
    // identity stability across xmlSetProp, so this matches consumer
    // expectations.
    let mut to_remove: *mut Attribute<'static> = ptr::null_mut();
    for existing in n.attributes() {
        if local_name(existing.name()) == target {
            to_remove = existing as *const Attribute<'_> as *mut Attribute<'static>;
            break;
        }
    }
    if !to_remove.is_null() {
        let _ = unsafe { xmlRemoveProp(to_remove) };
    }
    unsafe { xmlNewProp(node, name, value) }
}

/// libxml2 `xmlNewNsProp(node, ns, name, value)` — create a
/// namespaced attribute and attach it to `node`.  When `ns` is NULL,
/// behaves like [`xmlNewProp`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewNsProp(
    node: *mut Node<'static>,
    ns:   *mut sup_xml_tree::dom::Namespace<'static>,
    name: *const c_char,
    value: *const c_char,
) -> *mut Attribute<'static> {
    let new = unsafe { xmlNewProp(node, name, value) };
    if new.is_null() || ns.is_null() {
        return new;
    }
    // SAFETY: new came from xmlNewProp; ns is a non-null arena pointer.
    unsafe {
        let attr = &*(new as *const Attribute<'_>);
        let ns_ref = &*(ns as *const sup_xml_tree::dom::Namespace<'_>);
        attr.namespace.set(Some(ns_ref));
    }
    new
}

/// libxml2 `xmlSetNsProp(node, ns, name, value)` — set or update a
/// namespaced attribute.  Matching is by (namespace URI, local name).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetNsProp(
    node: *mut Node<'static>,
    ns:   *mut sup_xml_tree::dom::Namespace<'static>,
    name: *const c_char,
    value: *const c_char,
) -> *mut Attribute<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let name_s = match unsafe { cstr_to_str(name) } { Some(s) => s, None => return ptr::null_mut() };
    let target_local = local_name(name_s);
    // Determine the URI we're matching on: ns->href if non-NULL, else
    // "" (un-namespaced).
    let target_uri: &str = if ns.is_null() {
        ""
    } else {
        unsafe { (&*(ns as *const sup_xml_tree::dom::Namespace<'_>)).href() }
    };
    // Find an existing attribute with the same (uri, local_name).
    let mut to_remove: *mut Attribute<'static> = ptr::null_mut();
    for existing in n.attributes() {
        if local_name(existing.name()) != target_local {
            continue;
        }
        let existing_uri = existing.namespace.get()
            .map(|n| n.href())
            .unwrap_or("");
        if existing_uri == target_uri {
            to_remove = existing as *const Attribute<'_> as *mut Attribute<'static>;
            break;
        }
    }
    if !to_remove.is_null() {
        let _ = unsafe { xmlRemoveProp(to_remove) };
    }
    unsafe { xmlNewNsProp(node, ns, name, value) }
}

/// libxml2 `xmlNewNs(node, href, prefix)` — declare a namespace on
/// `node`.  Returns the new namespace; NULL on error.  When `node`
/// is NULL the namespace is created but not attached.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewNs(
    node:   *mut Node<'static>,
    href:   *const c_char,
    prefix: *const c_char,
) -> *mut sup_xml_tree::dom::Namespace<'static> {
    // Need a document to allocate from.
    let n = unsafe { node_ref(node) };
    let doc_ptr = match n {
        Some(nn) => nn.doc.get() as *mut XmlDoc,
        None => return ptr::null_mut(),
    };
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let href_s = unsafe { cstr_to_str(href) }.unwrap_or("");
    let prefix_s = unsafe { cstr_to_str(prefix) };
    // libxml2 refuses to add a second declaration for a prefix that is
    // already bound on the node: it walks the existing `nsDef`, and on a
    // matching prefix with a non-empty href it frees the new ns and
    // returns the existing one.  This is load-bearing for sub-tree
    // serialization — lxml re-declares each in-scope namespace on the
    // fragment root via `xmlNewNs`, and without the dedup a redeclared
    // default namespace would serialize as two `xmlns="…"` attributes.
    if let Some(nn) = n {
        let mut cur = nn.ns_def.get();
        while let Some(existing) = cur {
            if existing.prefix() == prefix_s && !existing.href().is_empty() {
                return existing as *const _ as *mut sup_xml_tree::dom::Namespace<'static>;
            }
            cur = existing.next.get();
        }
    }
    let ns_ref = d.bump_new_namespace(prefix_s, href_s);
    // Attach to the node's ns_def chain (libxml2 behavior).
    if let Some(nn) = n {
        d.attach_ns_def(nn, ns_ref);
    }
    ns_ref as *const _ as *mut sup_xml_tree::dom::Namespace<'static>
}

/// libxml2 `xmlSetNs(node, ns)` — set the element's namespace
/// binding (the `ns` field at offset 72).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetNs(
    node: *mut Node<'static>,
    ns:   *mut sup_xml_tree::dom::Namespace<'static>,
) {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return };
    if ns.is_null() {
        n.namespace.set(None);
    } else {
        // SAFETY: ns is a non-null arena pointer.
        unsafe {
            let ns_ref = &*(ns as *const sup_xml_tree::dom::Namespace<'_>);
            n.namespace.set(Some(ns_ref));
        }
    }
}

/// libxml2 `xmlNodeSetContent(cur, content)` — replace `cur`'s
/// libxml2 `xmlDOMWrapAdoptNode(ctxt, src_doc, node, dest_doc, dest_parent, options)`
/// — move `node` (a subtree from `src_doc`) into `dest_doc`,
/// optionally attaching to `dest_parent`.  In libxml2 this is a
/// move (sharing storage when both docs share a dict); we adopt by
/// **deep-copy** via [`sup_xml_tree::dom::Document::adopt_subtree`]
/// and attach the copy under `dest_parent`.
///
/// The `ctxt` argument (DOM-wrap context — namespace reconciliation
/// hints) is ignored; namespaces aren't yet copied across arenas (see
/// `adopt_subtree` doc).  Returns 0 on success, -1 on NULL inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDOMWrapAdoptNode(
    _ctxt:        *mut c_void,
    _src_doc:     *mut XmlDoc,
    node:         *mut Node<'static>,
    dest_doc:     *mut XmlDoc,
    dest_parent:  *mut Node<'static>,
    _options:     c_int,
) -> c_int {
    if node.is_null() || dest_doc.is_null() { return -1; }
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return -1 };
    let d = match unsafe { doc_ref(dest_doc) } { Some(d) => d, None => return -1 };
    let adopted = d.adopt_subtree(n);
    if !dest_parent.is_null() {
        if let Some(p) = unsafe { node_ref(dest_parent) } {
            d.append_child(p, adopted);
        }
    }
    0
}

/// libxml2 `xmlNodeSetContentLen(cur, content, len)` — bounded
/// variant of [`xmlNodeSetContent`].  Reads at most `len` bytes (or
/// stops at the first NUL within them) and delegates.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeSetContentLen(
    cur:     *mut Node<'static>,
    content: *const c_char,
    len:     c_int,
) {
    if cur.is_null() || content.is_null() || len < 0 { return; }
    // SAFETY: caller asserts `content` is readable for `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(content as *const u8, len as usize) };
    let stop = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    // Build a NUL-terminated CString and delegate.
    let cstr = match std::ffi::CString::new(&bytes[..stop]) {
        Ok(s)  => s,
        Err(_) => return,
    };
    unsafe { xmlNodeSetContent(cur, cstr.as_ptr()); }
}

/// libxml2 `xmlNodeSetLang(cur, lang)` — set the element's
/// `xml:lang` attribute.  No-op on non-elements or NULL `lang`.
///
/// The libxml2 contract also allows passing NULL to remove the
/// attribute; we don't implement that path yet (we don't have
/// xmlUnsetProp wired).  Callers needing "clear" should use
/// xmlSetProp with an empty string, or remove the attribute via
/// xmlRemoveProp.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeSetLang(
    cur:  *mut Node<'static>,
    lang: *const c_char,
) {
    if cur.is_null() || lang.is_null() { return; }
    let n = match unsafe { node_ref(cur) } { Some(n) => n, None => return };
    if !matches!(n.kind, NodeKind::Element) { return; }
    let lang_cstr = std::ffi::CString::new("xml:lang").unwrap();
    unsafe { crate::mutate::xmlSetProp(cur, lang_cstr.as_ptr(), lang); }
}

/// content.  For text/CData/Comment nodes, sets the content string.
/// For elements, replaces all children with a single text node
/// containing `content`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeSetContent(
    cur: *mut Node<'static>,
    content: *const c_char,
) {
    let n = match unsafe { node_ref(cur) } { Some(n) => n, None => return };
    let content_s = unsafe { cstr_to_str(content) }.unwrap_or("");
    let doc_ptr = n.doc.get() as *mut XmlDoc;
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return };
    match n.kind {
        NodeKind::Element => {
            // Drop all children, replace with a fresh text node.
            n.first_child.set(None);
            n.last_child.set(None);
            if !content_s.is_empty() {
                let txt = d.new_text(content_s);
                d.append_child(n, txt);
            }
        }
        NodeKind::Text | NodeKind::CData | NodeKind::Comment | NodeKind::Pi => {
            // Replace the content slot directly — c-abi field is
            // `Cell<ArenaCStr>`.
            let bump = d.bump();
            let bytes = content_s.as_bytes();
            let dst: &mut [u8] = bump.alloc_slice_fill_with(bytes.len() + 1, |i| {
                if i < bytes.len() { bytes[i] } else { 0 }
            });
            // SAFETY: dst points at a NUL-terminated UTF-8 slice
            // owned by the bump arena.
            unsafe {
                let new_content = sup_xml_tree::dom::ArenaCStr::from_raw(dst.as_ptr());
                n.content.set(Some(new_content));
            }
        }
        _ => {}
    }
}

/// libxml2 `xmlNodeSetName(node, name)` — rename a node.  Copies the
/// caller-supplied NUL-terminated `name` into the document's bump
/// arena, then writes the new pointer into `node.name` (offset 16 in
/// the c-abi `_xmlNode` layout — guaranteed by the compile-time
/// `offset_of!` assertions in `sup-xml-tree`).
///
/// The old name remains allocated in the arena; bumpalo never frees
/// individual allocations, so this is consistent with libxml2's
/// internal `xmlDictLookup`/`xmlStrdup` semantics from the caller's
/// perspective (the old name pointer is no longer referenced).
///
/// Returns silently — libxml2's `xmlNodeSetName` returns `void` too.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeSetName(
    node: *mut Node<'static>,
    name: *const c_char,
) {
    if node.is_null() || name.is_null() { return; }
    // Read-only borrow to look up the doc + allocate; the &Node
    // dies at the end of this block, before we do the raw write.
    let new_name_ptr: *const u8 = {
        let n = match unsafe { node_ref(node) } { Some(n) => n, None => return };
        let s = match unsafe { cstr_to_str(name) } { Some(s) if !s.is_empty() => s, _ => return };
        let doc_ptr = n.doc.get() as *mut XmlDoc;
        let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return };
        // Intern the new name through the document's name dict so it's
        // the dict-canonical pointer, exactly as the parser and
        // xmlNewDocNode do.  Consumers locate children by interning the
        // wanted tag (`xmlDictExists(doc->dict, name)`) and then
        // comparing node-name pointers (lxml.objectify's `_tagMatches`,
        // iterparse tag filters); a bump-allocated name carries the
        // right bytes but the wrong address and never matches.
        let dict = d.dict_ptr();
        if dict.is_null() { return; }
        // SAFETY: dict is the document's refcount-managed name dict,
        // live for as long as `d` (and the node) is.
        unsafe { (*dict).intern(s.as_bytes()) }
    };
    // SAFETY:
    //   * `node` points at a live `Node<'doc>` (caller contract).
    //   * `new_name_ptr` is a NUL-terminated UTF-8 slice in the
    //     doc's arena that lives as long as the doc.
    //   * `addr_of_mut!` takes the place expression's address without
    //     forming any reference, so no aliasing-with-&Node issue.
    //   * `Node.name` is `ArenaCStr` (8 bytes, single ptr); we write
    //     a fresh `ArenaCStr::from_raw` value over it.  The previous
    //     pointer is overwritten — bumpalo retains the bytes, so any
    //     C caller that cached the old pointer is unaffected.
    unsafe {
        let name_field: *mut sup_xml_tree::dom::ArenaCStr<'static> =
            std::ptr::addr_of_mut!((*node).name);
        std::ptr::write(
            name_field,
            sup_xml_tree::dom::ArenaCStr::from_raw(new_name_ptr),
        );
    }
}

/// libxml2 `xmlRemoveProp(attr)` — detach + (logically) free.
/// Returns 0 on success, -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRemoveProp(attr: *mut Attribute<'static>) -> std::os::raw::c_int {
    let a = match unsafe { attr_ref(attr) } { Some(a) => a, None => return -1 };
    let parent = match a.parent.get() { Some(p) => p, None => return -1 };
    // Unlink from the attribute list.
    let prev = a.prev.get();
    let next = a.next.get();
    match (prev, next) {
        (Some(p), Some(n)) => { p.next.set(Some(n)); n.prev.set(Some(p)); }
        (Some(p), None)    => { p.next.set(None); parent.last_attribute.set(Some(p)); }
        (None,    Some(n)) => { n.prev.set(None); parent.first_attribute.set(Some(n)); }
        (None,    None)    => {
            parent.first_attribute.set(None);
            parent.last_attribute.set(None);
        }
    }
    a.parent.set(None);
    a.next.set(None);
    a.prev.set(None);
    0
}

/// libxml2 `xmlCopyProp(target, cur)` — clone the attribute `cur` and
/// (if `target` is non-NULL) attach the copy to `target`.  The copy
/// lives in the same arena as `target` (or as `cur`, when `target`
/// is NULL).  Returns NULL on error.
///
/// The clone is byte-equivalent to the source: name and value are
/// re-interned in the destination arena.  Namespace bindings are not
/// reconciled — callers needing that should follow up with
/// [`xmlReconciliateNs`] (when available).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyProp(
    target: *mut Node<'static>,
    cur:    *mut Attribute<'static>,
) -> *mut Attribute<'static> {
    let src = match unsafe { attr_ref(cur) } { Some(a) => a, None => return ptr::null_mut() };
    // Resolve the destination doc: target's doc if provided, else the
    // source attribute's owning doc.
    let doc_ptr: *mut XmlDoc = if target.is_null() {
        // Source attribute's doc field.
        src.doc.get() as *mut XmlDoc
    } else {
        // SAFETY: caller asserts target is a valid arena node.
        unsafe { (*target).doc.get() as *mut XmlDoc }
    };
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let attr = d.new_attribute(src.name(), src.value());
    // Stamp the doc pointer so downstream walkers see the right owner.
    attr.doc.set(doc_ptr as *mut c_void);
    if !target.is_null() {
        // SAFETY: target is non-null and valid for the call.
        let tnode = unsafe { &*target };
        d.append_attribute(tnode, attr);
    }
    attr as *const Attribute<'_> as *mut Attribute<'static>
}

/// libxml2 `xmlCopyNamespace(cur)` — clone a single namespace
/// declaration.  The copy is detached — link it via `xmlSetNs` /
/// element `ns_def` manipulation as appropriate.  Returns NULL on
/// error.
///
/// Limitation: the cloned binding is allocated in the source's
/// owning document arena.  libxml2 itself doesn't take a target-doc
/// argument here — consumers that need a copy in a different arena
/// can chain through `xmlCopyNamespaceList` (not yet implemented).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyNamespace(
    cur: *mut Namespace<'static>,
) -> *mut Namespace<'static> {
    if cur.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts cur is a valid arena namespace pointer.
    let src = unsafe { &*cur };
    // We need an owning arena.  Look at the namespace's `context`
    // field (libxml2's xmlNs::context points to the owning xmlDoc).
    #[cfg(feature = "c-abi")]
    let doc_ptr = src.context.get() as *mut XmlDoc;
    #[cfg(not(feature = "c-abi"))]
    let doc_ptr: *mut XmlDoc = ptr::null_mut();
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let copy = d.bump_new_namespace(src.prefix(), src.href());
    copy as *const Namespace<'_> as *mut Namespace<'static>
}

/// libxml2 `xmlFreeProp(attr)` — release a single attribute.  The
/// attribute must already be detached from its parent (we don't
/// unlink it here, matching libxml2's contract that the caller has
/// done so).  Since attributes live in their document's arena, this
/// is effectively a no-op — memory is reclaimed when the document is
/// freed.  We accept the call for ABI compatibility and clear the
/// chain pointers as a safety measure against use-after-free.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeProp(attr: *mut Attribute<'static>) {
    if attr.is_null() { return; }
    // SAFETY: attr is a valid arena attribute pointer.
    let a = unsafe { &*attr };
    a.next.set(None);
    a.prev.set(None);
    a.parent.set(None);
}

// ── deep copy ─────────────────────────────────────────────────────────────

/// libxml2 `xmlCopyNode(node, recursive)` — copy a node into the
/// same document as the source.  `recursive`:
///   1 → deep copy (children too).
///   2 → shallow copy + properties (no children).
///   0 → shallow (no children, no properties).
///
/// The copy is detached — link it via `xmlAddChild` etc.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyNode(
    node: *mut Node<'static>,
    recursive: c_int,
) -> *mut Node<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let doc_ptr = n.doc.get() as *mut XmlDoc;
    let d = match unsafe { doc_ref(doc_ptr) } { Some(d) => d, None => return ptr::null_mut() };
    let copy_kids = recursive == 1;
    let copy_attrs = recursive == 1 || recursive == 2;
    deep_copy_top(d, n, copy_kids, copy_attrs, doc_ptr)
}

/// libxml2 `xmlDocCopyNode(node, doc, recursive)` — copy a node
/// into a *different* document.  Same semantics as `xmlCopyNode`
/// otherwise.  All strings are re-allocated in the destination
/// document's arena.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDocCopyNode(
    node: *mut Node<'static>,
    doc:  *mut XmlDoc,
    recursive: c_int,
) -> *mut Node<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let d = match unsafe { doc_ref(doc) } { Some(d) => d, None => return ptr::null_mut() };
    let copy_kids = recursive == 1;
    let copy_attrs = recursive == 1 || recursive == 2;
    deep_copy_top(d, n, copy_kids, copy_attrs, doc)
}

/// libxml2 `xmlCopyDoc(doc, recursive)` — copy an entire document.
/// `recursive=1` deep-copies the tree; `recursive=0` returns an
/// empty doc with just the metadata.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyDoc(
    doc: *mut XmlDoc,
    recursive: c_int,
) -> *mut XmlDoc {
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts doc came from xmlReadMemory/xmlNewDoc.
    let version_cs = unsafe {
        let d_ref = &*doc;
        std::ffi::CString::new(d_ref._doc.version.as_bytes()).unwrap_or_default()
    };
    let new_doc = unsafe { xmlNewDoc(version_cs.as_ptr()) };
    // Preserve `URL` (offset 136) — libxml2's xmlCopyDoc does this
    // unconditionally, and xsl:import / xsl:include base-URI
    // resolution relies on it.  Without it, libxslt walks
    // xmlNodeGetBase on a copied doc and gets NULL, so relative
    // hrefs can't resolve.
    unsafe {
        let src_url = (*doc).url;
        if !src_url.is_null() {
            let s = std::ffi::CStr::from_ptr(src_url);
            let cs = std::ffi::CString::new(s.to_bytes()).unwrap_or_default();
            (*new_doc).url = cs.into_raw();
        }
    }
    if recursive != 0 {
        // SAFETY: doc is non-null; children is its root (or NULL).
        let src_root = unsafe { (*doc).children.get() };
        if !src_root.is_null() {
            // Deep copy the source root into the new doc, then set
            // as root.
            let new_doc_d = match unsafe { doc_ref(new_doc) } { Some(d) => d, None => return ptr::null_mut() };
            // SAFETY: src_root points into source doc's arena.
            let src_n = unsafe { &*src_root };
            let copy = deep_copy_top(new_doc_d, src_n, true, true, new_doc);
            if !copy.is_null() {
                let _ = unsafe { xmlDocSetRootElement(new_doc, copy) };
            }
        }
    }
    // Copy the internal subset (DOCTYPE + declarations) so the copy
    // round-trips the same `<!DOCTYPE …[ … ]>` as the original.
    unsafe { crate::dtd::copy_int_subset(doc, new_doc); }
    new_doc
}

/// Allocate a fresh node in `dest` arena mirroring `src`.  Handles
/// all node kinds; copies the name (for elements/PIs) and content
/// (for text/CData/comment).
///
/// `dest_doc` is the `XmlDoc*` corresponding to `dest`; every fresh
/// node + attribute has its `doc` slot stamped to it so that
/// consumers reading `node->doc` after the copy (lxml's
/// `moveNodeToDocument` does this immediately on the just-returned
/// pointer) don't dereference NULL.
pub(crate) fn deep_copy_into<'d>(
    dest: &'d sup_xml_tree::dom::Document,
    src:  &Node<'_>,
    copy_kids: bool,
    copy_attrs: bool,
    dest_doc: *mut XmlDoc,
) -> *mut Node<'static> {
    let n: &Node<'d> = match src.kind {
        NodeKind::Element  => dest.new_element(dest.bump().alloc_str(src.name())),
        NodeKind::Text     => dest.new_text(dest.bump().alloc_str(src.content())),
        NodeKind::CData    => dest.new_cdata(dest.bump().alloc_str(src.content())),
        NodeKind::Comment  => dest.new_comment(dest.bump().alloc_str(src.content())),
        NodeKind::Pi       => dest.new_pi(
            dest.bump().alloc_str(src.name()),
            src.content_opt().map(|c| &*dest.bump().alloc_str(c)),
        ),
        // Attribute and Document discriminants don't appear as Node kinds.
        _ => return ptr::null_mut(),
    };
    // Stamp the doc pointer on the new node.  `Document::new_*`
    // leaves the c-abi `doc` slot NULL — only finalization (parser's
    // `into_xml_doc`) fills it in for parsed trees.  Copies bypass
    // that walk, so we stamp here.
    unsafe { stamp_doc_one(n, dest_doc); }
    // Copy the namespace into the destination arena.  The source ns
    // lives in the source document's arena; under per-document arenas
    // that arena can be freed independently of this copy (libxslt
    // compiles a stylesheet into copies, then frees the source doc —
    // the copies must not point back into it).  Allocating prefix/href
    // afresh in `dest` makes the copy self-contained.  Without setting
    // the ns at all, lxml's serializer-side `xmlCopyNode(root, 2)`
    // would lose the QName prefix.
    if matches!(src.kind, NodeKind::Element) {
        if let Some(src_ns) = src.namespace.get() {
            let prefix_copy: Option<&str> = src_ns.prefix().map(|p| &*dest.bump().alloc_str(p));
            let href_copy: &str = &*dest.bump().alloc_str(src_ns.href());
            let new_ns = dest.bump_new_namespace(prefix_copy, href_copy);
            let n_ref: &Node<'static> = unsafe { &*(n as *const Node<'_> as *const Node<'static>) };
            // SAFETY: `new_ns` lives in `dest`'s arena, which backs the
            // node we're returning; widening to 'static matches how the
            // c-abi tree stores namespace back-pointers.
            n_ref.namespace.set(Some(unsafe {
                &*(new_ns as *const _ as *const sup_xml_tree::dom::Namespace<'static>)
            }));
        }
    }
    if copy_attrs && matches!(src.kind, NodeKind::Element) {
        for src_attr in src.attributes() {
            let new_attr = dest.new_attribute(
                dest.bump().alloc_str(src_attr.name()),
                dest.bump().alloc_str(src_attr.value()),
            );
            // Copy the attribute's namespace into `dest`'s arena too
            // (same self-containment rationale as the element ns above)
            // — without it, serialization-side `xmlCopyNode(c_node, 2)`
            // loses namespaced attributes' prefixes (`<el ns:attr="…"/>`
            // round-trips as `<el attr="…"/>`).
            if let Some(src_ns) = src_attr.namespace.get() {
                let prefix_copy: Option<&str> = src_ns.prefix().map(|p| &*dest.bump().alloc_str(p));
                let href_copy: &str = &*dest.bump().alloc_str(src_ns.href());
                let new_ns = dest.bump_new_namespace(prefix_copy, href_copy);
                let new_attr_static: &Attribute<'static> = unsafe {
                    &*(new_attr as *const Attribute<'_> as *const Attribute<'static>)
                };
                new_attr_static.namespace.set(Some(unsafe {
                    &*(new_ns as *const _ as *const sup_xml_tree::dom::Namespace<'static>)
                }));
            }
            // Same doc-stamping rationale as on the parent node above.
            new_attr.doc.set(dest_doc as *mut std::os::raw::c_void);
            dest.append_attribute(n, new_attr);
        }
        // Copy namespace declarations.  These live on the `ns_def`
        // chain rather than in attributes (libxml2 convention);
        // without copying them, consumers that re-parse a copied
        // doc see undeclared prefixes (libxml2's xmlSchema does
        // exactly this — `xmlCopyDoc` + `xmlDocCopyNode` then
        // re-parse the result, and fails on prefixed elements when
        // the xmlns decl was dropped).
        //
        // compat is always built against tree's `c-abi` feature, so
        // `ns_def` is always available here — no cfg gate needed.
        let mut ns_cur = src.ns_def.get();
        while let Some(src_ns) = ns_cur {
            let prefix_copy: Option<&str> = src_ns.prefix().map(|p| &*dest.bump().alloc_str(p));
            let href_copy: &str = &*dest.bump().alloc_str(src_ns.href());
            let new_ns = dest.bump_new_namespace(prefix_copy, href_copy);
            dest.attach_ns_def(n, new_ns);
            ns_cur = src_ns.next.get();
        }
    }
    if copy_kids {
        for src_child in src.children() {
            let child_ptr = deep_copy_into(dest, src_child, copy_kids, copy_attrs, dest_doc);
            if !child_ptr.is_null() {
                // SAFETY: child_ptr was just allocated in dest.bump().
                let child_ref: &Node<'d> = unsafe { &*(child_ptr as *const Node<'d>) };
                dest.append_child(n, child_ref);
            }
        }
    }
    n as *const Node<'_> as *mut Node<'static>
}

/// Copy `src` into `dest` (via [`deep_copy_into`]) and then reconcile
/// the result's namespaces, mirroring libxml2's `xmlStaticCopyNode`.
///
/// This is the entry point the public copy functions use; the internal
/// recursion stays on [`deep_copy_into`] so reconciliation runs exactly
/// once, over the whole copied subtree.
pub(crate) fn deep_copy_top<'d>(
    dest:      &'d sup_xml_tree::dom::Document,
    src:       &Node<'_>,
    copy_kids: bool,
    copy_attrs: bool,
    dest_doc:  *mut XmlDoc,
) -> *mut Node<'static> {
    let copy = deep_copy_into(dest, src, copy_kids, copy_attrs, dest_doc);
    if !copy.is_null() {
        // SAFETY: `copy` was just allocated in `dest`'s arena, so it is
        // sound to view it with `dest`'s lifetime.
        let root: &'d Node<'d> = unsafe { &*(copy as *const Node<'d>) };
        reconcile_copied_ns(dest, root);
    }
    copy
}

/// libxml2's `xmlStaticCopyNode` re-declares any namespace that a copied
/// node references but whose `xmlns` declaration lived on an ancestor
/// *outside* the copied subtree: the declaration is recreated on the
/// copy's root.  Without this a lifted fragment serializes with a prefix
/// but no matching declaration — e.g. an `<xs:schema>` copied out of a
/// `<wsdl:definitions xmlns:xs="…">` re-parses as an undeclared `xs`
/// prefix (this is exactly how lxml feeds a sub-element to `XMLSchema`).
fn reconcile_copied_ns<'d>(dest: &'d sup_xml_tree::dom::Document, root: &'d Node<'d>) {
    if !matches!(root.kind, NodeKind::Element) {
        return;
    }
    // Pre-order walk over the copied subtree.  Raw pointers sidestep the
    // borrow that `children()` would otherwise tie to each `Node`.
    let mut stack: Vec<*const Node<'d>> = vec![root];
    while let Some(np) = stack.pop() {
        // SAFETY: every pointer pushed here came from `dest`'s arena.
        let n: &'d Node<'d> = unsafe { &*np };
        if matches!(n.kind, NodeKind::Element) {
            if let Some(ns) = n.namespace.get() {
                ensure_ns_declared(dest, root, n, ns.prefix(), ns.href());
            }
            for a in n.attributes() {
                if let Some(ns) = a.namespace.get() {
                    ensure_ns_declared(dest, root, n, ns.prefix(), ns.href());
                }
            }
        }
        for child in n.children() {
            stack.push(child as *const Node<'d>);
        }
    }
}

/// Declare `(prefix, href)` on `root` unless it is already in scope at
/// `node` (walking up to `root`).  The predefined `xml` prefix is never
/// re-declared (XML 1.0 §3.7).
fn ensure_ns_declared<'d>(
    dest:   &'d sup_xml_tree::dom::Document,
    root:   &'d Node<'d>,
    node:   &'d Node<'d>,
    prefix: Option<&str>,
    href:   &str,
) {
    if prefix == Some("xml") || href == "http://www.w3.org/XML/1998/namespace" {
        return;
    }
    if ns_prefix_in_scope(node, root, prefix) {
        return;
    }
    let prefix_copy: Option<&'d str> = prefix.map(|p| &*dest.bump().alloc_str(p));
    let href_copy: &'d str = &*dest.bump().alloc_str(href);
    let new_ns = dest.bump_new_namespace(prefix_copy, href_copy);
    dest.attach_ns_def(root, new_ns);
}

/// Is `prefix` bound by an `xmlns` declaration in scope at `node`,
/// searching `node` and its ancestors up to and including `root`?
fn ns_prefix_in_scope<'d>(node: &'d Node<'d>, root: &'d Node<'d>, prefix: Option<&str>) -> bool {
    let mut cur: Option<&'d Node<'d>> = Some(node);
    while let Some(c) = cur {
        if matches!(c.kind, NodeKind::Element) {
            let mut ns = c.ns_def.get();
            while let Some(d) = ns {
                if d.prefix() == prefix {
                    return true;
                }
                ns = d.next.get();
            }
        }
        if std::ptr::eq(c as *const Node<'_>, root as *const Node<'_>) {
            break;
        }
        cur = c.parent.get();
    }
    false
}

// ── linking + unlinking ───────────────────────────────────────────────────

/// libxml2 `xmlAddChild(parent, child)` — append `child` as the last
/// child of `parent`.  Returns `child` on success, NULL on error.
///
/// Cross-document attachment isn't supported in v0.1; mismatched
/// arenas return NULL with the last-error slot populated.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddChild(
    parent: *mut Node<'static>,
    child: *mut Node<'static>,
) -> *mut Node<'static> {
    let p = match unsafe { node_ref(parent) } { Some(n) => n, None => return ptr::null_mut() };
    let c = match unsafe { node_ref(child) } { Some(n) => n, None => return ptr::null_mut() };
    // Detach child from its current parent (if any) — libxml2 does this.
    detach_node(c);
    // Find the doc to call append_child via its method.
    let doc_ptr = p.doc.get() as *mut XmlDoc;
    // Pin the child's origin arena onto this doc before the graft
    // retags `c.doc` — keeps cross-thread-grafted memory alive.
    unsafe { retain_graft_arena(c, doc_ptr); }
    let d = match unsafe { doc_ref(doc_ptr) } {
        Some(d) => d,
        None => {
            // Fallback: parent has no doc pointer — just wire the
            // pointers manually.
            link_child_manual(p, c);
            return child;
        }
    };
    d.append_child(p, c);
    note_cross_doc_donation(c, p);
    child
}

/// libxml2 `xmlAddSibling(cur, elem)` — append `elem` to the end of
/// `cur`'s sibling list (i.e. after `cur`'s last sibling).  Returns
/// the appended node, or NULL on error.
///
/// `elem` is detached from any prior parent before being relinked.
/// Returns the newly-attached node (which may be `elem` or — for
/// text-into-text appends, where libxml2 merges into the tail — a
/// reference to the merged tail).  We currently always return
/// `elem` itself; text-merging is not yet performed.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddSibling(
    cur:  *mut Node<'static>,
    elem: *mut Node<'static>,
) -> *mut Node<'static> {
    let c = match unsafe { node_ref(cur) }  { Some(n) => n, None => return ptr::null_mut() };
    let _ = match unsafe { node_ref(elem) } { Some(n) => n, None => return ptr::null_mut() };
    // Walk to the tail of cur's sibling chain, then defer to
    // xmlAddNextSibling.  Using the existing helper keeps the
    // parent/cross-doc bookkeeping in one place.
    let mut tail = c;
    while let Some(next) = tail.next_sibling.get() {
        tail = next;
    }
    let tail_ptr = tail as *const Node<'_> as *mut Node<'static>;
    unsafe { xmlAddNextSibling(tail_ptr, elem) }
}

/// libxml2 `xmlAddNextSibling(node, new)` — insert `new` right after
/// `node` in its sibling list.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddNextSibling(
    node: *mut Node<'static>,
    new_node: *mut Node<'static>,
) -> *mut Node<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let nn = match unsafe { node_ref(new_node) } { Some(n) => n, None => return ptr::null_mut() };
    // Insert-after-self: no-op.  See xmlAddPrevSibling for context.
    if std::ptr::eq(n as *const _, nn as *const _) {
        return new_node;
    }
    detach_node(nn);
    // Pin nn's origin arena onto n's document before retagging nn.doc.
    unsafe { retain_graft_arena(nn, n.doc.get() as *mut XmlDoc); }
    // libxml2 links the sibling chain even for document-level nodes
    // whose `parent` is NULL (a created doc's root, or prolog/epilogue
    // comments/PIs around it).  `nn` inherits `n`'s parent (possibly
    // None); the chain pointers wire up regardless, and `nn` adopts
    // `n`'s owning document.
    let parent = n.parent.get();
    nn.parent.set(parent);
    nn.doc.set(n.doc.get());
    nn.prev_sibling.set(Some(n));
    let next = n.next_sibling.get();
    nn.next_sibling.set(next);
    n.next_sibling.set(Some(nn));
    match (next, parent) {
        (Some(nx), _) => nx.prev_sibling.set(Some(nn)),
        // `nn` is the new last sibling under an element parent.
        (None, Some(p)) => p.last_child.set(Some(nn)),
        // `nn` is the new last document-level node: update the doc's
        // `last` through the shared `XmlDoc`/`Node` layout.
        (None, None) => {
            let doc = n.doc.get() as *mut XmlDoc;
            if !doc.is_null() {
                unsafe { (*doc).last.set(nn as *const Node<'_> as *mut Node<'static>); }
            }
        }
    }
    if let Some(p) = parent { note_cross_doc_donation(nn, p); }
    new_node
}

/// libxml2 `xmlAddPrevSibling(node, new)` — insert `new` right before
/// `node`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddPrevSibling(
    node: *mut Node<'static>,
    new_node: *mut Node<'static>,
) -> *mut Node<'static> {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    let nn = match unsafe { node_ref(new_node) } { Some(n) => n, None => return ptr::null_mut() };
    // Insert-before-self: no-op.  libxml2 consumers (e.g. lxml's
    // `_addSibling`) hit this when reordering an already-adjacent
    // pair — they call `xmlAddPrevSibling(target, new_node)` even
    // when `new_node` is already `target`'s prev or the same node
    // and expect the tree to be unchanged.  Without this guard,
    // `detach_node(nn)` below removes the node from its parent
    // and then the subsequent `n.parent.get()` returns None,
    // bailing with the node permanently detached.
    if std::ptr::eq(n as *const _, nn as *const _) {
        return new_node;
    }
    detach_node(nn);
    // Pin nn's origin arena onto n's document before retagging nn.doc.
    unsafe { retain_graft_arena(nn, n.doc.get() as *mut XmlDoc); }
    // See `xmlAddNextSibling`: link the chain even for document-level
    // nodes with a NULL parent (here `nn` may become the doc's new
    // first child — a prolog comment/PI before the root element).
    let parent = n.parent.get();
    nn.parent.set(parent);
    nn.doc.set(n.doc.get());
    nn.next_sibling.set(Some(n));
    let prev = n.prev_sibling.get();
    nn.prev_sibling.set(prev);
    n.prev_sibling.set(Some(nn));
    match (prev, parent) {
        (Some(pv), _) => pv.next_sibling.set(Some(nn)),
        (None, Some(p)) => p.first_child.set(Some(nn)),
        // `nn` is the new first document-level node: update the doc's
        // `children` through the shared `XmlDoc`/`Node` layout.
        (None, None) => {
            let doc = n.doc.get() as *mut XmlDoc;
            if !doc.is_null() {
                unsafe { (*doc).children.set(nn as *const Node<'_> as *mut Node<'static>); }
            }
        }
    }
    // If the new node came from a different document's arena, that
    // arena now owns memory still reachable from this doc's tree —
    // mark it "donated" so xmlFreeDoc skips the arena drop.
    if let Some(p) = parent { note_cross_doc_donation(nn, p); }
    new_node
}

/// libxml2 `xmlReplaceNode(old, cur)` — replace `old` with `cur` in
/// the tree.  Returns `old` (now detached).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReplaceNode(
    old:  *mut Node<'static>,
    cur:  *mut Node<'static>,
) -> *mut Node<'static> {
    // Replacing a node with itself is a no-op (libxml2 returns early on
    // `old == cur`).  Without this guard, `detach_node` below would
    // unlink the node and the subsequent `o.parent` read — now NULL —
    // would bail, orphaning it.
    if std::ptr::eq(old, cur) {
        return old;
    }
    let o = match unsafe { node_ref(old) } { Some(n) => n, None => return ptr::null_mut() };
    let c = match unsafe { node_ref(cur) } { Some(n) => n, None => return ptr::null_mut() };
    detach_node(c);
    // Pin c's origin arena onto the document that owns `old`.
    unsafe { retain_graft_arena(c, o.doc.get() as *mut XmlDoc); }
    let parent = match o.parent.get() { Some(p) => p, None => return ptr::null_mut() };
    let prev = o.prev_sibling.get();
    let next = o.next_sibling.get();
    c.parent.set(Some(parent));
    c.prev_sibling.set(prev);
    c.next_sibling.set(next);
    match prev {
        Some(pv) => pv.next_sibling.set(Some(c)),
        None     => parent.first_child.set(Some(c)),
    }
    match next {
        Some(nx) => nx.prev_sibling.set(Some(c)),
        None     => parent.last_child.set(Some(c)),
    }
    o.parent.set(None);
    o.prev_sibling.set(None);
    o.next_sibling.set(None);
    note_cross_doc_donation(c, parent);
    old
}

/// libxml2 `xmlUnlinkNode(node)` — detach `node` from its parent.
/// The node itself is not freed; arena memory persists.
///
/// A cross-document move (lxml's `_appendChild` / `_addSibling`) opens
/// with `xmlUnlinkNode`, then relinks the node into the destination by
/// direct pointer writes we never see.  Stash the node's origin arena
/// so the move's subsequent cross-thread name re-intern
/// (`xmlDictLookup`) can pin it onto the destination — see
/// `dict::stash_graft_source_arena`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUnlinkNode(node: *mut Node<'static>) {
    if let Some(n) = unsafe { node_ref(node) } {
        let doc = n.doc.get() as *mut XmlDoc;
        if let Some(d) = unsafe { doc_ref(doc) } {
            crate::dict::stash_graft_source_arena(d.bump_arc());
        }
        // libxml2 special-cases the DTD node: unlinking a document's
        // internal or external subset clears the owning `doc` pointer so
        // `intSubset` / `extSubset` don't dangle (lxml's
        // `docinfo.clear()` relies on this to drop the DOCTYPE).
        if matches!(n.kind, NodeKind::Dtd) && !doc.is_null() {
            let node_addr = node as *mut std::os::raw::c_void;
            unsafe {
                if (*doc).int_subset == node_addr { (*doc).int_subset = ptr::null_mut(); }
                if (*doc).ext_subset == node_addr { (*doc).ext_subset = ptr::null_mut(); }
            }
        }
        detach_node(n);
    }
}

/// libxml2 `xmlFreeNode(node)` — would deallocate; we can't reclaim
/// individual arena allocations.  Unlinks from the tree and lets
/// the bytes leak (released on `xmlFreeDoc`).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeNode(node: *mut Node<'static>) {
    if let Some(n) = unsafe { node_ref(node) } {
        detach_node(n);
    }
}

/// libxml2 `xmlFreeNodeList(node)` — free `node` and every sibling
/// after it.  Arena-allocated; we unlink the chain and let the bytes
/// outlive (reclaimed wholesale on `xmlFreeDoc`).  NULL is a no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeNodeList(node: *mut Node<'static>) {
    let Some(start) = (unsafe { node_ref(node) }) else { return };
    // Collect siblings first — detaching mutates the chain.
    let mut to_unlink: Vec<&Node<'_>> = Vec::new();
    let mut cur = Some(start);
    while let Some(n) = cur {
        to_unlink.push(n);
        cur = n.next_sibling.get();
    }
    for n in to_unlink {
        detach_node(n);
    }
}

/// libxml2 `xmlNodeAddContent(node, content)` — append `content` to
/// `node`'s textual payload.  Behaviour by node kind:
/// * Element → append a text child (or merge into an existing
///   trailing text child).
/// * Text / CData / Comment / PI → concatenate to existing content.
/// NULL inputs are silent no-ops.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeAddContent(
    node: *mut Node<'static>,
    content: *const c_char,
) {
    let Some(n) = (unsafe { node_ref(node) }) else { return };
    let Some(s) = (unsafe { cstr_to_str(content) }) else { return };
    if s.is_empty() { return; }
    let doc_ptr = n.doc.get() as *mut XmlDoc;
    let Some(d) = (unsafe { doc_ref(doc_ptr) }) else { return };
    match n.kind {
        NodeKind::Element => {
            // Append a new text child.  If the last existing child is
            // a Text node, libxml2 concatenates onto it; we follow
            // suit so consumers see one merged text node.
            if let Some(last) = n.last_child.get() {
                if matches!(last.kind, NodeKind::Text) {
                    let combined = format!("{}{}", last.content(), s);
                    write_content_inplace(last, d, &combined);
                    return;
                }
            }
            let txt: &Node<'_> = d.new_text(d.bump().alloc_str(s));
            unsafe { stamp_doc_one(txt, doc_ptr); }
            d.append_child(n, txt);
        }
        NodeKind::Text | NodeKind::CData | NodeKind::Comment | NodeKind::Pi => {
            let combined = format!("{}{}", n.content(), s);
            write_content_inplace(n, d, &combined);
        }
        _ => {}
    }
}

/// libxml2 `xmlTextConcat(node, content, len)` — append `len` bytes
/// from `content` to a text-ish node's content.  Returns 0 on
/// success, -1 if `node` is NULL, isn't a text/cdata/comment node, or
/// the inputs are invalid.
///
/// This is the length-bounded sibling of [`xmlNodeAddContent`]; SAX
/// drivers building text incrementally from a chunked input stream
/// reach for it because the source buffer isn't NUL-terminated.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextConcat(
    node:    *mut Node<'static>,
    content: *const c_char,
    len:     c_int,
) -> c_int {
    let Some(n) = (unsafe { node_ref(node) }) else { return -1 };
    if !matches!(n.kind, NodeKind::Text | NodeKind::CData | NodeKind::Comment) {
        return -1;
    }
    if content.is_null() || len <= 0 {
        // libxml2 returns 0 for the zero-length / NULL case (nothing
        // to do, but not an error).
        return 0;
    }
    // SAFETY: caller asserts content has at least `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(content as *const u8, len as usize) };
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    let doc_ptr = n.doc.get() as *mut XmlDoc;
    let Some(d) = (unsafe { doc_ref(doc_ptr) }) else { return -1 };
    let combined = format!("{}{}", n.content(), s);
    write_content_inplace(n, d, &combined);
    0
}

/// Replace `node.content` with a fresh NUL-terminated slice in
/// `doc`'s arena.  Mirrors the `Cell<ArenaCStr>::set` pattern used by
/// [`xmlNodeSetContent`] — sidesteps `Node::set_content`'s
/// requirement of a `&DocumentBuilder` (which we can't get from a
/// built `Document`).
fn write_content_inplace(
    n: &Node<'_>,
    d: &sup_xml_tree::dom::Document,
    s: &str,
) {
    let bump = d.bump();
    let bytes = s.as_bytes();
    let dst: &mut [u8] = bump.alloc_slice_fill_with(bytes.len() + 1, |i| {
        if i < bytes.len() { bytes[i] } else { 0 }
    });
    // SAFETY: `dst` is a NUL-terminated UTF-8 slice owned by the
    // bump arena; ArenaCStr::from_raw requires exactly that.
    unsafe {
        let new_content = sup_xml_tree::dom::ArenaCStr::from_raw(dst.as_ptr());
        n.content.set(Some(new_content));
    }
}

/// libxml2 `xmlFreeNs` — same story (arena-allocated; logical no-op
/// after detach).  We don't track Namespace parentage so this is a
/// pure no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeNs(
    _ns: *mut sup_xml_tree::dom::Namespace<'static>,
) {
}

/// libxml2 `xmlFreeNsList` — free a chain of `xmlNs` records.  Same
/// no-op semantics as `xmlFreeNs`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeNsList(
    _ns: *mut sup_xml_tree::dom::Namespace<'static>,
) {
}

/// libxml2 `xmlFreePropList` — free an attribute chain.  No-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreePropList(
    _attr: *mut Attribute<'static>,
) {
}

// ── internal helpers ──────────────────────────────────────────────────────

fn detach_node<'a>(n: &'a Node<'a>) {
    // Mirrors libxml2's xmlUnlinkNode: only rewrite parent.first_child /
    // last_child when they actually point at `n`.  lxml's serializer
    // path writes `copy.parent = real.parent` on a detached copy that
    // never entered the children list — an unconditional clear would
    // wipe the real children when xmlFreeNode runs on that copy.
    if let Some(parent) = n.parent.get() {
        let n_ptr = n as *const Node<'_>;
        let prev = n.prev_sibling.get();
        let next = n.next_sibling.get();
        if let Some(p) = prev { p.next_sibling.set(next); }
        if let Some(nx) = next { nx.prev_sibling.set(prev); }
        if parent.first_child.get().map(|c| c as *const _) == Some(n_ptr) {
            parent.first_child.set(next);
        }
        if parent.last_child.get().map(|c| c as *const _) == Some(n_ptr) {
            parent.last_child.set(prev);
        }
    }
    n.parent.set(None);
    n.prev_sibling.set(None);
    n.next_sibling.set(None);
}

fn link_child_manual<'a>(parent: &'a Node<'a>, child: &'a Node<'a>) {
    child.parent.set(Some(parent));
    match parent.last_child.get() {
        None => {
            parent.first_child.set(Some(child));
            parent.last_child.set(Some(child));
        }
        Some(last) => {
            last.next_sibling.set(Some(child));
            child.prev_sibling.set(Some(last));
            parent.last_child.set(Some(child));
        }
    }
}

// Silence unused warnings for record_last_error / ErrorCode — they're
// reserved for future error-path use in this module.
#[allow(dead_code)]
fn _stub_error_use() {
    let _: fn(&XmlError) = record_last_error;
    let _ = ErrorCode::Ok;
    let _ = ErrorDomain::Tree;
    let _ = ErrorLevel::Error;
    let _ = NodeKind::Element;
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::c_int;

    use crate::parse::{xmlDocGetRootElement, xmlFreeDoc, xmlReadMemory};
    use crate::serialize::xmlDocDumpMemory;

    fn parse(src: &[u8]) -> *mut XmlDoc {
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null());
        doc
    }

    fn cs(s: &str) -> CString { CString::new(s).unwrap() }

    fn dump(doc: *mut XmlDoc) -> String {
        let mut mem: *mut c_char = ptr::null_mut();
        let mut size: c_int = 0;
        unsafe { xmlDocDumpMemory(doc, &mut mem, &mut size); }
        let s = unsafe { CStr::from_ptr(mem) }.to_str().unwrap().to_string();
        unsafe { crate::parse::xml_free_impl(mem as *mut _); }
        s
    }

    #[test]
    fn new_doc_node_appends() {
        let doc = parse(b"<r/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let name = cs("child");
        let child = unsafe { xmlNewDocNode(doc, ptr::null_mut(), name.as_ptr(), ptr::null()) };
        assert!(!child.is_null());
        let attached = unsafe { xmlAddChild(root, child) };
        assert_eq!(attached, child);

        let out = dump(doc);
        assert!(out.contains("<child"), "missing child in: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn set_prop_creates_then_updates() {
        let doc = parse(b"<r/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let name = cs("id");
        let v1 = cs("alpha");
        let v2 = cs("beta");
        let attr1 = unsafe { xmlSetProp(root, name.as_ptr(), v1.as_ptr()) };
        assert!(!attr1.is_null());
        // Update — second call replaces the value.  Our implementation
        // removes-and-readds (libxml2 mutates in place); pointer identity
        // is not preserved but consumer semantics ("read it back, get
        // new value") match.
        let attr2 = unsafe { xmlSetProp(root, name.as_ptr(), v2.as_ptr()) };
        assert!(!attr2.is_null());
        // Verify via dump.
        let out = dump(doc);
        assert!(out.contains("id=\"beta\""), "got: {out}");
        assert!(!out.contains("id=\"alpha\""),
                "old value should be gone: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn new_doc_text_with_content() {
        let doc = parse(b"<r/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let text = cs("hello world");
        let t = unsafe { xmlNewDocText(doc, text.as_ptr()) };
        assert!(!t.is_null());
        unsafe { xmlAddChild(root, t); }
        let out = dump(doc);
        assert!(out.contains("hello world"), "got: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn new_doc_node_with_inline_content() {
        let doc = parse(b"<r/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let name = cs("p");
        let txt = cs("inline");
        let p = unsafe { xmlNewDocNode(doc, ptr::null_mut(), name.as_ptr(), txt.as_ptr()) };
        unsafe { xmlAddChild(root, p); }
        let out = dump(doc);
        assert!(out.contains("<p>inline</p>") || out.contains("<p>inline"),
                "expected <p>inline, got: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn unlink_node_detaches() {
        let doc = parse(b"<r><a/><b/><c/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { crate::tree::xmlFirstElementChild(root) };
        unsafe { xmlUnlinkNode(a); }
        let out = dump(doc);
        assert!(!out.contains("<a"), "a should be gone: {out}");
        assert!(out.contains("<b"), "b should remain: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn free_node_with_borrowed_parent_pointer_preserves_real_children() {
        // lxml's serializer (_writeNodeToBuffer) creates a detached
        // shallow copy of the node being serialized, then directly
        // writes copy.parent = real.parent, copy.children = real.children
        // for namespace-decl purposes — without ever inserting copy into
        // real.parent's children list.  When xmlFreeNode runs on the
        // copy afterward, detach_node sees parent = <real_parent> but
        // prev/next = NULL.  If we blindly cleared parent.first_child /
        // last_child in that case, we'd wipe out the real children
        // chain — turning <a><b/><c/></a> into <a/> after serializing
        // an ElementTree wrapped around <b>.
        let doc = parse(b"<r><a><b/><c/></a></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { crate::tree::xmlFirstElementChild(root) };
        let b = unsafe { crate::tree::xmlFirstElementChild(a) };
        // Shallow copy of <b> (recursive=2 mirrors lxml's call).
        let copy_b = unsafe { xmlCopyNode(b, 2) };
        // Forge the parent link the way lxml's serializer does — raw
        // pointer write, no children-list insertion.
        unsafe {
            let copy_ref: &Node<'_> = &*(copy_b as *const Node<'_>);
            let a_ref: &Node<'_> = &*(a as *const Node<'_>);
            copy_ref.parent.set(Some(a_ref));
        }
        // Now free it — the moral equivalent of lxml's xmlFreeNode.
        unsafe { xmlFreeNode(copy_b); }
        // The real <b> and <c> must still be in <a>.
        let out = dump(doc);
        assert!(out.contains("<b") && out.contains("<c"),
                "real children of <a> were lost: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn replace_node_swaps() {
        let doc = parse(b"<r><a id=\"1\"/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { crate::tree::xmlFirstElementChild(root) };
        let name = cs("b");
        let b = unsafe { xmlNewDocNode(doc, ptr::null_mut(), name.as_ptr(), ptr::null()) };
        unsafe { xmlReplaceNode(a, b); }
        let out = dump(doc);
        assert!(out.contains("<b"), "b should be in tree: {out}");
        assert!(!out.contains("<a "), "a should be gone: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn null_safety() {
        assert!(unsafe { xmlNewDocNode(ptr::null_mut(), ptr::null_mut(), ptr::null(), ptr::null()) }.is_null());
        assert!(unsafe { xmlNewDocText(ptr::null_mut(), ptr::null()) }.is_null());
        assert!(unsafe { xmlAddChild(ptr::null_mut(), ptr::null_mut()) }.is_null());
        unsafe { xmlUnlinkNode(ptr::null_mut()); }
        unsafe { xmlFreeNode(ptr::null_mut()); }
    }

    #[test]
    fn copy_node_deep() {
        let doc = parse(b"<r><a id=\"1\"><inner>text</inner></a><b/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { crate::tree::xmlFirstElementChild(root) };
        // Deep-copy a + descendants.
        let copy = unsafe { xmlCopyNode(a, 1) };
        assert!(!copy.is_null());
        // Attach the copy as a child of root.
        unsafe { xmlAddChild(root, copy); }
        let out = dump(doc);
        // Should now have TWO <a> elements with id=1.
        assert_eq!(out.matches("<a id=\"1\">").count(), 2, "got: {out}");
        // Both should contain <inner>text</inner>.
        assert_eq!(out.matches("<inner>text</inner>").count(), 2, "got: {out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn doc_copy_node_into_new_doc() {
        let src = parse(b"<r><greeting>hello</greeting></r>");
        let src_root = unsafe { xmlDocGetRootElement(src) };
        let greet = unsafe { crate::tree::xmlFirstElementChild(src_root) };

        let new_doc = unsafe { xmlNewDoc(ptr::null()) };
        let copy = unsafe { xmlDocCopyNode(greet, new_doc, 1) };
        let _ = unsafe { xmlDocSetRootElement(new_doc, copy) };

        let out = dump(new_doc);
        assert!(out.contains("<greeting>hello</greeting>"), "got: {out}");
        unsafe {
            xmlFreeDoc(src);
            xmlFreeDoc(new_doc);
        }
    }

    #[test]
    fn new_doc_then_set_root_round_trip() {
        // Build an empty doc, allocate a root element in it, install it.
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        assert!(!doc.is_null());
        let name = cs("greeting");
        let root = unsafe { xmlNewDocNode(doc, ptr::null_mut(), name.as_ptr(), ptr::null()) };
        assert!(!root.is_null());
        let old = unsafe { xmlDocSetRootElement(doc, root) };
        assert!(old.is_null(), "fresh doc should have no prior root");

        // Verify by serializing.
        let out = dump(doc);
        assert!(out.contains("<greeting"), "got: {out}");

        // Set a different root — should detach the old.
        let n2 = cs("replacement");
        let root2 = unsafe { xmlNewDocNode(doc, ptr::null_mut(), n2.as_ptr(), ptr::null()) };
        let old2 = unsafe { xmlDocSetRootElement(doc, root2) };
        assert_eq!(old2, root, "set should return the previous root");
        let out2 = dump(doc);
        assert!(out2.contains("<replacement"), "got: {out2}");
        assert!(!out2.contains("<greeting"), "old root should be gone: {out2}");

        unsafe { xmlFreeDoc(doc); }
    }

    // ── tests for the xmlNew* helper family ──────────────────────────────

    #[test]
    fn xml_new_child_appends_to_parent() {
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        let pname = cs("parent");
        let parent = unsafe { xmlNewDocNode(doc, ptr::null_mut(), pname.as_ptr(), ptr::null()) };
        let _ = unsafe { xmlDocSetRootElement(doc, parent) };

        let cname = cs("child");
        let cval  = cs("hi");
        let child = unsafe { xmlNewChild(parent, ptr::null_mut(), cname.as_ptr(), cval.as_ptr()) };
        assert!(!child.is_null());
        // Parent now has one child element which itself has one text child.
        let p = unsafe { &*parent };
        let first = p.first_child.get().expect("parent should have a child");
        assert_eq!(first as *const _ as *const Node<'_>, child as *const Node<'_>);
        let text = first.first_child.get().expect("child should have text");
        assert_eq!(text.content(), "hi");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_new_text_child_synonymous_with_new_child() {
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        let pname = cs("parent");
        let parent = unsafe { xmlNewDocNode(doc, ptr::null_mut(), pname.as_ptr(), ptr::null()) };
        let _ = unsafe { xmlDocSetRootElement(doc, parent) };

        let cname = cs("child");
        let cval  = cs("yo");
        let child = unsafe { xmlNewTextChild(parent, ptr::null_mut(), cname.as_ptr(), cval.as_ptr()) };
        assert!(!child.is_null());
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_new_text_creates_detached_text() {
        let content = cs("free-floating");
        let n = unsafe { xmlNewText(content.as_ptr()) };
        assert!(!n.is_null());
        let n_ref = unsafe { &*n };
        assert_eq!(n_ref.content(), "free-floating");
        // Text node has no parent until grafted.
        assert!(n_ref.parent.get().is_none());
    }

    #[test]
    fn xml_new_text_len_truncates_to_explicit_length() {
        let content = b"hello world\0";
        let n = unsafe { xmlNewTextLen(content.as_ptr() as *const c_char, 5) };
        assert!(!n.is_null());
        let n_ref = unsafe { &*n };
        assert_eq!(n_ref.content(), "hello");
    }

    #[test]
    fn xml_new_comment_creates_detached_comment() {
        let content = cs("commenting");
        let n = unsafe { xmlNewComment(content.as_ptr()) };
        assert!(!n.is_null());
        let n_ref = unsafe { &*n };
        assert_eq!(n_ref.content(), "commenting");
        assert!(matches!(n_ref.kind, sup_xml_tree::dom::NodeKind::Comment));
    }

    #[test]
    fn xml_new_doc_prop_creates_attribute() {
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        let name = cs("id");
        let value = cs("42");
        let attr = unsafe { xmlNewDocProp(doc, name.as_ptr(), value.as_ptr()) };
        assert!(!attr.is_null());
        let a = unsafe { &*attr };
        assert_eq!(a.name(), "id");
        assert_eq!(a.value(), "42");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_new_doc_node_eat_name_frees_caller_name() {
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        // Caller hands us a name allocated via xmlStrdup-style.
        use crate::alloc::alloc_registered_cstring;
        let caller_name = alloc_registered_cstring(b"renamed");
        let n = unsafe { xmlNewDocNodeEatName(doc, ptr::null_mut(), caller_name, ptr::null()) };
        assert!(!n.is_null());
        // We can't easily test that the name pointer was freed
        // without UB, but the node should hold a copy.
        let n_ref = unsafe { &*n };
        assert_eq!(n_ref.name(), "renamed");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_new_doc_raw_node_synonymous_with_doc_node() {
        let doc = unsafe { xmlNewDoc(ptr::null()) };
        let n = cs("el");
        let c = cs("raw <text>");
        let node = unsafe { xmlNewDocRawNode(doc, ptr::null_mut(), n.as_ptr(), c.as_ptr()) };
        assert!(!node.is_null());
        let n_ref = unsafe { &*node };
        assert_eq!(n_ref.name(), "el");
        // Content was attached as a text child (same as xmlNewDocNode).
        let txt = n_ref.first_child.get().expect("text child");
        assert_eq!(txt.content(), "raw <text>");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_node_set_name_renames_an_element() {
        let doc = parse(b"<r><a/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let child = unsafe { (*root).first_child.get() }.expect("child")
            as *const Node<'static> as *mut Node<'static>;
        let r_ref = unsafe { &*root };
        let c_ref = unsafe { &*child };
        assert_eq!(r_ref.name(), "r");
        assert_eq!(c_ref.name(), "a");

        let new_name = cs("renamed");
        unsafe { xmlNodeSetName(child, new_name.as_ptr()); }
        assert_eq!(c_ref.name(), "renamed",
            "xmlNodeSetName must take effect");
        let new_root_name = cs("newroot");
        unsafe { xmlNodeSetName(root, new_root_name.as_ptr()); }
        assert_eq!(r_ref.name(), "newroot");

        // Serialization reflects the new names.
        let s = dump(doc);
        assert!(s.contains("<newroot>"), "got: {s}");
        assert!(s.contains("<renamed/>"), "got: {s}");

        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_node_set_name_null_inputs_are_silent_noops() {
        let doc = parse(b"<r/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let r_ref = unsafe { &*root };
        // NULL name: no-op, name unchanged.
        unsafe { xmlNodeSetName(root, ptr::null()); }
        assert_eq!(r_ref.name(), "r");
        // NULL node: no-op, no crash.
        let new_name = cs("x");
        unsafe { xmlNodeSetName(ptr::null_mut(), new_name.as_ptr()); }
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_copy_prop_clones_attribute_onto_target() {
        let doc = parse(b"<r><a id=\"42\"/><b/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { (*root).first_child.get() }.unwrap()
            as *const Node<'_> as *mut Node<'static>;
        let b = unsafe { (*a).next_sibling.get() }.unwrap()
            as *const Node<'_> as *mut Node<'static>;
        let id_attr = unsafe { (*a).first_attribute.get() }.unwrap()
            as *const Attribute<'_> as *mut Attribute<'static>;
        let cloned = unsafe { xmlCopyProp(b, id_attr) };
        assert!(!cloned.is_null());
        let out = dump(doc);
        assert!(out.contains("<a id=\"42\""), "{out}");
        assert!(out.contains("<b id=\"42\""), "{out}");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_text_concat_appends_to_text_node() {
        let doc = parse(b"<r>hello</r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let text = unsafe { (*root).first_child.get() }.unwrap()
            as *const Node<'_> as *mut Node<'static>;
        let more = b" world";
        let rc = unsafe {
            xmlTextConcat(text, more.as_ptr() as *const c_char, more.len() as c_int)
        };
        assert_eq!(rc, 0);
        let out = dump(doc);
        assert!(out.contains("hello world"), "{out}");
        // Non-text node returns -1.
        let rc2 = unsafe {
            xmlTextConcat(root, more.as_ptr() as *const c_char, more.len() as c_int)
        };
        assert_eq!(rc2, -1);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_add_sibling_appends_at_tail() {
        let doc = parse(b"<r><a/><b/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { (*root).first_child.get() }.unwrap()
            as *const Node<'_> as *mut Node<'static>;
        let name = cs("z");
        let z = unsafe {
            xmlNewDocNode(doc, ptr::null_mut(), name.as_ptr(), ptr::null())
        };
        let attached = unsafe { xmlAddSibling(a, z) };
        assert!(!attached.is_null());
        let out = dump(doc);
        // z must land at the tail, after b — not between a and b.
        assert!(
            out.find("<a/>").unwrap()
                < out.find("<b/>").unwrap()
                && out.find("<b/>").unwrap() < out.find("<z/>").unwrap(),
            "{out}"
        );
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_doc_compress_mode_round_trips() {
        let doc = parse(b"<r/>");
        // Default is 0.
        assert_eq!(unsafe { xmlGetDocCompressMode(doc) }, 0);
        unsafe { xmlSetDocCompressMode(doc, 5); }
        assert_eq!(unsafe { xmlGetDocCompressMode(doc) }, 5);
        // Out-of-range values clamp.
        unsafe { xmlSetDocCompressMode(doc, 42); }
        assert_eq!(unsafe { xmlGetDocCompressMode(doc) }, 9);
        unsafe { xmlSetDocCompressMode(doc, -3); }
        assert_eq!(unsafe { xmlGetDocCompressMode(doc) }, 0);
        // NULL doc → -1 from getter; setter is no-op.
        assert_eq!(unsafe { xmlGetDocCompressMode(ptr::null()) }, -1);
        unsafe { xmlSetDocCompressMode(ptr::null_mut(), 3); }
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn xml_set_tree_doc_stamps_subtree() {
        // Create two docs; move a subtree's doc pointer from one to the
        // other and verify every descendant + attr gets re-stamped.
        let d1 = unsafe { xmlNewDoc(cs("1.0").as_ptr()) };
        let d2 = unsafe { xmlNewDoc(cs("1.0").as_ptr()) };

        let elem = unsafe {
            xmlNewDocNode(d1, ptr::null_mut(), cs("e").as_ptr(), ptr::null())
        };
        let child = unsafe {
            xmlNewDocNode(d1, ptr::null_mut(), cs("c").as_ptr(), ptr::null())
        };
        unsafe { xmlAddChild(elem, child); }
        unsafe { xmlSetProp(elem, cs("k").as_ptr(), cs("v").as_ptr()); }

        // Pre-stamp: all live in d1.
        assert_eq!(unsafe { (*elem).doc.get() } as *mut XmlDoc, d1);
        assert_eq!(unsafe { (*child).doc.get() } as *mut XmlDoc, d1);

        unsafe { xmlSetTreeDoc(elem, d2); }
        assert_eq!(unsafe { (*elem).doc.get() } as *mut XmlDoc, d2);
        assert_eq!(unsafe { (*child).doc.get() } as *mut XmlDoc, d2);
        let attr = unsafe { (*elem).first_attribute.get() }.unwrap();
        assert_eq!(attr.doc.get() as *mut XmlDoc, d2);

        unsafe { xmlFreeDoc(d1); xmlFreeDoc(d2); }
    }

    #[test]
    fn new_ns_does_not_redeclare_a_bound_prefix() {
        // libxml2's xmlNewNs refuses to add a second declaration for a
        // prefix already bound on the node — it returns the existing ns
        // instead.  lxml relies on this when re-declaring in-scope
        // namespaces on a serialized fragment root: without it, a
        // redeclared default namespace emits two `xmlns="…"` attributes.
        let doc = parse(b"<r xmlns=\"http://first\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let first = unsafe { (*root).ns_def.get() }.unwrap();

        // Re-declaring the default prefix returns the existing binding and
        // leaves a single declaration on the node.
        let href = CString::new("http://second").unwrap();
        let got = unsafe { xmlNewNs(root, href.as_ptr(), ptr::null()) };
        assert_eq!(got as *const _, first as *const _ as *mut _);
        assert!(first.next.get().is_none());
        assert_eq!(first.href(), "http://first");

        // A genuinely new prefix is still appended.
        let p = CString::new("p").unwrap();
        let h = CString::new("http://p").unwrap();
        let pns = unsafe { xmlNewNs(root, h.as_ptr(), p.as_ptr()) };
        assert!(!pns.is_null());
        assert!(first.next.get().is_some());

        unsafe { xmlFreeDoc(doc); }
    }
}
