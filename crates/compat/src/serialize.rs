//! Tier 1 serialization.
//!
//! Three functions, all hand back newly-allocated buffers that the
//! caller releases via [`crate::parse::xml_free_impl`]:
//!
//!   - [`xmlDocDumpMemory`] — compact whole-document dump
//!   - [`xmlDocDumpFormatMemory`] — same, with optional pretty-print
//!   - [`xmlNodeDump`] — serialize a single subtree (without the
//!     surrounding XML declaration)
//!
//! The output buffers are NUL-terminated UTF-8 (matching libxml2's
//! return shape), registered with the allocator so `xmlFree` knows
//! to release them.

use std::os::raw::{c_char, c_int};
use std::ptr;

use sup_xml_core::serializer::{serialize_node_to_string, serialize_with, SerializeOptions};
use sup_xml_tree::dom::{Attribute, Node, XmlDoc};

use crate::alloc::alloc_registered_cstring;
use crate::outbuf::{xmlBuffer, xmlBufferAdd};

/// libxml2 `xmlDocDumpMemory(doc, *mem, *size)`.  Serializes `doc`
/// (with XML declaration) into a freshly-allocated UTF-8 buffer.
///
/// On success, `*mem` is set to the buffer and `*size` to its byte
/// length (excluding the trailing NUL).  Caller releases via
/// [`crate::parse::xml_free_impl`].  On NULL `doc`, sets `*mem = NULL` and
/// `*size = 0`.
///
/// # Safety
///
/// `mem` and `size` may each be NULL (the corresponding output is
/// then dropped on the floor — matches libxml2's NULL-tolerant
/// behavior).  Non-NULL pointers must reference writable storage.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDocDumpMemory(
    doc:      *const XmlDoc,
    mem:      *mut *mut c_char,
    size:     *mut c_int,
) {
    // SAFETY: caller asserts preconditions on doc/mem/size; passed
    // through to dump_doc which checks NULL and treats valid pointers
    // as readable/writable.
    unsafe { dump_doc(doc, mem, size, /*format=*/ false) }
}

/// libxml2 `xmlDocDumpFormatMemory(doc, *mem, *size, format)`.
/// `format=0` is identical to [`xmlDocDumpMemory`]; `format=1`
/// inserts newlines + indentation between children.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDocDumpFormatMemory(
    doc:    *const XmlDoc,
    mem:    *mut *mut c_char,
    size:   *mut c_int,
    format: c_int,
) {
    // SAFETY: see xmlDocDumpMemory.
    unsafe { dump_doc(doc, mem, size, format != 0) }
}

unsafe fn dump_doc(
    doc:    *const XmlDoc,
    mem:    *mut *mut c_char,
    size:   *mut c_int,
    format: bool,
) {
    if doc.is_null() {
        unsafe {
            if !mem.is_null()  { *mem  = ptr::null_mut(); }
            if !size.is_null() { *size = 0; }
        }
        return;
    }
    // SAFETY: doc is non-null per the check; the embedded _doc is the
    // Rust Document that owns the arena and stays alive for as long
    // as the caller holds the XmlDoc pointer.
    let d = unsafe { &*doc };
    let opts = SerializeOptions {
        write_xml_decl: true,
        format,
        indent:         if format { "  ".to_string() } else { String::new() },
        html_mode:      false,
        xhtml:          unsafe { crate::outbuf::doc_is_xhtml(doc) },
        out_charset:    sup_xml_core::output::OutputCharset::Utf8,
    };
    let s = serialize_with(&d._doc, &opts);
    let bytes = s.as_bytes();
    let raw = alloc_registered_cstring(bytes);
    unsafe {
        if !mem.is_null()  { *mem  = raw; }
        if !size.is_null() {
            // libxml2 reports byte length excluding the NUL terminator.
            *size = if raw.is_null() { 0 } else { bytes.len() as c_int };
        }
    }
}

