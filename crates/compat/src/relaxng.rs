//! libxml2 RelaxNG validator façade — wraps `sup_xml_core::relaxng`.
//!
//! Same shape as the XSD wrap in [`crate::xsd`]:
//!   * `xmlRelaxNGNew*ParserCtxt` → opaque heap struct holding the schema source
//!   * `xmlRelaxNGParse(ctxt)`    → compiles into a real [`RngSchema`]
//!   * `xmlRelaxNGNewValidCtxt` / `xmlRelaxNGSetValidStructuredErrors`
//!     / `xmlRelaxNGValidateDoc` route through our validator and dispatch
//!     each issue through the caller's structured-error callback.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::options::ParseOptions;
use sup_xml_core::parser::parse_bytes;
use sup_xml_core::relaxng::{parse_schema_with_base, validate, RngSchema};
use sup_xml_tree::dom::XmlDoc;

use crate::error::{record_last_error, xmlError, StructuredErrorFn};

enum RngSource {
    Doc(*const XmlDoc),
    File(CString),
    Memory(Vec<u8>),
}

pub struct xmlRelaxNGParserCtxt {
    source: RngSource,
    error_cb: RefCell<Option<(StructuredErrorFn, *mut c_void)>>,
}

pub struct xmlRelaxNG {
    inner: RngSchema,
}

pub struct xmlRelaxNGValidCtxt {
    schema:   *const xmlRelaxNG,
    error_cb: RefCell<Option<(StructuredErrorFn, *mut c_void)>>,
}

// ── parser context ────────────────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGNewDocParserCtxt(
    doc: *const XmlDoc,
) -> *mut xmlRelaxNGParserCtxt {
    if doc.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(xmlRelaxNGParserCtxt {
        source:   RngSource::Doc(doc),
        error_cb: RefCell::new(None),
    }))
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGNewParserCtxt(
    filename: *const c_char,
) -> *mut xmlRelaxNGParserCtxt {
    if filename.is_null() { return ptr::null_mut(); }
    let path = unsafe { CStr::from_ptr(filename) }.to_owned();
    Box::into_raw(Box::new(xmlRelaxNGParserCtxt {
        source:   RngSource::File(path),
        error_cb: RefCell::new(None),
    }))
}

/// libxml2 `xmlRelaxNGNewMemParserCtxt(buffer, size)` — build a
/// RelaxNG parser context from an in-memory schema source.  The
/// bytes are copied into the ctxt so the caller's buffer doesn't
/// need to outlive it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGNewMemParserCtxt(
    buffer: *const c_char,
    size:   c_int,
) -> *mut xmlRelaxNGParserCtxt {
    if buffer.is_null() || size <= 0 { return ptr::null_mut(); }
    // SAFETY: caller asserts `buffer` is readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    Box::into_raw(Box::new(xmlRelaxNGParserCtxt {
        source:   RngSource::Memory(bytes.to_vec()),
        error_cb: RefCell::new(None),
    }))
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGFreeParserCtxt(ctxt: *mut xmlRelaxNGParserCtxt) {
    if ctxt.is_null() { return; }
    unsafe { let _ = Box::from_raw(ctxt); }
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGSetParserStructuredErrors(
    ctxt:      *mut xmlRelaxNGParserCtxt,
    callback:  Option<StructuredErrorFn>,
    user_data: *mut c_void,
) {
    if ctxt.is_null() { return; }
    let c = unsafe { &*ctxt };
    *c.error_cb.borrow_mut() = callback.map(|cb| (cb, user_data));
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGParse(
    ctxt: *mut xmlRelaxNGParserCtxt,
) -> *mut xmlRelaxNG {
    if ctxt.is_null() { return ptr::null_mut(); }
    let c = unsafe { &*ctxt };
    let src: String = match &c.source {
        RngSource::Doc(doc) => {
            let mut mem: *mut c_char = ptr::null_mut();
            let mut size: c_int = 0;
            unsafe { crate::serialize::xmlDocDumpMemory(*doc, &mut mem, &mut size); }
            if mem.is_null() { return ptr::null_mut(); }
            let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size as usize) };
            let s = std::str::from_utf8(bytes).unwrap_or("").to_string();
            unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
            s
        }
        RngSource::File(path) => {
            let p = match path.to_str() {
                Ok(p) => p,
                Err(_) => return ptr::null_mut(),
            };
            match std::fs::read_to_string(p) {
                Ok(s) => s,
                Err(e) => {
                    emit_parser_error(ctxt, &XmlError::new(
                        ErrorDomain::Io, ErrorLevel::Fatal,
                        format!("xmlRelaxNGParse: {p}: {e}"),
                    ));
                    return ptr::null_mut();
                }
            }
        }
        RngSource::Memory(bytes) => {
            match std::str::from_utf8(bytes) {
                Ok(s)  => s.to_string(),
                Err(_) => {
                    emit_parser_error(ctxt, &XmlError::new(
                        ErrorDomain::Parser, ErrorLevel::Fatal,
                        "xmlRelaxNGNewMemParserCtxt: schema source is not valid UTF-8",
                    ));
                    return ptr::null_mut();
                }
            }
        }
    };
    // Base URI for resolving `<include href="…">`: the schema document's
    // URL (lxml sets it when parsing from a file/path) or the file path.
    let base: Option<String> = match &c.source {
        RngSource::Doc(doc) => {
            let url = unsafe { (**doc).url };
            if url.is_null() {
                None
            } else {
                unsafe { CStr::from_ptr(url) }.to_str().ok().filter(|s| !s.is_empty()).map(str::to_string)
            }
        }
        RngSource::File(path) => path.to_str().ok().map(str::to_string),
        RngSource::Memory(_) => None,
    };
    match parse_schema_with_base(&src, base.as_deref()) {
        Ok(schema) => Box::into_raw(Box::new(xmlRelaxNG { inner: schema })),
        Err(e) => {
            emit_parser_error(ctxt, &e);
            ptr::null_mut()
        }
    }
}

