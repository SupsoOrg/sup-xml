//! Tier 1 attribute API.
//!
//! Five functions, each NULL-safe:
//!
//!   - [`xmlGetProp`] — first attribute matching `name` (any namespace)
//!   - [`xmlGetNoNsProp`] — first non-namespaced attribute matching `name`
//!   - [`xmlGetNsProp`] — attribute matching `name` and namespace URI
//!   - [`xmlHasProp`] — pointer to the matching `xmlAttr*` (or NULL)
//!   - (T-ATTR-04 — direct walks of `element->properties` — needs no
//!     dedicated function; C tests read the field directly)
//!
//! The four "Get" variants return malloc'd copies of the attribute
//! value, registered with [`crate::alloc`] so [`crate::parse::xml_free_impl`]
//! can recognize them.  `xmlHasProp` returns the arena-resident
//! `xmlAttr*` directly — caller must NOT xmlFree it.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::ptr;

use sup_xml_tree::dom::{Attribute, Node};

use crate::alloc::alloc_registered_cstring;

// ── helpers ────────────────────────────────────────────────────────────────

/// Safely turn a `*const c_char` into a `&str`.  Returns `""` on
/// invalid UTF-8 (matches libxml2's "lenient on bad input" stance for
/// these lookups — bad names just don't match).
#[inline]
unsafe fn cstr_to_str<'a>(p: *const c_char) -> &'a str {
    if p.is_null() {
        return "";
    }
    // SAFETY: caller asserts p is a NUL-terminated C string.
    let cs = unsafe { CStr::from_ptr(p) };
    cs.to_str().unwrap_or("")
}

/// Extract the local part of an XML name — everything after the last
/// `:`, or the whole string if there's no colon.  libxml2 stores
/// `attr->name` as the local part with namespace info on `attr->ns`;
/// sup-xml's parser leaves the full QName in `attr->name`, so we
/// derive the local part here for matching purposes.
#[inline]
fn local_name(s: &str) -> &str {
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None    => s,
    }
}

/// Walk an element's attribute list looking for a local-name match,
/// applying the namespace predicate `ns_match`.  `xmlns` /
/// `xmlns:*` attributes are skipped — libxml2 treats them as
/// namespace declarations on `ns_def`, not regular attributes.
/// Returns the first hit.
fn find_attribute<'a, F>(
    el: &'a Node<'a>,
    name: &str,
    ns_match: F,
) -> Option<&'a Attribute<'a>>
where
    F: Fn(Option<&str>) -> bool,
{
    // Some callers pass the local name only (libxml2 convention —
    // `c_attr.name` is the local part with the prefix on `c_attr.ns`);
    // others pass the full QName ("a:attr").  Compare local-against-
    // local so both paths resolve the same attribute.
    let lookup_local = local_name(name);
    for attr in el.attributes() {
        let raw = attr.name();
        // Skip namespace declarations.
        if raw == "xmlns" || raw.starts_with("xmlns:") {
            continue;
        }
        if local_name(raw) != lookup_local {
            continue;
        }
        let attr_ns_href = attr.namespace.get().map(|ns| ns.href());
        if ns_match(attr_ns_href) {
            return Some(attr);
        }
    }
    None
}

// ── exported functions ─────────────────────────────────────────────────────

/// libxml2 `xmlGetProp`.  Returns a newly-allocated UTF-8 copy of the
/// first attribute on `node` named `name` (across any namespace), or
/// NULL if no match.
///
/// The returned pointer must be released via [`crate::parse::xml_free_impl`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetProp(
    node: *const Node<'static>,
    name: *const c_char,
) -> *mut c_char {
    if node.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts pointers are valid.
    let n = unsafe { &*node };
    let name_str = unsafe { cstr_to_str(name) };
    match find_attribute(n, name_str, |_| true) {
        Some(a) => alloc_registered_cstring(a.value().as_bytes()),
        None    => ptr::null_mut(),
    }
}