/// libxml2 `xmlNodeDump(buf, doc, cur, level, format)`.
///
/// **API divergence note**: real libxml2's `xmlNodeDump` writes into
/// a caller-supplied `xmlBuffer*` and returns the byte count.  We
/// don't ship the `xmlBuffer` type in v0.1; for now this function
/// allocates its own buffer, hands it back via an out-pointer, and
/// returns the size.  Callers that want libxml2's true signature
/// will need to wait for the xmlBuffer slice (Tier 2).
///
/// On success: writes a freshly-allocated UTF-8 buffer to `*out_buf`
/// and returns its byte length.  Caller `xmlFree`s `*out_buf`.
/// Returns -1 on error (NULL inputs).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeDump(
    out_buf: *mut *mut c_char,
    _doc:    *const XmlDoc,
    cur:     *const Node<'static>,
    _level:  c_int,
    format:  c_int,
) -> c_int {
    if cur.is_null() || out_buf.is_null() {
        return -1;
    }
    // SAFETY: cur is non-null per the check; lifetime tied to caller.
    let n = unsafe { &*cur };
    let opts = SerializeOptions {
        write_xml_decl: false,
        format:         format != 0,
        indent:         if format != 0 { "  ".to_string() } else { String::new() },
        html_mode:      false,
        xhtml:          unsafe { crate::outbuf::doc_is_xhtml(_doc) },
        out_charset:    sup_xml_core::output::OutputCharset::Utf8,
    };
    let s = serialize_node_to_string(n, &opts);
    let bytes = s.as_bytes();
    let raw = alloc_registered_cstring(bytes);
    if raw.is_null() {
        return -1;
    }
    unsafe { *out_buf = raw; }
    bytes.len() as c_int
}

