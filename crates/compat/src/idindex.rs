//! `xmlGetID` / `xmlRemoveID` — document-scoped index of `id="…"`
//! attributes.
//!
//! libxml2 builds this lazily on first lookup.  We do the same: walk
//! the document tree once on the first `xmlGetID` call against a
//! given doc, cache the results in a thread-local `HashMap` keyed on
//! the doc pointer's address.  Subsequent lookups are O(1).
//!
//! We DON'T persist anything on the `XmlDoc` itself — that would
//! require either adding a private-use field or growing the struct.
//! A per-thread shared map keyed on the doc's address gives us O(1)
//! lookup without touching ABI layout.  Drawback: if a caller passes
//! a doc between threads (which libxml2 also disallows for mutation),
//! the cache doesn't migrate.  Acceptable for now.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_tree::dom::{Attribute, Node, NodeKind, XmlDoc};

// ── per-thread cache ───────────────────────────────────────────────────────

thread_local! {
    /// Map: doc-pointer-address → (id_value → attribute-pointer).
    /// We use `usize` keys (addresses) so we don't borrow from the doc.
    /// Values are raw pointers, also `usize` for the same reason.
    static ID_CACHES: RefCell<HashMap<usize, HashMap<String, usize>>>
        = RefCell::new(HashMap::new());
}

/// Build (or refresh) the ID index for `doc`.  Walks every element
/// looking for `id="..."` and `xml:id="..."` attributes.  libxml2
/// also indexes DTD-declared ID attributes; we don't have DTD-type
/// info on attributes in v0.1, so for now only the literal names
/// match.  Real-world XML pretty much always uses `id` anyway.
fn build_index_for(doc: *const XmlDoc) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    if doc.is_null() {
        return map;
    }
    // SAFETY: caller asserts doc came from xmlReadMemory.
    let d = unsafe { &*doc };
    let mut stack: Vec<*const Node<'static>> = Vec::new();
    if let Some(root) = unsafe { (d.children.get() as *const Node<'static>).as_ref() } {
        stack.push(root);
    }
    while let Some(np) = stack.pop() {
        // SAFETY: np was popped from the stack we built from arena pointers.
        let n = unsafe { &*np };
        if matches!(n.kind, NodeKind::Element) {
            for attr in n.attributes() {
                let local = local_name_of(attr.name());
                if local == "id" {
                    let id_val = attr.value().to_string();
                    if !id_val.is_empty() {
                        let attr_addr = attr as *const Attribute<'_> as usize;
                        map.insert(id_val, attr_addr);
                    }
                }
            }
            // Push children in reverse so traversal is document order.
            let mut child = n.first_child.get();
            let mut buf: Vec<&Node<'static>> = Vec::new();
            while let Some(c) = child {
                buf.push(c);
                child = c.next_sibling.get();
            }
            for c in buf.into_iter().rev() {
                stack.push(c as *const Node<'static>);
            }
        }
    }
    map
}

#[inline]
fn local_name_of(s: &str) -> &str {
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None    => s,
    }
}

fn with_index<R>(doc: *const XmlDoc, f: impl FnOnce(&mut HashMap<String, usize>) -> R) -> R {
    let key = doc as usize;
    ID_CACHES.with(|caches| {
        let mut caches = caches.borrow_mut();
        let entry = caches.entry(key).or_insert_with(|| build_index_for(doc));
        f(entry)
    })
}

/// Drop the cached index for `doc` — called from `xmlFreeDoc` (FUTURE:
/// we'd need to wire this into `XmlDoc::free`; for now it's exposed
/// for completeness).
#[doc(hidden)]
pub fn invalidate(doc: *const XmlDoc) {
    let key = doc as usize;
    ID_CACHES.with(|caches| {
        caches.borrow_mut().remove(&key);
    });
}

// ── exported entry points ──────────────────────────────────────────────────

/// `xmlGetID(doc, name)` — return the attribute whose `id` value
/// equals `name`, or NULL if no such attribute exists in the doc.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetID(
    doc:  *const XmlDoc,
    name: *const c_char,
) -> *mut Attribute<'static> {
    if doc.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts name is NUL-terminated.
    let name_str = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    with_index(doc, |index| {
        match index.get(name_str) {
            Some(&addr) => addr as *mut Attribute<'static>,
            None        => ptr::null_mut(),
        }
    })
}