/// libxml2 `xmlGetNoNsProp`.  Like `xmlGetProp`, but only matches
/// attributes that have no namespace (i.e. `attr->ns == NULL`).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetNoNsProp(
    node: *const Node<'static>,
    name: *const c_char,
) -> *mut c_char {
    if node.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlGetProp.
    let n = unsafe { &*node };
    let name_str = unsafe { cstr_to_str(name) };
    match find_attribute(n, name_str, |ns| ns.is_none()) {
        Some(a) => alloc_registered_cstring(a.value().as_bytes()),
        None    => ptr::null_mut(),
    }
}

/// libxml2 `xmlGetNsProp(node, name, namespace)`.  Match on attribute
/// name AND namespace URI.  When `namespace` is NULL, the function
/// behaves like [`xmlGetNoNsProp`] (only matches un-namespaced attrs).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetNsProp(
    node: *const Node<'static>,
    name: *const c_char,
    namespace: *const c_char,
) -> *mut c_char {
    if node.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlGetProp.
    let n = unsafe { &*node };
    let name_str = unsafe { cstr_to_str(name) };
    // NULL `namespace` → match only un-namespaced attrs.
    if namespace.is_null() {
        return match find_attribute(n, name_str, |ns| ns.is_none()) {
            Some(a) => alloc_registered_cstring(a.value().as_bytes()),
            None    => ptr::null_mut(),
        };
    }
    let ns_uri = unsafe { cstr_to_str(namespace) };
    match find_attribute(n, name_str, |ns| ns == Some(ns_uri)) {
        Some(a) => alloc_registered_cstring(a.value().as_bytes()),
        None    => ptr::null_mut(),
    }
}

/// libxml2 `xmlHasProp`.  Returns the matching `xmlAttr*` from
/// `node->properties` (any namespace) — same matching policy as
/// [`xmlGetProp`] — or NULL on miss.
///
/// The returned pointer is arena-resident: do NOT xmlFree it.  It
/// stays valid until the owning document is freed.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHasProp(
    node: *const Node<'static>,
    name: *const c_char,
) -> *mut Attribute<'static> {
    if node.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlGetProp.
    let n = unsafe { &*node };
    let name_str = unsafe { cstr_to_str(name) };
    match find_attribute(n, name_str, |_| true) {
        Some(a) => a as *const Attribute<'_> as *mut Attribute<'static>,
        None    => ptr::null_mut(),
    }
}