/// libxml2 `xmlAttrSerializeTxtContent(buf, doc, attr, string)` —
/// append `string` (or `attr`'s value when `string` is NULL) to
/// `buf`, applying XML attribute-value escaping.  Used by consumers
/// that build attribute markup by hand (XSLT result-tree
/// serialization, lxml's custom serializer paths).
///
/// Escape rules per XML 1.0 § 3.3.3 (AttValue):
///   `&` → `&amp;`, `<` → `&lt;`, `"` → `&quot;`,
///   `\t` → `&#9;`, `\n` → `&#10;`, `\r` → `&#13;`.
/// `>` is not escaped — libxml2 doesn't either; only `<` is forbidden
/// in attribute values.
///
/// `doc` is accepted for API parity; the escape rules don't change
/// based on document properties.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAttrSerializeTxtContent(
    buf:    *mut xmlBuffer,
    _doc:   *mut XmlDoc,
    attr:   *mut Attribute<'static>,
    string: *const c_char,
) {
    if buf.is_null() {
        return;
    }
    // Source bytes: explicit `string` if non-NULL, else attr->value.
    let owned_from_attr: Option<&str>;
    let src: &[u8] = if !string.is_null() {
        // SAFETY: caller asserts NUL-terminated when non-NULL.
        unsafe { std::ffi::CStr::from_ptr(string) }.to_bytes()
    } else if !attr.is_null() {
        // SAFETY: attr is non-null per the check; lives in caller's arena.
        let a = unsafe { &*attr };
        owned_from_attr = Some(a.value());
        owned_from_attr.unwrap().as_bytes()
    } else {
        return;
    };

    // Walk the bytes, emitting escape sequences for the AttValue set
    // and coalescing pass-through runs into single appends.
    let mut start = 0;
    for (i, &b) in src.iter().enumerate() {
        let esc: &[u8] = match b {
            b'&'  => b"&amp;",
            b'<'  => b"&lt;",
            b'"'  => b"&quot;",
            b'\t' => b"&#9;",
            b'\n' => b"&#10;",
            b'\r' => b"&#13;",
            _     => continue,
        };
        if start < i {
            let chunk = &src[start..i];
            unsafe { xmlBufferAdd(buf, chunk.as_ptr() as *const c_char, chunk.len() as c_int); }
        }
        unsafe { xmlBufferAdd(buf, esc.as_ptr() as *const c_char, esc.len() as c_int); }
        start = i + 1;
    }
    if start < src.len() {
        let chunk = &src[start..];
        unsafe { xmlBufferAdd(buf, chunk.as_ptr() as *const c_char, chunk.len() as c_int); }
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    use crate::parse::{xmlDocGetRootElement, xmlFree, xmlFreeDoc, xmlReadMemory};

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

    /// T-SER-01 in Rust form: parse → dump → reparse → compare via dump.
    #[test]
    fn dump_then_reparse_round_trip() {
        let src = b"<r><a id=\"42\"/><b>text<c/></b></r>";
        let doc1 = parse(src);

        let mut mem1: *mut c_char = ptr::null_mut();
        let mut size1: c_int = 0;
        unsafe { xmlDocDumpMemory(doc1, &mut mem1, &mut size1); }
        assert!(!mem1.is_null());
        assert!(size1 > 0);
        let s1 = unsafe { CStr::from_ptr(mem1) }.to_str().unwrap().to_string();

        // Reparse the dump.  We feed `size1` bytes (NOT including NUL).
        let doc2 = unsafe {
            xmlReadMemory(mem1, size1, ptr::null(), ptr::null(), 0)
        };
        assert!(!doc2.is_null());

        let mut mem2: *mut c_char = ptr::null_mut();
        let mut size2: c_int = 0;
        unsafe { xmlDocDumpMemory(doc2, &mut mem2, &mut size2); }
        let s2 = unsafe { CStr::from_ptr(mem2) }.to_str().unwrap().to_string();

        // After one dump → reparse cycle, the byte stream stabilizes.
        assert_eq!(s1, s2);

        unsafe {
            xmlFree(mem1 as *mut _);
            xmlFree(mem2 as *mut _);
            xmlFreeDoc(doc1);
            xmlFreeDoc(doc2);
        }
    }

    #[test]
    fn formatted_dump_contains_newlines() {
        let doc = parse(b"<r><a/><b/></r>");
        let mut mem: *mut c_char = ptr::null_mut();
        let mut size: c_int = 0;
        unsafe { xmlDocDumpFormatMemory(doc, &mut mem, &mut size, 1); }
        let s = unsafe { CStr::from_ptr(mem) }.to_str().unwrap();
        assert!(s.contains('\n'), "formatted dump should contain newlines: {s:?}");
        unsafe { xmlFree(mem as *mut _); xmlFreeDoc(doc); }
    }

    #[test]
    fn node_dump_writes_subtree_only() {
        let doc = parse(b"<r><a id=\"42\"/><b><c/></b></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        // Subtree from root <r> — should not include XML declaration.
        let mut buf: *mut c_char = ptr::null_mut();
        let n = unsafe { xmlNodeDump(&mut buf, doc, root, 0, 0) };
        assert!(n > 0);
        let s = unsafe { CStr::from_ptr(buf) }.to_str().unwrap();
        assert!(!s.starts_with("<?xml"), "subtree dump must not include XML decl");
        assert!(s.contains("<a id=\"42\"/>"));
        unsafe { xmlFree(buf as *mut _); xmlFreeDoc(doc); }
    }

    #[test]
    fn null_safety() {
        // NULL doc / NULL out-pointers shouldn't crash.
        let mut mem: *mut c_char = ptr::null_mut();
        let mut size: c_int = 99;
        unsafe { xmlDocDumpMemory(ptr::null(), &mut mem, &mut size); }
        assert!(mem.is_null());
        assert_eq!(size, 0);

        // NULL cur to xmlNodeDump → -1.
        let mut buf: *mut c_char = ptr::null_mut();
        let n = unsafe { xmlNodeDump(&mut buf, ptr::null(), ptr::null(), 0, 0) };
        assert_eq!(n, -1);
    }

    #[test]
    fn xml_attr_serialize_txt_content_escapes_attval_set() {
        use crate::outbuf::{xmlBufferContent, xmlBufferCreate, xmlBufferFree};
        let buf = unsafe { xmlBufferCreate() };
        // String with every AttValue-relevant escape, plus pass-throughs.
        let src = std::ffi::CString::new("a&b<c\"d\te\nf\rg>h").unwrap();
        unsafe {
            xmlAttrSerializeTxtContent(
                buf, ptr::null_mut(), ptr::null_mut(), src.as_ptr(),
            );
        }
        let out = unsafe { CStr::from_ptr(xmlBufferContent(buf)) }.to_str().unwrap();
        // `>` is not escaped (matches libxml2's behavior).
        assert_eq!(out, "a&amp;b&lt;c&quot;d&#9;e&#10;f&#13;g>h");
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_attr_serialize_txt_content_null_string_uses_attr_value() {
        use crate::outbuf::{xmlBufferContent, xmlBufferCreate, xmlBufferFree};
        let doc = parse(b"<r k=\"a&amp;b\"/>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let attr = unsafe { (*root).first_attribute.get() }.unwrap()
            as *const Attribute<'_> as *mut Attribute<'static>;
        let buf = unsafe { xmlBufferCreate() };
        unsafe {
            xmlAttrSerializeTxtContent(buf, doc, attr, ptr::null());
        }
        let out = unsafe { CStr::from_ptr(xmlBufferContent(buf)) }.to_str().unwrap();
        assert_eq!(out, "a&amp;b");
        unsafe { xmlBufferFree(buf); xmlFreeDoc(doc); }
    }
}
