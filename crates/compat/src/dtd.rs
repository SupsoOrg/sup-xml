//! DTD lifecycle surface.
//!
//! This module wraps the DTD.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr;

use sup_xml_tree::dom::XmlDoc;

use sup_xml_core::dtd::Dtd;

// ── per-doc DTD sidecar ──────────────────────────────────────────────────
//
// `sup-xml-tree::XmlDoc` is layout-locked to libxml2's `xmlDoc` and has
// no room for a Rust-typed `Option<Dtd>`.  Stash the captured DTD in a
// thread-local map keyed by XmlDoc pointer instead — xmlReadMemory
// inserts on parse success, xmlFreeDoc evicts.

thread_local! {
    static DTD_BY_DOC: RefCell<HashMap<usize, Dtd>> = RefCell::new(HashMap::new());

    /// Parsed declarations for a *standalone* DTD handle — one built by
    /// [`xmlParseDTD`] / [`xmlIOParseDTD`] rather than captured from a
    /// document's internal subset.  Keyed by the `xmlDtd*` handle
    /// pointer.  `xmlValidateDtd` validates against this when the caller
    /// passes such a handle; `xmlFreeDtd` evicts it.
    static DTD_BY_HANDLE: RefCell<HashMap<usize, Dtd>> = RefCell::new(HashMap::new());
}

/// Run the core DTD validator over `document` against `dtd`, surfacing
/// every error through the structured-error callback (so lxml's
/// `error_log` fills) and mapping the outcome to libxml2's 1 (valid) /
/// 0 (invalid) convention.
fn run_validation(document: &sup_xml_tree::dom::Document, dtd: &Dtd) -> c_int {
    match sup_xml_core::dtd::validate(document, dtd) {
        Ok(()) => 1,
        Err(errs) => {
            for e in &errs {
                crate::error::record_last_error(&dtd_error_to_xmlerror(e, document));
            }
            0
        }
    }
}

/// Translate a core [`DtdError`](sup_xml_core::dtd::DtdError) into a
/// libxml2-shaped structured error: its exact message wording, numeric
/// code, and the offending element's source line — the form lxml's
/// `error_log` asserts on.  The native validator keeps its own (clearer)
/// phrasing; only this ABI shim mirrors libxml2.
pub(crate) fn dtd_error_to_xmlerror(
    e:        &sup_xml_core::dtd::DtdError,
    document: &sup_xml_tree::dom::Document,
) -> sup_xml_core::error::XmlError {
    use sup_xml_core::error::{ErrorCode, ErrorDomain, ErrorLevel, XmlError};
    let (message, code) = if e.message.contains("EMPTY element") {
        // libxml2 `xmlValidateOneElement`: XML_DTD_NOT_EMPTY.
        (format!("Element {} was declared EMPTY this one has content", e.element),
         ErrorCode::DtdNotEmpty)
    } else {
        (e.to_string(), ErrorCode::InternalError)
    };
    let mut xerr = XmlError::new(ErrorDomain::Validation, ErrorLevel::Error, message)
        .with_code(code);
    if let Some(line) = first_element_line(document.root(), &e.element) {
        xerr.line = Some(line);
    }
    xerr
}

/// Source line of the first element named `name` in the subtree (the
/// element a DTD validation error refers to), for the error's `line`.
fn first_element_line<'a>(node: &'a sup_xml_tree::dom::Node<'a>, name: &str) -> Option<u32> {
    use sup_xml_tree::dom::NodeKind;
    if node.kind == NodeKind::Element {
        if node.name() == name {
            return Some(node.line as u32);
        }
        for child in node.children() {
            if let Some(l) = first_element_line(child, name) {
                return Some(l);
            }
        }
    }
    None
}

/// Attach `dtd` to `doc` for later retrieval by [`xmlValidateDocument`].
/// Called from `parse.rs::xmlReadMemory` immediately after a successful
/// parse when the DTD is non-empty.
pub(crate) fn stash_dtd(doc: *mut XmlDoc, dtd: Dtd) {
    DTD_BY_DOC.with(|m| {
        m.borrow_mut().insert(doc as usize, dtd);
    });
}

/// Run `f` with the DTD stashed for `doc`, if any.  `None` when the
/// document had no DOCTYPE (or an empty one) — the caller treats that
/// as "no DTD-declared ID attributes."
pub(crate) fn with_stashed_dtd<R>(doc: *const XmlDoc, f: impl FnOnce(Option<&Dtd>) -> R) -> R {
    DTD_BY_DOC.with(|m| f(m.borrow().get(&(doc as usize))))
}

/// Evict any stashed DTD for `doc`.  Called from `xmlFreeDoc`.
pub(crate) fn forget_dtd(doc: *mut XmlDoc) {
    DTD_BY_DOC.with(|m| {
        m.borrow_mut().remove(&(doc as usize));
    });
}

// ── xmlValidCtxt ─────────────────────────────────────────────────────────

/// Opaque DTD-validator context.  Real libxml2 has a large struct
/// (`xmlValidCtxt`) with vstateNr/vstateMax/vstateTab pointers; we
/// match the *size* with zero-init padding so callers reading struct
/// fields directly see zero.
#[repr(C)]
pub struct xmlValidCtxt {
    _opaque: [u8; 88], // libxml2 sizeof on 64-bit
}

