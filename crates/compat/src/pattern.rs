//! libxml2 `xmlPattern` API — compile + match a stripped-XPath
//! pattern against nodes during a traversal.
//!
//! Delegates to [`sup_xml_core::xpath::pattern::Pattern`].  The Rust
//! engine is in core; this crate is just the C ABI wrapper plus the
//! pointer-discipline ceremony libxml2 callers expect.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use sup_xml_core::xpath::pattern::Pattern;
use sup_xml_tree::dom::Node;

/// Opaque to C callers — a compiled libxml2-flavour pattern.
#[allow(non_camel_case_types)]
pub struct xmlPattern {
    inner: Pattern,
}

/// libxml2 `xmlPatterncompile(pattern, dict, flags, namespaces)`.
///
/// Compiles the supplied pattern source.  `dict`, `flags`, and
/// `namespaces` are accepted for API parity and currently ignored —
/// patterns are stored fully-decoded inside the engine, the supported
/// grammar is a fixed subset (libxml2's pattern subset), and the
/// `XML_PATTERN_*` flags are non-determining for the cases we cover.
///
/// Returns NULL on NULL input or compile error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPatterncompile(
    pattern:     *const c_char,
    _dict:       *mut std::os::raw::c_void,
    _flags:      c_int,
    _namespaces: *const *const c_char,
) -> *mut xmlPattern {
    if pattern.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let src = match unsafe { CStr::from_ptr(pattern) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    match Pattern::compile(src) {
        Ok(p)  => Box::into_raw(Box::new(xmlPattern { inner: p })),
        Err(_) => ptr::null_mut(),
    }
}

/// libxml2 `xmlPatternMatch(comp, node)` — does `node` match `comp`?
///
/// Returns 1 on match, 0 on no match, -1 on NULL inputs.  Uses the
/// backward-walk evaluator (random access via parent pointers).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPatternMatch(
    comp: *mut xmlPattern,
    node: *mut Node<'static>,
) -> c_int {
    if comp.is_null() || node.is_null() {
        return -1;
    }
    // SAFETY: comp came from Box::into_raw in xmlPatterncompile.
    // node is a raw libxml2-shape pointer the caller asserts is valid.
    let pat = unsafe { &*comp };
    let n   = unsafe { &*node };
    if pat.inner.matches(n) { 1 } else { 0 }
}

/// libxml2 `xmlFreePattern(comp)` — drop a compiled pattern.
/// NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreePattern(comp: *mut xmlPattern) {
    if comp.is_null() { return; }
    // SAFETY: comp came from Box::into_raw in xmlPatterncompile.
    unsafe { let _ = Box::from_raw(comp); }
}

/// libxml2 `xmlPatternStreamable(comp)` — would this pattern be usable
/// in the streaming (forward state-machine) evaluator?
///
/// We implement only the backward-walk evaluator ([`xmlPatternMatch`],
/// random access via parent pointers) and ship no `xmlStream*` push API,
/// so we report `0`: not streamable.  Consumers that gate on this fall
/// back to random-access matching rather than reaching for a streaming
/// path that does not exist.  Reporting `1` would be a false promise —
/// there is no state machine to drive.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPatternStreamable(_comp: *mut xmlPattern) -> c_int {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn compile(src: &str) -> *mut xmlPattern {
        let c = CString::new(src).unwrap();
        unsafe { xmlPatterncompile(c.as_ptr(), ptr::null_mut(), 0, ptr::null()) }
    }

    fn parse(src: &str) -> *mut sup_xml_tree::dom::XmlDoc {
        unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        }
    }

    #[test]
    fn streamable_reports_not_streamable() {
        let p = compile("a/b");
        assert!(!p.is_null());
        // We ship no streaming evaluator, so this must be 0.
        assert_eq!(unsafe { xmlPatternStreamable(p) }, 0);
        unsafe { xmlFreePattern(p); }
    }

    #[test]
    fn element_pattern_still_matches() {
        let doc = parse("<r><e/></r>");
        assert!(!doc.is_null());
        let root = unsafe { &*((*doc).children.get() as *const Node<'static>) };
        let e = root.first_child.get().expect("<e> missing");
        let p = compile("e");
        assert_eq!(
            unsafe { xmlPatternMatch(p, e as *const Node as *mut Node<'static>) },
            1
        );
        unsafe { xmlFreePattern(p); crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn attribute_pattern_matches_attr_node() {
        // A libxml2 consumer reaches an attribute through `node->properties`
        // and passes the `xmlAttr*` to xmlPatternMatch as an `xmlNode*`.
        // Under the c-abi layout that pointer carries NodeKind::Attribute,
        // so attribute patterns must match it (previously they silently
        // returned no-match).
        let doc = parse(r#"<r><e foo="1" bar="2"/></r>"#);
        assert!(!doc.is_null());
        let root = unsafe { &*((*doc).children.get() as *const Node<'static>) };
        let e = root.first_child.get().expect("<e> missing");
        let e_ptr = e as *const Node as *mut Node<'static>;

        let foo_name = CString::new("foo").unwrap();
        let foo_attr = unsafe { crate::attr::xmlHasProp(e_ptr, foo_name.as_ptr()) };
        assert!(!foo_attr.is_null(), "xmlHasProp(foo) returned null");
        let attr_as_node = foo_attr as *mut Node<'static>;

        let p_foo = compile("@foo");
        let p_bar = compile("@bar");
        let p_any = compile("@*");
        let p_step = compile("e/@foo");
        unsafe {
            assert_eq!(xmlPatternMatch(p_foo, attr_as_node), 1, "@foo should match the foo attr");
            assert_eq!(xmlPatternMatch(p_bar, attr_as_node), 0, "@bar should not match the foo attr");
            assert_eq!(xmlPatternMatch(p_any, attr_as_node), 1, "@* should match any attr");
            assert_eq!(xmlPatternMatch(p_step, attr_as_node), 1, "e/@foo should walk attr→parent element");
            xmlFreePattern(p_foo);
            xmlFreePattern(p_bar);
            xmlFreePattern(p_any);
            xmlFreePattern(p_step);
            crate::parse::xmlFreeDoc(doc);
        }
    }
}