/// `xmlRemoveID(doc, attr)` — drop the index entry for `attr`.
/// Returns 0 on success, -1 if not found.  libxml2 returns this so
/// callers know whether the attribute was actually indexed.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRemoveID(
    doc:  *mut XmlDoc,
    attr: *mut Attribute<'static>,
) -> std::os::raw::c_int {
    if doc.is_null() || attr.is_null() {
        return -1;
    }
    let target = attr as usize;
    with_index(doc as *const _, |index| {
        // Find the key that maps to `target` and remove it.  Index
        // is keyed by ID value, not by attribute address — linear
        // search.  Acceptable: indexes are typically small.
        let key = index.iter()
            .find_map(|(k, &v)| if v == target { Some(k.clone()) } else { None });
        match key {
            Some(k) => { index.remove(&k); 0 }
            None    => -1,
        }
    })
}

// ── doc->ids hash table (libxml2 `_xmlID` / `xmlAddID`) ─────────────────────
//
// libxml2 populates `doc->ids` — a hash of ID-value → `xmlID` — as it
// parses attributes the DTD declared `ID` (or the predefined `xml:id`).
// lxml's `parseid` / `_IDDict` read this field directly: it raises
// "No ID dictionary available" when `doc->ids` is NULL, then
// `xmlHashLookup` / `xmlHashScan` over it and follow each entry's
// `attr` to the owning element.  We build the same structure once,
// right after a DTD-bearing parse.

/// libxml2 `_xmlID`.  Only `attr` (offset 16) is load-bearing for
/// lxml — it follows `attr->parent` to the element and takes the ID
/// value from the hash key — but the remaining fields are populated
/// for ABI fidelity with other consumers.
#[repr(C)]
struct XmlId {
    next:   *mut c_void,             //  0  (unused; legacy bucket link)
    value:  *const c_char,           //  8  (the ID value)
    attr:   *mut Attribute<'static>, // 16  (attribute carrying the ID)
    name:   *const c_char,           // 24  (attribute name)
    lineno: c_int,                   // 32
    _pad:   c_int,                   // 36
    doc:    *mut XmlDoc,             // 40
}

/// Whether `attr` is an ID-typed attribute: the predefined `xml:id`
/// (XML-ID Recommendation — local name `id` in the XML namespace), or
/// an attribute the DTD declared with type `ID`.  In c-abi mode a
/// namespaced attribute's `name()` is the local part, so `xml:id` is
/// detected by namespace, not by the `xml:` prefix string.
#[inline]
fn is_id_attr(attr: &Attribute<'_>, elem_ids: Option<&Vec<String>>) -> bool {
    if attr.name() == "id" {
        if let Some(ns) = attr.namespace.get() {
            if ns.href() == "http://www.w3.org/XML/1998/namespace"
                || ns.prefix() == Some("xml")
            {
                return true;
            }
        }
    }
    elem_ids.is_some_and(|v| v.iter().any(|a| a == attr.name()))
}