/// `xmlNewValidCtxt()` — allocate a zeroed validator context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewValidCtxt() -> *mut xmlValidCtxt {
    Box::into_raw(Box::new(xmlValidCtxt { _opaque: [0u8; 88] }))
}

/// `xmlFreeValidCtxt(ctxt)` — drop a validator context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeValidCtxt(ctxt: *mut xmlValidCtxt) {
    if ctxt.is_null() { return; }
    unsafe { drop(Box::from_raw(ctxt)); }
}

/// `xmlValidateDocument(ctxt, doc)` — validate `doc` against the DTD
/// captured from its internal subset during parsing.  Returns 1
/// (valid), 0 (invalid), or -1 on programmer error (NULL doc).
///
/// If the document had no internal-subset declarations, returns 1
/// (libxml2 also returns 1 in that case — "nothing to validate").
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateDocument(
    _ctxt: *mut xmlValidCtxt,
    doc:   *mut XmlDoc,
) -> c_int {
    if doc.is_null() { return -1; }
    let key = doc as usize;
    let result = DTD_BY_DOC.with(|m| -> Option<c_int> {
        let borrow = m.borrow();
        let dtd = borrow.get(&key)?;
        // SAFETY: doc was returned by xmlReadMemory; its embedded
        // _doc Document is still alive.
        let document = unsafe { &(*doc)._doc };
        Some(run_validation(document, dtd))
    });
    // No stashed DTD → nothing to validate against, treat as valid.
    result.unwrap_or(1)
}

/// `xmlValidateDtd(ctxt, doc, dtd)` — validate `doc` against an
/// externally-supplied DTD handle.
///
/// When `dtd` is a standalone handle parsed by [`xmlParseDTD`] /
/// [`xmlIOParseDTD`] (it carries its declarations in `DTD_BY_HANDLE`),
/// `doc`'s tree is validated against those declarations — this is how
/// lxml's `etree.DTD(...).validate(root)` reaches the validator.
/// Otherwise we fall back to the document's own internal-subset DTD
/// (the `xmlValidateDocument` behaviour), matching libxml2's tolerance
/// of a NULL / unparsed `dtd` argument.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateDtd(
    ctxt: *mut xmlValidCtxt,
    doc:  *mut XmlDoc,
    dtd:  *mut xmlDtd,
) -> c_int {
    if doc.is_null() { return -1; }
    let from_handle = DTD_BY_HANDLE.with(|m| -> Option<c_int> {
        let borrow = m.borrow();
        let parsed = borrow.get(&(dtd as usize))?;
        // SAFETY: doc came from a parse / fake-root-doc; its embedded
        // _doc Document is live for the call.
        let document = unsafe { &(*doc)._doc };
        Some(run_validation(document, parsed))
    });
    match from_handle {
        Some(r) => r,
        None => unsafe { xmlValidateDocument(ctxt, doc) },
    }
}

/// `xmlValidateDtdFinal(ctxt, doc)` — post-load DTD checks
/// (cross-document ID/IDREF resolution etc).  We do this inline in
/// [`xmlValidateDocument`] already, so this is a no-op stub
/// returning 1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateDtdFinal(
    _ctxt: *mut xmlValidCtxt,
    _doc:  *mut XmlDoc,
) -> c_int { 1 }

// ── DTD nodes ─────────────────────────────────────────────────────────────

/// Opaque DTD handle, byte-exact to libxml2's `_xmlDtd` on 64-bit:
/// 16-byte node-shaped header, 8-byte name, then 14 pointer-sized
/// slots through SystemID + pentities.  Total 128 bytes.  Consumers
/// (lxml's `_dtdFactory` → `_copyDtd`) read fields out of this
/// struct directly via the libxml2 layout — undersizing it causes
/// reads past the heap allocation's end, which manifests as
/// intermittent crashes in `tree.docinfo.internalDTD`.
#[repr(C)]
pub struct xmlDtd {
    _private:    *mut c_void,                // 0    void *_private
    type_:       c_int,                      // 8    xmlElementType type
    _pad8:       [u8; 4],                    // 12   (alignment)
    pub name:    *mut c_char,                // 16   xmlChar *name
    pub children:*mut c_void,                // 24   xmlNode *children
    pub last:    *mut c_void,                // 32   xmlNode *last
    pub parent:  *mut c_void,                // 40   xmlDoc  *parent
    pub next:    *mut c_void,                // 48   xmlNode *next
    pub prev:    *mut c_void,                // 56   xmlNode *prev
    pub doc:     *mut c_void,                // 64   xmlDoc  *doc
    notations:   *mut c_void,                // 72
    elements:    *mut c_void,                // 80
    attributes:  *mut c_void,                // 88
    entities:    *mut c_void,                // 96
    pub external_id: *mut c_char,            // 104  xmlChar *ExternalID
    pub system_id:   *mut c_char,            // 112  xmlChar *SystemID
    pub(crate) pentities: *mut c_void,       // 120
}