fn emit_parser_error(ctxt: *mut xmlRelaxNGParserCtxt, err: &XmlError) {
    if ctxt.is_null() { record_last_error(err); return; }
    let c = unsafe { &*ctxt };
    emit_via_callback(&c.error_cb.borrow(), err);
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGFree(schema: *mut xmlRelaxNG) {
    if schema.is_null() { return; }
    unsafe { let _ = Box::from_raw(schema); }
}

// ── valid context ─────────────────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGNewValidCtxt(
    schema: *mut xmlRelaxNG,
) -> *mut xmlRelaxNGValidCtxt {
    if schema.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(xmlRelaxNGValidCtxt {
        schema:   schema as *const _,
        error_cb: RefCell::new(None),
    }))
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGFreeValidCtxt(ctxt: *mut xmlRelaxNGValidCtxt) {
    if ctxt.is_null() { return; }
    unsafe { let _ = Box::from_raw(ctxt); }
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGSetValidStructuredErrors(
    ctxt:      *mut xmlRelaxNGValidCtxt,
    callback:  Option<StructuredErrorFn>,
    user_data: *mut c_void,
) {
    if ctxt.is_null() { return; }
    let c = unsafe { &*ctxt };
    *c.error_cb.borrow_mut() = callback.map(|cb| (cb, user_data));
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGValidateDoc(
    ctxt: *mut xmlRelaxNGValidCtxt,
    doc:  *mut XmlDoc,
) -> c_int {
    if ctxt.is_null() || doc.is_null() { return -1; }
    let c = unsafe { &*ctxt };
    if c.schema.is_null() { return -1; }
    let schema = unsafe { &(*c.schema).inner };
    // lxml validates a subtree by handing us a `_fakeRootDoc`: a doc
    // whose libxml2-ABI `children` pointer is rewritten by hand to the
    // node being validated, without updating the embedded Document's
    // cached `root`/`first_sibling`.  Re-derive those from the live ABI
    // chain before serializing, or the dump reads the stale entry and
    // the re-parsed tree is empty/wrong.  (Same fix the XPath engine
    // applies for libxslt result trees — see `sync_doc_entry_from_abi`.)
    unsafe { crate::xpath::sync_doc_entry_from_abi(doc); }
    // Serialize then re-parse via our regular parser to get a
    // Document<'_> we can hand to validate().
    let mut mem: *mut c_char = ptr::null_mut();
    let mut size: c_int = 0;
    unsafe { crate::serialize::xmlDocDumpMemory(doc, &mut mem, &mut size); }
    if mem.is_null() { return -1; }
    let bytes_vec = unsafe {
        std::slice::from_raw_parts(mem as *const u8, size as usize)
    }.to_vec();
    unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    let doc = match parse_bytes(&bytes_vec, &opts) {
        Ok(d) => d,
        Err(e) => {
            emit_via_callback(&c.error_cb.borrow(), &e);
            return -1;
        }
    };
    match validate(schema, &doc) {
        Ok(()) => 0,
        Err(e) => {
            emit_via_callback(&c.error_cb.borrow(), &e);
            // RelaxNG's sup-xml-core API returns at most one error;
            // libxml2's return is +ve == invalid, so report 1.
            1
        }
    }
}

// ── shared error-emit helper (mirror of xsd.rs) ──────────────────────────

fn emit_via_callback(
    cb: &Option<(StructuredErrorFn, *mut c_void)>,
    err: &XmlError,
) {
    let Some((cb_fn, user_data)) = cb else {
        record_last_error(err);
        return;
    };
    let msg_cs = CString::new(err.message.clone()).unwrap_or_default();
    let mut e: xmlError = unsafe { std::mem::zeroed() };
    e.domain  = err.domain as c_int;
    e.code    = err.code as c_int;
    e.message = msg_cs.as_ptr() as *mut c_char;
    e.level   = err.level as c_int;
    e.line    = err.line.unwrap_or(0) as c_int;
    e.int2    = err.column.unwrap_or(0) as c_int;
    // SAFETY: cb_fn is a caller-supplied extern "C" fn pointer;
    // &mut e and *user_data are valid for the call duration.
    unsafe { cb_fn(*user_data, &mut e as *mut xmlError); }
    drop(msg_cs);
}

// ── PHP-needed entry points (callback registrars + cleanup) ─────────────

/// `xmlRelaxNGSetParserErrors(ctxt, err, warn, ctx)` — install legacy
/// (non-structured) error / warning callbacks on the parser context.
/// PHP's RelaxNG validator binds these at construction so its own
/// error-mapping code can intercept.  We don't dispatch through
/// legacy callbacks (the structured variant is the canonical path);
/// accept the call as a no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGSetParserErrors(
    _ctxt: *mut xmlRelaxNGParserCtxt,
    _err:  *mut std::os::raw::c_void,
    _warn: *mut std::os::raw::c_void,
    _user: *mut std::os::raw::c_void,
) {}

/// `xmlRelaxNGSetValidErrors(ctxt, err, warn, ctx)` — same shape on
/// the validation context.  Same scope: no-op for now.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGSetValidErrors(
    _ctxt: *mut xmlRelaxNGValidCtxt,
    _err:  *mut std::os::raw::c_void,
    _warn: *mut std::os::raw::c_void,
    _user: *mut std::os::raw::c_void,
) {}