/// libxml2 `xmlHasNsProp(node, name, namespace)`.  Like
/// [`xmlHasProp`] but adds namespace matching with the same
/// semantics as [`xmlGetNsProp`]: NULL `namespace` matches
/// un-namespaced attrs only.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHasNsProp(
    node: *const Node<'static>,
    name: *const c_char,
    namespace: *const c_char,
) -> *mut Attribute<'static> {
    if node.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlGetProp.
    let n = unsafe { &*node };
    let name_str = unsafe { cstr_to_str(name) };
    let hit = if namespace.is_null() {
        find_attribute(n, name_str, |ns| ns.is_none())
    } else {
        let ns_uri = unsafe { cstr_to_str(namespace) };
        find_attribute(n, name_str, |ns| ns == Some(ns_uri))
    };
    match hit {
        Some(a) => a as *const Attribute<'_> as *mut Attribute<'static>,
        None    => ptr::null_mut(),
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::c_int;

    use crate::parse::{xmlDocGetRootElement, xmlFree, xmlFreeDoc, xmlReadMemory};

    fn parse(src: &[u8]) -> *mut sup_xml_tree::dom::XmlDoc {
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

    fn cs(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn get_prop_basic() {
        let doc = parse(b"<r id=\"42\" name=\"hello\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };

        let id = cs("id");
        let p = unsafe { xmlGetProp(root, id.as_ptr()) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "42");
        unsafe { xmlFree(p as *mut _); }

        let name = cs("name");
        let p = unsafe { xmlGetProp(root, name.as_ptr()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "hello");
        unsafe { xmlFree(p as *mut _); }

        // Missing attribute → NULL.
        let missing = cs("missing");
        let p = unsafe { xmlGetProp(root, missing.as_ptr()) };
        assert!(p.is_null());

        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn has_prop_returns_arena_ptr() {
        let doc = parse(b"<r id=\"42\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let id = cs("id");
        let a = unsafe { xmlHasProp(root, id.as_ptr()) };
        assert!(!a.is_null());
        // The Attribute's value() should match the input.
        assert_eq!(unsafe { &*a }.value(), "42");
        // xmlFree on an arena pointer must be a safe no-op.
        unsafe { xmlFree(a as *mut _); }
        // Re-read after the "free": pointer should still be valid.
        assert_eq!(unsafe { &*a }.value(), "42");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn no_ns_prop_ignores_namespaced() {
        // Document order: namespace decl, plain id, prefixed id.  The
        // namespace decl belongs in ns_def (libxml2 convention) — our
        // attribute walk filters it out via the xmlns-prefix rule.
        let doc = parse(
            b"<r xmlns:x=\"http://example.com/x\" id=\"plain\" x:id=\"prefixed\"/>",
        );
        let root = unsafe { xmlDocGetRootElement(doc) };
        let id = cs("id");
        // xmlGetProp accepts the first match by local name regardless
        // of namespace.  In our parser the in-document order is
        // {xmlns:x, id, x:id}; xmlns:x is filtered, leaving id first.
        let p = unsafe { xmlGetProp(root, id.as_ptr()) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "plain");
        unsafe { xmlFree(p as *mut _); }

        // xmlGetNoNsProp must skip the namespaced one.
        let p = unsafe { xmlGetNoNsProp(root, id.as_ptr()) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "plain");
        unsafe { xmlFree(p as *mut _); }
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn ns_prop_matches_by_uri() {
        let doc = parse(
            b"<r xmlns:x=\"http://example.com/x\" \
                 xmlns:y=\"http://example.com/y\" \
                 x:id=\"X-id\" y:id=\"Y-id\"/>",
        );
        let root = unsafe { xmlDocGetRootElement(doc) };
        let id  = cs("id");
        let nsx = cs("http://example.com/x");
        let nsy = cs("http://example.com/y");

        let p = unsafe { xmlGetNsProp(root, id.as_ptr(), nsx.as_ptr()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "X-id");
        unsafe { xmlFree(p as *mut _); }

        let p = unsafe { xmlGetNsProp(root, id.as_ptr(), nsy.as_ptr()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "Y-id");
        unsafe { xmlFree(p as *mut _); }

        // Unknown namespace → NULL.
        let unknown = cs("http://nope/");
        let p = unsafe { xmlGetNsProp(root, id.as_ptr(), unknown.as_ptr()) };
        assert!(p.is_null());

        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn null_safety() {
        let id = cs("id");
        let null_node = ptr::null::<Node<'static>>();
        assert!(unsafe { xmlGetProp(null_node, id.as_ptr())     }.is_null());
        assert!(unsafe { xmlGetNoNsProp(null_node, id.as_ptr()) }.is_null());
        assert!(unsafe { xmlGetNsProp(null_node, id.as_ptr(), ptr::null()) }.is_null());
        assert!(unsafe { xmlHasProp(null_node, id.as_ptr())     }.is_null());

        // NULL name input.
        let doc = parse(b"<r id=\"42\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert!(unsafe { xmlGetProp(root, ptr::null()) }.is_null());
        assert!(unsafe { xmlHasProp(root, ptr::null()) }.is_null());
        unsafe { xmlFreeDoc(doc); }
    }

    /// T-MEM-02 in Rust form: xmlFree on an arena pointer is a safe
    /// no-op — the pointer remains readable after.
    #[test]
    fn free_on_arena_pointer_is_noop() {
        let doc = parse(b"<r id=\"42\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        // root->name is arena-resident.
        let name_ptr = unsafe { &*root }.name.as_ptr() as *mut std::os::raw::c_void;
        unsafe { xmlFree(name_ptr); }
        // Still readable — we didn't actually free the bump arena.
        assert_eq!(unsafe { &*root }.name(), "r");
        unsafe { xmlFreeDoc(doc); }
    }
}