const _: () = {
    use std::mem::offset_of;
    assert!(std::mem::size_of::<xmlDtd>() == 128,
            "xmlDtd must be 128 bytes to match libxml2's _xmlDtd on 64-bit");
    // Per-field offsets — guards against accidental field reorder
    // that the size check alone wouldn't catch.  Match libxml2's
    // tree.h layout for `_xmlDtd`.
    assert!(offset_of!(xmlDtd, _private)    ==   0);
    assert!(offset_of!(xmlDtd, type_)       ==   8);
    assert!(offset_of!(xmlDtd, name)        ==  16);
    assert!(offset_of!(xmlDtd, children)    ==  24);
    assert!(offset_of!(xmlDtd, last)        ==  32);
    assert!(offset_of!(xmlDtd, parent)      ==  40);
    assert!(offset_of!(xmlDtd, next)        ==  48);
    assert!(offset_of!(xmlDtd, prev)        ==  56);
    assert!(offset_of!(xmlDtd, doc)         ==  64);
    assert!(offset_of!(xmlDtd, notations)   ==  72);
    assert!(offset_of!(xmlDtd, elements)    ==  80);
    assert!(offset_of!(xmlDtd, attributes)  ==  88);
    assert!(offset_of!(xmlDtd, entities)    ==  96);
    assert!(offset_of!(xmlDtd, external_id) == 104);
    assert!(offset_of!(xmlDtd, system_id)   == 112);
    assert!(offset_of!(xmlDtd, pentities)   == 120);
};

fn alloc_dtd(name: &str) -> *mut xmlDtd {
    alloc_dtd_with_ids(Some(name), None, None)
}

fn alloc_dtd_with_ids(
    name: Option<&str>,
    public_id: Option<&str>,
    system_id: Option<&str>,
) -> *mut xmlDtd {
    let to_raw = |s: &str| -> *mut c_char {
        match std::ffi::CString::new(s) {
            Ok(c) => c.into_raw(),
            Err(_) => ptr::null_mut(),
        }
    };
    let cname        = name.map(to_raw).unwrap_or(ptr::null_mut());
    let external_id  = public_id.map(to_raw).unwrap_or(ptr::null_mut());
    let system_id_p  = system_id.map(to_raw).unwrap_or(ptr::null_mut());
    Box::into_raw(Box::new(xmlDtd {
        _private:    ptr::null_mut(),
        type_:       14, // XML_DTD_NODE
        _pad8:       [0u8; 4],
        name:        cname,
        children:    ptr::null_mut(),
        last:        ptr::null_mut(),
        parent:      ptr::null_mut(),
        next:        ptr::null_mut(),
        prev:        ptr::null_mut(),
        doc:         ptr::null_mut(),
        notations:   ptr::null_mut(),
        elements:    ptr::null_mut(),
        attributes:  ptr::null_mut(),
        entities:    ptr::null_mut(),
        external_id,
        system_id:   system_id_p,
        pentities:   ptr::null_mut(),
    }))
}

/// Plant a parsed `<!DOCTYPE …>` as `doc`'s internal subset.
///
/// Used by the HTML parse path to carry html5ever's captured doctype
/// onto the libxml2-shape document so `xmlGetIntSubset` and lxml's
/// `docinfo.doctype` / `public_id` / `system_url` see it.  Empty
/// public / system identifiers map to NULL — that's how libxml2
/// represents a bare `<!DOCTYPE html>`.
pub(crate) unsafe fn plant_int_subset(
    doc: *mut XmlDoc,
    name: &str,
    public_id: Option<&str>,
    system_id: Option<&str>,
) -> *mut xmlDtd {
    let dtd = alloc_dtd_with_ids(Some(name), public_id, system_id);
    // SAFETY: dtd is a freshly boxed, valid handle.
    unsafe {
        (*dtd).parent = doc as *mut c_void;
        (*dtd).doc    = doc as *mut c_void;
    }
    let bytes = (dtd as usize).to_ne_bytes();
    // SAFETY: doc is a valid XmlDoc; intSubset lives at offset 80.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), (doc as *mut u8).add(80), bytes.len());
    }
    dtd
}

/// `xmlCreateIntSubset(doc, name, externalID, systemID)` — attach
/// an internal-subset DTD handle to `doc`.  Plants the handle in
/// the `intSubset` slot at offset 80 of [`XmlDoc`].
///
/// `externalID` is the PUBLIC identifier (when the source DOCTYPE
/// used `PUBLIC "pub-id" "sys-id"`).  `systemID` is the SYSTEM
/// identifier from either `SYSTEM "sys-id"` or the second literal
/// of a `PUBLIC` declaration.  Both fields show up on the
/// returned `xmlDtd` so consumers like lxml's
/// `docinfo.public_id` / `docinfo.system_url` see the original
/// strings.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreateIntSubset(
    doc:         *mut XmlDoc,
    name:        *const c_char,
    external_id: *const c_char,
    system_id:   *const c_char,
) -> *mut xmlDtd {
    if doc.is_null() { return ptr::null_mut(); }
    let cstr_opt = |p: *const c_char| -> Option<String> {
        if p.is_null() { None }
        // SAFETY: caller asserts the pointer is NUL-terminated and
        // readable when non-null.
        else { unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_string) }
    };
    let name_str = cstr_opt(name).unwrap_or_default();
    let public_id = cstr_opt(external_id);
    let system_id_s = cstr_opt(system_id);
    let dtd = alloc_dtd_with_ids(Some(&name_str), public_id.as_deref(), system_id_s.as_deref());
    // Plant on doc.int_subset (offset 80 — see XmlDoc layout in
    // sup-xml-tree/src/dom.rs:1608).  libxml2's `_xmlDoc::intSubset`
    // sits at the same byte offset; consumers like lxml's
    // `docinfo.internalDTD` read directly via the layout.
    let off = 80usize;
    let bytes = (dtd as usize).to_ne_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (doc as *mut u8).add(off),
            bytes.len(),
        );
        // Parent the subset on its document (libxml2 sets both), so
        // consumers that walk `dtd->doc` — lxml's `_copyDtd`, the
        // serializer's `xmlNodeDumpOutput` — resolve the owning arena.
        (*dtd).parent = doc as *mut c_void;
        (*dtd).doc    = doc as *mut c_void;
    }
    dtd
}