/// `xmlRelaxNGCleanupTypes()` — process-shutdown hook for the RelaxNG
/// datatype subsystem.  We don't allocate global state for types, so
/// this is a no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRelaxNGCleanupTypes() {}

// ── unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    const RNG_SCHEMA: &[u8] = br#"<?xml version="1.0"?>
<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
  <element name="name"><text/></element>
  <element name="age"><text/></element>
</element>"#;

    #[test]
    fn compile_and_validate_valid_doc() {
        let schema_doc = parse(RNG_SCHEMA);
        let pctxt = unsafe { xmlRelaxNGNewDocParserCtxt(schema_doc) };
        let schema = unsafe { xmlRelaxNGParse(pctxt) };
        assert!(!schema.is_null(), "schema compile failed");
        unsafe { xmlRelaxNGFreeParserCtxt(pctxt); }

        let doc = parse(b"<r><name>alice</name><age>30</age></r>");
        let vctxt = unsafe { xmlRelaxNGNewValidCtxt(schema) };
        let ret = unsafe { xmlRelaxNGValidateDoc(vctxt, doc) };
        assert_eq!(ret, 0, "valid doc should pass: ret={ret}");

        unsafe {
            xmlRelaxNGFreeValidCtxt(vctxt);
            xmlRelaxNGFree(schema);
            xmlFreeDoc(doc);
            xmlFreeDoc(schema_doc);
        }
    }

    #[test]
    fn validate_invalid_doc() {
        let schema_doc = parse(RNG_SCHEMA);
        let pctxt = unsafe { xmlRelaxNGNewDocParserCtxt(schema_doc) };
        let schema = unsafe { xmlRelaxNGParse(pctxt) };
        unsafe { xmlRelaxNGFreeParserCtxt(pctxt); }

        let bad = parse(b"<wrong/>");
        let vctxt = unsafe { xmlRelaxNGNewValidCtxt(schema) };
        let ret = unsafe { xmlRelaxNGValidateDoc(vctxt, bad) };
        assert!(ret > 0, "invalid doc should fail: ret={ret}");

        unsafe {
            xmlRelaxNGFreeValidCtxt(vctxt);
            xmlRelaxNGFree(schema);
            xmlFreeDoc(bad);
            xmlFreeDoc(schema_doc);
        }
    }

    #[test]
    fn null_safety() {
        unsafe {
            xmlRelaxNGFree(ptr::null_mut());
            xmlRelaxNGFreeValidCtxt(ptr::null_mut());
            xmlRelaxNGFreeParserCtxt(ptr::null_mut());
            assert!(xmlRelaxNGParse(ptr::null_mut()).is_null());
            assert_eq!(xmlRelaxNGValidateDoc(ptr::null_mut(), ptr::null_mut()), -1);
        }
    }
}
