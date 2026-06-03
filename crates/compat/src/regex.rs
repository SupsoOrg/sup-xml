//! libxml2 `xmlRegexp` API surface — compile + match XSD §F regular
//! expressions.
//!
//! libxml2's `xmlRegexp` is named after XSD's regex flavour (the one
//! used in `xs:pattern` facets), not POSIX or PCRE.  Our implementation
//! delegates to `sup_xml_core::xsd::regex::Pattern`, which is the same
//! engine the validator uses internally.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use sup_xml_core::xsd::regex::Pattern;

/// Opaque to C callers — a compiled XSD §F pattern.  Returned by
/// `xmlRegexpCompile`, freed by `xmlRegFreeRegexp`, executed by
/// `xmlRegexpExec`.
#[allow(non_camel_case_types)]
pub struct xmlRegexp {
    inner: Pattern,
}

/// libxml2 `xmlRegexpCompile(regexp)` — compile an XSD §F pattern.
/// Returns NULL on NULL input or syntax error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegexpCompile(regexp: *const c_char) -> *mut xmlRegexp {
    if regexp.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let src = match unsafe { CStr::from_ptr(regexp) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    match Pattern::compile(src) {
        Ok(p)  => Box::into_raw(Box::new(xmlRegexp { inner: p })),
        Err(_) => ptr::null_mut(),
    }
}

/// libxml2 `xmlRegexpExec(comp, content)` — test whether `content`
/// matches the entire compiled pattern.  XSD §F patterns are
/// implicitly anchored to both ends.
///
/// Returns:
///   - `1` on match
///   - `0` on no match
///   - `-1` on NULL inputs or invalid UTF-8
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegexpExec(
    comp:    *mut xmlRegexp,
    content: *const c_char,
) -> c_int {
    if comp.is_null() || content.is_null() {
        return -1;
    }
    // SAFETY: caller asserts both pointers valid.
    let r = unsafe { &*comp };
    let s = match unsafe { CStr::from_ptr(content) }.to_str() {
        Ok(s)  => s,
        Err(_) => return -1,
    };
    if r.inner.is_match(s) { 1 } else { 0 }
}

/// libxml2 `xmlRegFreeRegexp(comp)` — drop a compiled pattern.
/// NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegFreeRegexp(comp: *mut xmlRegexp) {
    if comp.is_null() { return; }
    // SAFETY: comp came from Box::into_raw in xmlRegexpCompile.
    unsafe { let _ = Box::from_raw(comp); }
}

/// libxml2 `xmlRegexpIsDeterminist(comp)` — 1 if the regex is a
/// deterministic finite automaton (no backtracking), 0 if not, -1 on
/// NULL.  XSD §F patterns are required by the spec to be
/// deterministic at the content-model level; our engine compiles to
/// either a forward-only linear matcher or an NFA driven by a Pike VM,
/// neither of which backtracks, so we report 1 for any successfully
/// compiled pattern.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegexpIsDeterminist(comp: *mut xmlRegexp) -> c_int {
    if comp.is_null() { return -1; }
    1
}