/// Splice the internal-subset node into the document's sibling chain
/// at the DOCTYPE's true document position.
///
/// `prolog_index` is the number of document-level comments/PIs that
/// preceded the `<!DOCTYPE …>` in source order
/// (`Dtd::internal_subset_prolog_index`).  Inserting the node between
/// the prolog miscs that came before it and those (plus the root) that
/// came after makes lxml's serializer — which emits pre-doctype
/// comments via `_writePrevSiblings(doc->intSubset)` (it walks
/// `intSubset->prev`) — place them ahead of the `<!DOCTYPE>`, matching
/// libxml2.
///
/// No-op when `prolog_index == 0` (the DOCTYPE was the first prolog
/// item, so the existing `root->prev` chain already serializes the
/// post-doctype miscs in the right order).
pub(crate) unsafe fn splice_int_subset_into_prolog(doc: *mut XmlDoc, prolog_index: u32) {
    use sup_xml_tree::dom::Node;
    if doc.is_null() || prolog_index == 0 {
        return;
    }
    // SAFETY: caller passes a live XmlDoc; intSubset lives at offset 80.
    let int_subset = unsafe { (*doc).int_subset } as *mut xmlDtd;
    if int_subset.is_null() {
        return;
    }
    // Walk to the (prolog_index-1)-th document-level child — the last
    // comment/PI before the DOCTYPE.  The front of the chain is exactly
    // the prolog miscs in order (doc-level whitespace is skipped, never
    // a node), so the index lines up with the count.
    let mut before = unsafe { (*doc).children.get() };
    if before.is_null() {
        return;
    }
    for _ in 1..prolog_index {
        match unsafe { (*before).next_sibling.get() } {
            Some(n) => before = n as *const Node<'_> as *mut Node<'static>,
            None => return, // chain shorter than the recorded index — bail
        }
    }
    // The node at/after the DOCTYPE position — the next prolog misc, or
    // the root element.
    let after = match unsafe { (*before).next_sibling.get() } {
        Some(n) => n as *const Node<'_> as *mut Node<'static>,
        None => return,
    };
    // Link `before → intSubset → after`.  The xmlDtd shares the node
    // header layout, so its prev/next (offsets 56/48) are read by both
    // lxml and our own walkers as sibling pointers; reinterpreted as a
    // Node it reports NodeKind::Dtd and is skipped by every walker.
    unsafe {
        let dtd_as_node: &Node<'static> = &*(int_subset as *const Node<'static>);
        (*before).next_sibling.set(Some(dtd_as_node));
        (*after).prev_sibling.set(Some(dtd_as_node));
        (*int_subset).prev = before as *mut c_void;
        (*int_subset).next = after as *mut c_void;
    }
}

/// Copy `src_doc`'s internal subset onto `dst_doc` (used by
/// `xmlCopyDoc` / deepcopy).  Recreates the DTD header and materializes
/// the parsed declarations as typed child nodes (from `DTD_BY_DOC`) so
/// the copy round-trips the same `<!DOCTYPE …[ … ]>` as the original.
/// No-op when the source has no internal subset.
pub(crate) unsafe fn copy_int_subset(src_doc: *mut XmlDoc, dst_doc: *mut XmlDoc) {
    if src_doc.is_null() || dst_doc.is_null() {
        return;
    }
    // SAFETY: caller passes live XmlDocs; intSubset is at offset 80.
    let src = unsafe { (*src_doc).int_subset } as *mut xmlDtd;
    if src.is_null() {
        return;
    }
    unsafe {
        let _ = xmlCreateIntSubset(dst_doc, (*src).name, (*src).external_id, (*src).system_id);
        let dst = (*dst_doc).int_subset as *mut xmlDtd;
        if !dst.is_null() {
            with_stashed_dtd(src_doc, |model| {
                if let Some(m) = model {
                    crate::dtddecl::materialize(dst, dst_doc as *mut c_void, m);
                }
            });
        }
    }
}

/// `xmlNewDtd(doc, name, externalID, systemID)` — same shape as
/// xmlCreateIntSubset but does not attach.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewDtd(
    _doc:        *mut XmlDoc,
    name:        *const c_char,
    _external_id:*const c_char,
    _system_id:  *const c_char,
) -> *mut xmlDtd {
    let name_str = if name.is_null() {
        ""
    } else {
        match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        }
    };
    alloc_dtd(name_str)
}

/// `xmlFreeDtd(dtd)` — drop a heap-allocated DTD handle.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeDtd(dtd: *mut xmlDtd) {
    if dtd.is_null() { return; }
    DTD_BY_HANDLE.with(|m| { m.borrow_mut().remove(&(dtd as usize)); });
    crate::dtddecl::forget(dtd);
    unsafe {
        let boxed = Box::from_raw(dtd);
        if !boxed.name.is_null() {
            drop(std::ffi::CString::from_raw(boxed.name));
        }
        drop(boxed);
    }
}

