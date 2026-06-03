//! EXSLT extension-registration entry points.
//!
//! libxslt's EXSLT (`exslt:date`, `exslt:math`, `exslt:str`,
//! `exslt:set`) provides additional XPath functions in distinct
//! namespaces.  In real libxslt, `exslt*XpathCtxtRegister` installs
//! both the function table and the prefix→URI binding on the given
//! `xmlXPathContext`; `exsltRegisterAll` installs every family into
//! libxslt's process-wide default function table that
//! `xmlXPathNewContext` reads from.
//!
//! In sup-xml the *functions* are always available — they live in
//! `sup_xml_core::xpath::exslt` and the engine's function
//! dispatcher consults them automatically (see
//! `xpath::eval::eval_function`).  The only thing the registration
//! calls need to do is what `xmlXPathRegisterNs` already does:
//! bind the supplied prefix to the family's namespace URI on the
//! given context.
//!
//! `exsltRegisterAll()` has no context to bind to, so it remains a
//! no-op success; callers normally pair it with explicit
//! `xmlXPathRegisterNs` calls (or supply prefixes via lxml's
//! `namespaces=` kwarg).

use std::os::raw::{c_char, c_int, c_void};

use crate::xpath::{xmlXPathContext, xmlXPathRegisterNs};

/// EXSLT namespace URIs — same constants the engine matches on.
/// Kept here as static C strings so we can hand pointers straight
/// to `xmlXPathRegisterNs` (which takes `*const c_char`).
const MATH_URI: &[u8] = b"http://exslt.org/math\0";
const DATE_URI: &[u8] = b"http://exslt.org/dates-and-times\0";
const STR_URI:  &[u8] = b"http://exslt.org/strings\0";
const SET_URI:  &[u8] = b"http://exslt.org/sets\0";

/// Default conventional prefix for each family — used when the
/// caller passes NULL for `prefix`, matching libxslt's "register
/// under the canonical name" fallback.
const MATH_PREFIX: &[u8] = b"math\0";
const DATE_PREFIX: &[u8] = b"date\0";
const STR_PREFIX:  &[u8] = b"str\0";
const SET_PREFIX:  &[u8] = b"set\0";

/// Common path for the four `exslt<Family>XpathCtxtRegister` calls:
/// bind `prefix` (or the family's conventional default) to `uri` on
/// `ctxt`'s namespace map.  Returns 0 on success, -1 on failure.
unsafe fn register_family(
    ctxt:           *mut c_void,
    prefix:         *const c_char,
    default_prefix: &'static [u8],
    uri:            &'static [u8],
) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    let prefix_ptr = if prefix.is_null() {
        default_prefix.as_ptr() as *const c_char
    } else {
        prefix
    };
    let uri_ptr = uri.as_ptr() as *const c_char;
    unsafe { xmlXPathRegisterNs(ctxt as *mut xmlXPathContext, prefix_ptr, uri_ptr) }
}

/// `exsltRegisterAll()` — libxslt installs every EXSLT family on
/// its process-wide default function table.  Our engine has those
/// functions always available, so this is a no-op success; pair it
/// with explicit `xmlXPathRegisterNs` calls (or supply prefixes via
/// lxml's `namespaces=`) to make `math:foo(...)` etc. resolvable.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn exsltRegisterAll() {}

/// `exsltDateXpathCtxtRegister(ctxt, prefix)` — bind the EXSLT
/// date/time URI to `prefix` on `ctxt` (or `"date"` if `prefix` is
/// NULL).  Returns 0 on success.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn exsltDateXpathCtxtRegister(
    ctxt:   *mut c_void,
    prefix: *const c_char,
) -> c_int {
    unsafe { register_family(ctxt, prefix, DATE_PREFIX, DATE_URI) }
}

/// `exsltMathXpathCtxtRegister(ctxt, prefix)` — bind the EXSLT
/// math URI to `prefix`.  Defaults to `"math"`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn exsltMathXpathCtxtRegister(
    ctxt:   *mut c_void,
    prefix: *const c_char,
) -> c_int {
    unsafe { register_family(ctxt, prefix, MATH_PREFIX, MATH_URI) }
}

/// `exsltSetsXpathCtxtRegister(ctxt, prefix)` — bind the EXSLT
/// set URI to `prefix`.  Defaults to `"set"`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn exsltSetsXpathCtxtRegister(
    ctxt:   *mut c_void,
    prefix: *const c_char,
) -> c_int {
    unsafe { register_family(ctxt, prefix, SET_PREFIX, SET_URI) }
}

/// `exsltStrXpathCtxtRegister(ctxt, prefix)` — bind the EXSLT
/// string URI to `prefix`.  Defaults to `"str"`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn exsltStrXpathCtxtRegister(
    ctxt:   *mut c_void,
    prefix: *const c_char,
) -> c_int {
    unsafe { register_family(ctxt, prefix, STR_PREFIX, STR_URI) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::ptr;

    /// NULL-context calls fail cleanly with -1; NULL-prefix calls
    /// fall back to the family's conventional prefix.  Real
    /// registration is exercised through the
    /// xmlXPathNewContext-based tests in `xpath::tests`.
    #[test]
    fn null_ctxt_returns_minus_one() {
        let p = CString::new("math").unwrap();
        assert_eq!(
            unsafe { exsltMathXpathCtxtRegister(ptr::null_mut(), p.as_ptr()) },
            -1,
        );
        assert_eq!(
            unsafe { exsltDateXpathCtxtRegister(ptr::null_mut(), ptr::null()) },
            -1,
        );
        assert_eq!(
            unsafe { exsltSetsXpathCtxtRegister(ptr::null_mut(), ptr::null()) },
            -1,
        );
        assert_eq!(
            unsafe { exsltStrXpathCtxtRegister(ptr::null_mut(), ptr::null()) },
            -1,
        );
    }

    #[test]
    fn register_all_is_noop() {
        unsafe { exsltRegisterAll(); }
    }
}