/// Populate `doc->ids` for a freshly parsed document — registering each
/// DTD-`ID`-typed (or `xml:id`) attribute keyed by its value.
///
/// libxml2 builds this table inline during the parse; we build it in a
/// single post-parse walk.  We populate the table the consumer's
/// `startDocument` SAX callback already created on `doc->ids` (lxml's
/// `_initSaxDocument` does this when `collect_ids` is on).  When
/// `doc->ids` is NULL the consumer asked not to collect IDs (lxml
/// `collect_ids=False`) and we do nothing — matching libxml2.
pub(crate) fn populate_doc_id_table(doc: *mut XmlDoc) {
    if doc.is_null() {
        return;
    }
    let table = unsafe { (*doc).ids } as *mut crate::hash::xmlHashTable;
    if table.is_null() {
        return;
    }
    crate::dtd::with_stashed_dtd(doc, |dtd| {
        let id_attrs = dtd.map(sup_xml_core::dtd::collect_id_attrs).unwrap_or_default();

        // SAFETY: doc came from a successful parse.
        let d = unsafe { &*doc };
        let mut stack: Vec<*const Node<'static>> = Vec::new();
        if let Some(root) = unsafe { (d.children.get() as *const Node<'static>).as_ref() } {
            stack.push(root);
        }
        while let Some(np) = stack.pop() {
            // SAFETY: np came from this doc's arena.
            let n = unsafe { &*np };
            if matches!(n.kind, NodeKind::Element) {
                let elem_ids = id_attrs.get(n.name());
                for attr in n.attributes() {
                    if !is_id_attr(attr, elem_ids) {
                        continue;
                    }
                    let value = attr.value();
                    if value.is_empty() {
                        continue;
                    }
                    let (Ok(value_cs), Ok(name_cs)) =
                        (CString::new(value), CString::new(attr.name()))
                    else {
                        continue;
                    };
                    let value_raw = value_cs.into_raw();
                    let name_raw = name_cs.into_raw();
                    let id = Box::into_raw(Box::new(XmlId {
                        next:   ptr::null_mut(),
                        value:  value_raw,
                        attr:   attr as *const Attribute<'_> as *mut Attribute<'static>,
                        name:   name_raw,
                        lineno: 0,
                        _pad:   0,
                        doc,
                    }));
                    // First declaration of a given ID wins; libxml2
                    // likewise rejects a duplicate.  On collision,
                    // reclaim what we just allocated so nothing leaks.
                    let rc = unsafe {
                        crate::hash::xmlHashAddEntry(table, value_raw, id as *mut c_void)
                    };
                    if rc != 0 {
                        unsafe { free_id(id); }
                    }
                }
                let mut child = n.first_child.get();
                let mut buf: Vec<&Node<'static>> = Vec::new();
                while let Some(c) = child {
                    buf.push(c);
                    child = c.next_sibling.get();
                }
                for c in buf.into_iter().rev() {
                    stack.push(c as *const Node<'static>);
                }
            }
        }
    });
}

/// Free a single `xmlID` and the C strings it owns.
///
/// # Safety
/// `id` must be a pointer returned by `Box::into_raw` in
/// [`build_doc_id_table`], not yet freed.
unsafe fn free_id(id: *mut XmlId) {
    let boxed = unsafe { Box::from_raw(id) };
    if !boxed.value.is_null() {
        drop(unsafe { CString::from_raw(boxed.value as *mut c_char) });
    }
    if !boxed.name.is_null() {
        drop(unsafe { CString::from_raw(boxed.name as *mut c_char) });
    }
}

unsafe extern "C" fn free_id_entry(payload: *mut c_void, _user: *mut c_void, _name: *const c_char) {
    if !payload.is_null() {
        unsafe { free_id(payload as *mut XmlId); }
    }
}

/// Release `doc->ids` and every `xmlID` it owns.  Called from
/// `xmlFreeDoc`; a no-op when the document never built an ID table.
pub(crate) fn free_doc_id_table(doc: *mut XmlDoc) {
    if doc.is_null() {
        return;
    }
    let ids = unsafe { (*doc).ids } as *mut crate::hash::xmlHashTable;
    if ids.is_null() {
        return;
    }
    unsafe { (*doc).ids = ptr::null_mut(); }
    unsafe {
        crate::hash::xmlHashScan(ids, Some(free_id_entry), ptr::null_mut());
        crate::hash::xmlHashFree(ids, None);
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::c_int;

    use crate::parse::{xmlFreeDoc, xmlReadMemory};

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

    #[test]
    fn get_id_finds_indexed() {
        let doc = parse(
            b"<root>\
                <a id=\"alpha\"/>\
                <b id=\"beta\"><c id=\"gamma\"/></b>\
              </root>",
        );

        let alpha = cs("alpha");
        let a = unsafe { xmlGetID(doc, alpha.as_ptr()) };
        assert!(!a.is_null());
        assert_eq!(unsafe { &*a }.value(), "alpha");

        let gamma = cs("gamma");
        let c = unsafe { xmlGetID(doc, gamma.as_ptr()) };
        assert!(!c.is_null());
        assert_eq!(unsafe { &*c }.value(), "gamma");

        // Missing ID → NULL.
        let missing = cs("delta");
        assert!(unsafe { xmlGetID(doc, missing.as_ptr()) }.is_null());

        invalidate(doc);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn remove_id_drops_entry() {
        let doc = parse(b"<r><a id=\"x\"/></r>");
        let x = cs("x");
        let a = unsafe { xmlGetID(doc, x.as_ptr()) };
        assert!(!a.is_null());

        let rc = unsafe { xmlRemoveID(doc, a) };
        assert_eq!(rc, 0);

        // After removal, lookup returns NULL.
        assert!(unsafe { xmlGetID(doc, x.as_ptr()) }.is_null());

        // Double-remove returns -1.
        let rc = unsafe { xmlRemoveID(doc, a) };
        assert_eq!(rc, -1);

        invalidate(doc);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn null_safety() {
        let n = cs("anything");
        assert!(unsafe { xmlGetID(ptr::null(), n.as_ptr()) }.is_null());
        let doc = parse(b"<r/>");
        assert!(unsafe { xmlGetID(doc, ptr::null()) }.is_null());
        assert_eq!(unsafe { xmlRemoveID(ptr::null_mut(), ptr::null_mut()) }, -1);
        invalidate(doc);
        unsafe { xmlFreeDoc(doc); }
    }
}