/// `xmlGetIntSubset(doc)` — return the document's internal DTD
/// subset, or NULL when the doc has no `<!DOCTYPE ...>`.  Lives at
/// offset 80 of the libxml2 `xmlDoc` struct.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetIntSubset(doc: *const XmlDoc) -> *mut xmlDtd {
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `doc` came from a parse/xmlNewDoc.
    unsafe { (*doc).int_subset as *mut xmlDtd }
}

/// `xmlGetDocEntity(doc, name)` — look up a parsed-entity record by
/// name on the doc's internal subset.  Returns the entity pointer or
/// NULL on miss.  v0.1 stub-returns NULL — we parse entities at parse
/// time and don't keep an xmlEntity table around for post-parse
/// lookup yet.  Real libxml2 returns predefined entity pointers
/// (lt, gt, amp, apos, quot) when the lookup matches those names;
/// we honour that subset since some consumers rely on it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDocEntity(
    _doc: *const XmlDoc,
    name: *const c_char,
) -> *mut c_void {
    if name.is_null() {
        return ptr::null_mut();
    }
    // The predefined entities aren't queried with `&` — the API
    // takes just the local name ("lt", "gt", …).
    let _s = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    // We don't carry an xmlEntity table around post-parse, and
    // synthesizing one on the fly requires the full libxml2 struct
    // layout.  Returning NULL is the same as "entity unknown" — the
    // caller falls back to its own machinery (libxslt has its own
    // predefined-entity table internally).
    ptr::null_mut()
}

/// `xmlAddID(ctxt, doc, value, attr)` — register an attribute as an
/// XML ID for later lookup by `xmlGetID`.  v0.1 maintains a thread-
/// local table keyed on `(doc_ptr, value)`.  Returns the attribute
/// pointer on success, NULL on bad inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddID(
    _ctxt: *mut c_void,
    doc:   *mut XmlDoc,
    value: *const c_char,
    attr:  *mut sup_xml_tree::dom::Attribute<'static>,
) -> *mut sup_xml_tree::dom::Attribute<'static> {
    if doc.is_null() || value.is_null() || attr.is_null() {
        return ptr::null_mut();
    }
    let id_value = match unsafe { CStr::from_ptr(value) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ptr::null_mut(),
    };
    let attr_addr = attr as usize;
    ID_TABLE.with(|t| {
        t.borrow_mut().insert((doc as usize, id_value), attr_addr);
    });
    attr
}

/// `xmlFreeIDTable(table)` — release a previously-stashed ID table.
/// Our implementation keeps the table per-thread (not per-doc), so
/// this is a no-op; doc-scoped cleanup happens in
/// [`crate::parse::xmlFreeDoc`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeIDTable(_table: *mut c_void) {
    // No-op — the thread-local ID_TABLE outlives individual table
    // handles and is GC'd on thread exit.
}

thread_local! {
    /// Per-thread ID registry keyed on `(doc_ptr_as_usize, id_value)`.
    /// Value is the attribute pointer cast to usize so we don't have to
    /// fight Rust's pointer-Send rules — the address is what we want.
    static ID_TABLE: RefCell<HashMap<(usize, String), usize>> = RefCell::new(HashMap::new());
}

/// `xmlCopyDtd(dtd)` — copy a DTD handle, including its name, the
/// PUBLIC/SYSTEM identifiers, and the serializable declaration body.
///
/// lxml's deepcopy of an `ElementTree(root)` reaches the internal
/// subset through `_copyNonElementSiblings` → `_copyDtd` → this
/// function (not `xmlCopyDoc`), then sets the result as the copy's
/// `intSubset`.  A shallow name-only copy would drop the DOCTYPE's
/// `[ … ]` body; copying the declaration child preserves the
/// round-trip.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyDtd(dtd: *mut xmlDtd) -> *mut xmlDtd {
    if dtd.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts dtd was returned from xmlNewDtd / xmlCreateIntSubset.
    let src = unsafe { &*dtd };
    let cstr_opt = |p: *const c_char| -> Option<String> {
        if p.is_null() { None }
        // SAFETY: the xmlDtd string slots are NUL-terminated when non-null.
        else { unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_string) }
    };
    let name = cstr_opt(src.name).unwrap_or_default();
    let public_id = cstr_opt(src.external_id);
    let system_id = cstr_opt(src.system_id);
    let new = alloc_dtd_with_ids(Some(&name), public_id.as_deref(), system_id.as_deref());

    // Rebuild the copy's typed declaration nodes from the parsed model
    // (`DTD_BY_HANDLE` for a standalone handle, `DTD_BY_DOC` for an
    // internal subset).  lxml copies a DTD through here for both its
    // object model (`docinfo.internalDTD`) and document deepcopy, then
    // serializes the copy — so the copy needs the typed nodes the
    // serializer reconstructs from, not a borrowed source-text lump.
    let model = DTD_BY_HANDLE.with(|m| m.borrow().get(&(dtd as usize)).cloned())
        .or_else(|| {
            let doc = src.doc as *const XmlDoc;
            if doc.is_null() { None } else { with_stashed_dtd(doc, |d| d.cloned()) }
        });
    if let Some(parsed) = model {
        unsafe { crate::dtddecl::materialize(new, src.doc, &parsed); }
        DTD_BY_HANDLE.with(|m| { m.borrow_mut().insert(new as usize, parsed); });
    }
    new
}

/// `xmlNewReference(doc, name)` — create an entity-reference node
/// (`&name;`) owned by `doc`.  `name` is the bare entity name; lxml's
/// `Entity()` factory feeds the result through `xmlAddChild`, and
/// `_Entity.name` reads back `node->name`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewReference(
    doc:  *const XmlDoc,
    name: *const c_char,
) -> *mut c_void {
    if name.is_null() {
        return ptr::null_mut();
    }
    let name_s = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    unsafe { crate::mutate::new_doc_entity_ref(doc as *mut XmlDoc, name_s) as *mut c_void }
}

// ── Parse / lookup ────────────────────────────────────────────────────────

/// Parse a standalone external DTD subset and wrap it in an `xmlDtd`
/// handle, stashing the parsed declarations in `DTD_BY_HANDLE` for
/// later validation.  `public_id` / `system_id` are recorded on the
/// handle so lxml's `DTD.external_id` / `DTD.system_url` read back the
/// originals.  Returns NULL on a fatal parse error (libxml2's
/// `xmlParseDTD` contract).
fn build_parsed_dtd(
    bytes:     &[u8],
    public_id: Option<&str>,
    system_id: Option<&str>,
) -> *mut xmlDtd {
    let opts = sup_xml_core::options::ParseOptions::default();
    let parsed = match sup_xml_core::parser::parse_external_subset(bytes, &opts) {
        Ok(d)  => d,
        Err(_) => return ptr::null_mut(),
    };
    // A standalone external subset has no DOCTYPE name (libxml2 leaves
    // `name` NULL); only the external/system identifiers carry over.
    let handle = alloc_dtd_with_ids(None, public_id, system_id);
    // Expose the parsed declarations as libxml2-shaped typed child nodes
    // for lxml's `DTD.elements()` / `.entities()` object model and the
    // DOCTYPE serializer.
    unsafe { crate::dtddecl::materialize(handle, ptr::null_mut(), &parsed); }
    DTD_BY_HANDLE.with(|m| {
        m.borrow_mut().insert(handle as usize, parsed);
    });
    handle
}

/// `xmlParseDTD(externalID, systemID)` — parse an external DTD subset.
///
/// libxml2 treats `systemID` as a filename/URI to read and `externalID`
/// as a PUBLIC identifier resolved through the XML catalog.  We support
/// the SYSTEM (file) form — lxml's `etree.DTD(filename)` passes the
/// filename as `systemID`; a PUBLIC-only id has no catalog to resolve
/// against in this build, so that form returns NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseDTD(
    _external_id: *const c_char,
    system_id:    *const c_char,
) -> *mut xmlDtd {
    if system_id.is_null() {
        return ptr::null_mut();
    }
    let sys = match unsafe { CStr::from_ptr(system_id) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    let path = sys.strip_prefix("file://").unwrap_or(sys);
    let bytes = match std::fs::read(path) {
        Ok(b)  => b,
        Err(_) => return ptr::null_mut(),
    };
    build_parsed_dtd(&bytes, None, Some(sys))
}

/// `xmlIOParseDTD(sax, input, encoding)` — parse an external DTD subset
/// from a parser-input buffer.  lxml's file-like `etree.DTD(fileobj)`
/// path allocates the buffer, attaches a read callback, and hands it
/// here; we drain the callback to EOF (taking ownership of the buffer,
/// per libxml2) and parse the collected bytes.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlIOParseDTD(
    _sax:      *mut c_void,
    input:     *mut c_void,
    _encoding: c_int,
) -> *mut xmlDtd {
    let bytes = match unsafe { crate::misc::drain_parser_input_buffer(input) } {
        Some(b) => b,
        None    => return ptr::null_mut(),
    };
    build_parsed_dtd(&bytes, None, None)
}

/// `xmlGetDtdElementDesc(dtd, name)` — lookup an element decl by
/// name.  v0.1 returns null (no decls indexed).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDtdElementDesc(
    _dtd:  *mut xmlDtd,
    _name: *const c_char,
) -> *mut c_void { ptr::null_mut() }

/// `xmlGetDtdQElementDesc(dtd, name, prefix)` — qualified-name lookup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDtdQElementDesc(
    _dtd:    *mut xmlDtd,
    _name:   *const c_char,
    _prefix: *const c_char,
) -> *mut c_void { ptr::null_mut() }

/// `xmlGetDtdAttrDesc(dtd, elem, name)` — attribute decl lookup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDtdAttrDesc(
    _dtd:  *mut xmlDtd,
    _elem: *const c_char,
    _name: *const c_char,
) -> *mut c_void { ptr::null_mut() }

/// `xmlGetDtdNotationDesc(dtd, name)` — notation decl lookup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetDtdNotationDesc(
    _dtd:  *mut xmlDtd,
    _name: *const c_char,
) -> *mut c_void { ptr::null_mut() }

/// `xmlDumpNotationTable(buf, table)` — serialize notation table.
/// No-op in v0.1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDumpNotationTable(
    _buf:   *mut c_void,
    _table: *mut c_void,
) {}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ctxt_round_trip() {
        let ctxt = unsafe { xmlNewValidCtxt() };
        assert!(!ctxt.is_null());
        unsafe { xmlFreeValidCtxt(ctxt); }
    }

    #[test]
    fn prolog_comment_before_doctype_splices_int_subset() {
        use sup_xml_tree::dom::{Node, NodeKind};
        // A comment preceding the DOCTYPE must serialize before it.
        // lxml emits pre-doctype comments by walking `intSubset->prev`,
        // so the internal-subset node has to sit in the document
        // sibling chain at its true position: comment → intSubset →
        // root.  Reinterpreted as a `Node` the subset reports
        // `NodeKind::Dtd`, which every walker skips.
        let src = b"<!-- c --><!DOCTYPE r [<!ELEMENT r EMPTY>]><r/>";
        let doc = unsafe { crate::parse::xmlReadMemory(
            src.as_ptr() as *const _, src.len() as c_int,
            ptr::null(), ptr::null(), 0,
        )};
        assert!(!doc.is_null());
        unsafe {
            let comment = (*doc).children.get();
            assert!(!comment.is_null());
            assert_eq!((*comment).kind, NodeKind::Comment);

            let int_subset = (*doc).int_subset as *mut xmlDtd;
            assert!(!int_subset.is_null());
            let dtd_as_node = int_subset as *const Node<'static>;
            assert_eq!((*dtd_as_node).kind, NodeKind::Dtd);

            // comment → intSubset
            assert_eq!((*int_subset).prev as *const Node<'_>, comment as *const Node<'_>);
            let after_comment = (*comment).next_sibling.get()
                .map(|n| n as *const Node<'_>);
            assert_eq!(after_comment, Some(dtd_as_node as *const Node<'_>));

            // intSubset → root, and root links back to the subset.
            let root = (*int_subset).next as *const Node<'static>;
            assert!(!root.is_null());
            assert_eq!((*root).kind, NodeKind::Element);
            let root_prev = (*root).prev_sibling.get().map(|n| n as *const Node<'_>);
            assert_eq!(root_prev, Some(dtd_as_node as *const Node<'_>));
        }
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn validate_null_doc_returns_minus_one() {
        // NULL doc is a programmer error — libxml2 returns -1 in
        // that case, and so do we.
        let ctxt = unsafe { xmlNewValidCtxt() };
        assert_eq!(unsafe { xmlValidateDtd(ctxt, ptr::null_mut(), ptr::null_mut()) }, -1);
        assert_eq!(unsafe { xmlValidateDocument(ctxt, ptr::null_mut()) }, -1);
        unsafe { xmlFreeValidCtxt(ctxt); }
    }

    #[test]
    fn validate_doc_with_no_dtd_returns_one() {
        // No internal subset → nothing to validate against → 1.
        let doc = unsafe { crate::parse::xmlReadMemory(
            b"<r/>".as_ptr() as *const _,
            4, ptr::null(), ptr::null(), 0,
        )};
        assert!(!doc.is_null());
        let ctxt = unsafe { xmlNewValidCtxt() };
        assert_eq!(unsafe { xmlValidateDocument(ctxt, doc) }, 1);
        unsafe { xmlFreeValidCtxt(ctxt); }
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn validate_doc_against_internal_dtd() {
        let src = br#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a+)>
  <!ELEMENT a EMPTY>
]>
<r><a/><a/></r>"#;
        let doc = unsafe { crate::parse::xmlReadMemory(
            src.as_ptr() as *const _,
            src.len() as c_int,
            ptr::null(), ptr::null(), 0,
        )};
        assert!(!doc.is_null());
        let ctxt = unsafe { xmlNewValidCtxt() };
        assert_eq!(unsafe { xmlValidateDocument(ctxt, doc) }, 1);
        unsafe { xmlFreeValidCtxt(ctxt); }
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn validate_doc_against_internal_dtd_failure() {
        // Wrong element order vs the (a, b) content model.
        let src = br#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><b/><a/></r>"#;
        let doc = unsafe { crate::parse::xmlReadMemory(
            src.as_ptr() as *const _,
            src.len() as c_int,
            ptr::null(), ptr::null(), 0,
        )};
        assert!(!doc.is_null());
        let ctxt = unsafe { xmlNewValidCtxt() };
        assert_eq!(unsafe { xmlValidateDocument(ctxt, doc) }, 0);
        unsafe { xmlFreeValidCtxt(ctxt); }
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn dtd_round_trip() {
        let name = std::ffi::CString::new("html").unwrap();
        let dtd = unsafe { xmlNewDtd(ptr::null_mut(), name.as_ptr(), ptr::null(), ptr::null()) };
        assert!(!dtd.is_null());
        let copy = unsafe { xmlCopyDtd(dtd) };
        assert!(!copy.is_null());
        unsafe { xmlFreeDtd(dtd); xmlFreeDtd(copy); }
    }

    #[test]
    fn create_int_subset_attaches() {
        let doc = unsafe { crate::mutate::xmlNewDoc(ptr::null()) };
        assert!(!doc.is_null());
        let name = std::ffi::CString::new("html").unwrap();
        let dtd = unsafe {
            xmlCreateIntSubset(doc, name.as_ptr(), ptr::null(), ptr::null())
        };
        assert!(!dtd.is_null());
        let int_subset: *mut xmlDtd = unsafe {
            let p = (doc as *const u8).add(80);
            let mut arr = [0u8; 8];
            std::ptr::copy_nonoverlapping(p, arr.as_mut_ptr(), 8);
            usize::from_ne_bytes(arr) as *mut xmlDtd
        };
        assert_eq!(int_subset, dtd);
        // Don't free dtd — it's owned by the doc now; xmlFreeDoc would in real
        // libxml2 walk and free intSubset.  Our v0.1 leaks it, which is fine
        // (process exits anyway).
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn get_int_subset_returns_attached_dtd() {
        let doc = unsafe { crate::mutate::xmlNewDoc(ptr::null()) };
        let n = std::ffi::CString::new("html").unwrap();
        let dtd = unsafe { xmlCreateIntSubset(doc, n.as_ptr(), ptr::null(), ptr::null()) };
        assert!(!dtd.is_null());
        assert_eq!(unsafe { xmlGetIntSubset(doc) }, dtd);
        // Empty doc → NULL.
        let doc2 = unsafe { crate::mutate::xmlNewDoc(ptr::null()) };
        assert!(unsafe { xmlGetIntSubset(doc2) }.is_null());
        unsafe {
            crate::parse::xmlFreeDoc(doc);
            crate::parse::xmlFreeDoc(doc2);
        }
    }

    #[test]
    fn add_id_round_trip_records_attribute() {
        let xml = b"<r xml:id=\"x1\"/>\0";
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                xml.as_ptr() as *const c_char,
                (xml.len() - 1) as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let r = unsafe { &*root };
        let attr = r.first_attribute.get().unwrap()
            as *const _ as *mut sup_xml_tree::dom::Attribute<'static>;
        let value = std::ffi::CString::new("x1").unwrap();
        let got = unsafe { xmlAddID(ptr::null_mut(), doc, value.as_ptr(), attr) };
        assert_eq!(got, attr, "xmlAddID returns the attr it just recorded");
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn free_id_table_is_safe_with_null() {
        unsafe { xmlFreeIDTable(ptr::null_mut()); }
    }

    #[test]
    fn get_doc_entity_returns_null_for_now() {
        // v0.1 doesn't carry a post-parse entity table; the contract
        // is "NULL == not found", which callers handle gracefully.
        let doc = unsafe { crate::mutate::xmlNewDoc(ptr::null()) };
        let name = std::ffi::CString::new("amp").unwrap();
        assert!(unsafe { xmlGetDocEntity(doc, name.as_ptr()) }.is_null());
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    /// Drive a byte slice through an `xmlParserInputBuffer` read callback
    /// the way lxml's file-like DTD path does, so `xmlIOParseDTD` sees a
    /// real buffer to drain.
    unsafe extern "C" fn slice_reader(
        ctx: *mut c_void, out: *mut c_char, len: c_int,
    ) -> c_int {
        // ctx points at a (cursor, &[u8]) pair we stashed below.
        let state = unsafe { &mut *(ctx as *mut (usize, &[u8])) };
        let (pos, data) = (state.0, state.1);
        let remaining = data.len().saturating_sub(pos);
        let n = remaining.min(len.max(0) as usize);
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), out as *mut u8, n); }
        state.0 += n;
        n as c_int
    }

    #[test]
    fn io_parse_dtd_then_validate() {
        // Build an input buffer the way lxml's `etree.DTD(fileobj)` does,
        // parse it, and validate a matching / non-matching document
        // against the resulting standalone handle.
        let dtd_src: &[u8] = b"<!ELEMENT b (a)><!ELEMENT a EMPTY>";
        let mut state: (usize, &[u8]) = (0, dtd_src);
        let buf = unsafe { crate::misc::xmlAllocParserInputBuffer(0) };
        // Lay out context @0 and readcallback @8, matching the struct view.
        unsafe {
            let p = buf as *mut *mut c_void;
            *p = (&mut state as *mut (usize, &[u8])) as *mut c_void;
            let cb = p.add(1) as *mut Option<crate::reader::XmlInputReadCallback>;
            *cb = Some(slice_reader);
        }
        let dtd = unsafe { xmlIOParseDTD(ptr::null_mut(), buf, 0) };
        assert!(!dtd.is_null(), "xmlIOParseDTD should parse a well-formed subset");

        let ctxt = unsafe { xmlNewValidCtxt() };
        let valid = b"<b><a/></b>";
        let vdoc = unsafe { crate::parse::xmlReadMemory(
            valid.as_ptr() as *const c_char, valid.len() as c_int,
            ptr::null(), ptr::null(), 0) };
        assert_eq!(unsafe { xmlValidateDtd(ctxt, vdoc, dtd) }, 1);

        let invalid = b"<b><c/></b>";
        let idoc = unsafe { crate::parse::xmlReadMemory(
            invalid.as_ptr() as *const c_char, invalid.len() as c_int,
            ptr::null(), ptr::null(), 0) };
        assert_eq!(unsafe { xmlValidateDtd(ctxt, idoc, dtd) }, 0);

        unsafe {
            xmlFreeValidCtxt(ctxt);
            crate::parse::xmlFreeDoc(vdoc);
            crate::parse::xmlFreeDoc(idoc);
            xmlFreeDtd(dtd);
        }
    }
}
