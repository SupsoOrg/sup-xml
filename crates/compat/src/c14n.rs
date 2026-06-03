//! libxml2 C14N (Canonical XML) façade — wraps
//! `sup_xml_core::canonical` behind `xmlC14NDocDumpMemory`,
//! `xmlC14NDocSaveTo`, and `xmlC14NExecute`.
//!
//! libxml2's `xmlC14NDocSave` (writes to a filename) is also exported,
//! routed through `xmlC14NDocDumpMemory` then `std::fs::write`.
//!
//! The `mode` parameter follows libxml2's `xmlC14NMode`:
//!   0 = `XML_C14N_1_0`         → C14n10
//!   1 = `XML_C14N_EXCLUSIVE_1_0` → ExcC14n10 with caller-supplied prefix list
//!   2 = `XML_C14N_1_1`         → falls back to C14n10 (we don't yet
//!                                 distinguish 1.0 from 1.1)
//!
//! `xmlC14NDocSaveTo` and `xmlC14NExecute` stream canonical bytes
//! directly into the supplied `xmlOutputBuffer` — the full canonical
//! form is never materialized in memory.  This matters for XML-DSig,
//! where the caller wires a hash context as the buffer's write
//! callback and feeds incremental chunks into the digest.
//!
//! The `nodes` / `xpath_nodes_set` argument (a node-set subtree
//! selector) on the *Doc* entry points is currently ignored — we
//! always canonicalize the whole document.  `xmlC14NExecute` gives
//! callers a finer-grained `is_visible` predicate for the same purpose.

use std::ffi::CStr;
use std::io::{self, Write};
use std::os::raw::{c_char, c_int, c_uchar, c_void};
use std::ptr;

use sup_xml_core::canonical::{
    canonicalize_to_bytes, canonicalize_with, C14nMode, CanonicalizeOptions, VisitTarget,
};
use sup_xml_tree::dom::{Attribute, Node, XmlDoc};

use crate::alloc::alloc_registered_cstring;
use crate::outbuf::xmlOutputBuffer;

/// libxml2's `xmlC14NIsVisibleCallback` — invoked per node and per
/// attribute during canonicalization.  Returns non-zero to include
/// the target, zero to exclude it.
///
/// Both the node and parent pointers may in practice point at an
/// `xmlAttr` (cast to `xmlNodePtr`); callers discriminate by reading
/// the `type` field at offset 8.
pub type xmlC14NIsVisibleCallback = unsafe extern "C" fn(
    user_data: *mut c_void,
    node:      *mut Node<'static>,
    parent:    *mut Node<'static>,
) -> c_int;

/// `xmlC14NDocDumpMemory(doc, nodes, mode, inclusive_ns_prefixes,
///                        with_comments, doc_txt_ptr)`.
///
/// Writes the canonicalized bytes to `*doc_txt_ptr` (caller releases
/// with `xmlFree`) and returns the byte length, or -1 on error.
///
/// `nodes` (xmlNodeSetPtr) and `inclusive_ns_prefixes` (xmlChar**) are
/// accepted but `nodes` is currently ignored — we canonicalize the
/// full document.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlC14NDocDumpMemory(
    doc:                     *const XmlDoc,
    _nodes:                  *mut std::os::raw::c_void,
    mode:                    c_int,
    inclusive_ns_prefixes:   *mut *const c_uchar,
    with_comments:           c_int,
    doc_txt_ptr:             *mut *mut c_uchar,
) -> c_int {
    if doc.is_null() || doc_txt_ptr.is_null() {
        return -1;
    }
    // SAFETY: callers asserted doc came from xmlReadMemory; its
    // embedded Document stays valid for the call's duration.
    let owned = unsafe { &*doc };
    let opts = CanonicalizeOptions {
        mode: c14n_mode_from_libxml(mode, inclusive_ns_prefixes),
        with_comments: with_comments != 0,
    };
    let bytes = canonicalize_to_bytes(&owned._doc, &opts);
    let len = bytes.len() as c_int;
    let raw = alloc_registered_cstring(&bytes) as *mut c_uchar;
    // SAFETY: caller asserted doc_txt_ptr points at writable storage.
    unsafe { *doc_txt_ptr = raw; }
    if raw.is_null() { -1 } else { len }
}

/// `xmlC14NDocSave(doc, nodes, mode, inclusive_ns_prefixes,
///                 with_comments, filename, compression)`.
///
/// Canonicalize + write to `filename`.  Compression is ignored (we
/// don't implement gzip output).  Returns the byte count, or -1 on
/// error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlC14NDocSave(
    doc:                     *const XmlDoc,
    nodes:                   *mut std::os::raw::c_void,
    mode:                    c_int,
    inclusive_ns_prefixes:   *mut *const c_uchar,
    with_comments:           c_int,
    filename:                *const c_char,
    _compression:            c_int,
) -> c_int {
    if filename.is_null() {
        return -1;
    }
    let mut mem: *mut c_uchar = ptr::null_mut();
    let len = unsafe {
        xmlC14NDocDumpMemory(
            doc, nodes, mode, inclusive_ns_prefixes, with_comments, &mut mem,
        )
    };
    if len < 0 || mem.is_null() {
        return -1;
    }
    // SAFETY: filename is non-null per the check and asserted NUL-terminated.
    let path_cs = unsafe { CStr::from_ptr(filename) };
    let path = match path_cs.to_str() {
        Ok(p) => p,
        Err(_) => {
            unsafe { crate::parse::xml_free_impl(mem as *mut std::os::raw::c_void); }
            return -1;
        }
    };
    // SAFETY: mem points at `len` bytes we just registered.
    let bytes = unsafe { std::slice::from_raw_parts(mem, len as usize) };
    let result = std::fs::write(path, bytes);
    unsafe { crate::parse::xml_free_impl(mem as *mut std::os::raw::c_void); }
    match result {
        Ok(_)  => len,
        Err(_) => -1,
    }
}

/// `xmlC14NDocSaveTo(doc, nodes, mode, inclusive_ns_prefixes,
///                   with_comments, buf)`.
///
/// Canonicalize and stream the bytes into `buf` via
/// [`xmlOutputBufferWrite`].  Whatever destination `buf` was created
/// with (in-memory, file-backed, user I/O callbacks) receives chunks
/// as the walker produces them — the full canonical form is never
/// materialized in memory.  Returns the byte count written, or -1 on
/// error.
///
/// `nodes` (xmlNodeSetPtr) is currently ignored — we canonicalize
/// the whole document.  Callers needing per-node filtering should
/// use [`xmlC14NExecute`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlC14NDocSaveTo(
    doc:                     *const XmlDoc,
    _nodes:                  *mut c_void,
    mode:                    c_int,
    inclusive_ns_prefixes:   *mut *const c_uchar,
    with_comments:           c_int,
    buf:                     *mut c_void,
) -> c_int {
    unsafe {
        xmlC14NExecute(
            doc,
            None,
            ptr::null_mut(),
            mode,
            inclusive_ns_prefixes,
            with_comments,
            buf as *mut xmlOutputBuffer,
        )
    }
}

/// `xmlC14NExecute(doc, is_visible_callback, user_data, mode,
///                 inclusive_ns_prefixes, with_comments, buf)`.
///
/// The streaming form used by XML-DSig signers: bytes flow through
/// `buf`'s write path as the walker produces them, so a caller wiring
/// a hash context as the buffer's write callback feeds the digest
/// incrementally without materializing the canonical form.
///
/// `is_visible_callback` is invoked per node and per attribute.
/// Returning non-zero includes the target; zero excludes it (and, for
/// element nodes, excludes the entire subtree per XML-DSig "subtree
/// exclusion" semantics).  A NULL callback includes everything.
///
/// Returns the byte count written, or -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlC14NExecute(
    doc:                     *const XmlDoc,
    is_visible_callback:     Option<xmlC14NIsVisibleCallback>,
    user_data:               *mut c_void,
    mode:                    c_int,
    inclusive_ns_prefixes:   *mut *const c_uchar,
    with_comments:           c_int,
    buf:                     *mut xmlOutputBuffer,
) -> c_int {
    if doc.is_null() || buf.is_null() {
        return -1;
    }
    // SAFETY: doc came from xmlReadMemory; its embedded Document
    // stays valid for the call's duration.
    let owned = unsafe { &*doc };
    let opts = CanonicalizeOptions {
        mode: c14n_mode_from_libxml(mode, inclusive_ns_prefixes),
        with_comments: with_comments != 0,
    };

    let mut sink = OutputBufferSink::new(buf);
    let predicate = |target: VisitTarget<'_, '_>| -> bool {
        let Some(cb) = is_visible_callback else { return true; };
        let (node_ptr, parent_ptr) = match target {
            VisitTarget::Node(n) => {
                let parent = n.parent.get()
                    .map(|p| p as *const Node<'_> as *mut Node<'static>)
                    .unwrap_or(ptr::null_mut());
                (n as *const Node<'_> as *mut Node<'static>, parent)
            }
            VisitTarget::Attribute(a) => {
                let parent = a.parent.get()
                    .map(|p| p as *const Node<'_> as *mut Node<'static>)
                    .unwrap_or(ptr::null_mut());
                // Attribute and Node share their first-eight-field layout
                // under the c-abi feature; libxml2's callback expects an
                // xmlNodePtr and discriminates via the `type` field.
                (a as *const Attribute<'_> as *mut Node<'static>, parent)
            }
        };
        // SAFETY: caller of xmlC14NExecute guarantees the callback is
        // a valid C function compatible with xmlC14NIsVisibleCallback.
        unsafe { cb(user_data, node_ptr, parent_ptr) != 0 }
    };

    match canonicalize_with(&owned._doc, &opts, &mut sink, predicate) {
        Ok(())  => sink.written(),
        Err(_)  => -1,
    }
}

/// `std::io::Write` sink that pipes each chunk through
/// [`xmlOutputBufferWrite`] — the buffer's write path then dispatches
/// to its in-memory / file-backed / user-callback destination.
struct OutputBufferSink {
    buf:     *mut xmlOutputBuffer,
    written: c_int,
}

impl OutputBufferSink {
    fn new(buf: *mut xmlOutputBuffer) -> Self {
        Self { buf, written: 0 }
    }

    /// Total bytes successfully passed to `xmlOutputBufferWrite`.
    fn written(&self) -> c_int {
        self.written
    }
}

impl Write for OutputBufferSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let len = c_int::try_from(bytes.len())
            .map_err(|_| io::Error::other("xmlOutputBufferWrite: chunk exceeds c_int range"))?;
        // SAFETY: `buf` was supplied by the caller of xmlC14NExecute
        // and stays valid for the call; `bytes` is a valid slice.
        let n = unsafe {
            crate::outbuf::xmlOutputBufferWrite(self.buf, len, bytes.as_ptr() as *const c_char)
        };
        if n < 0 {
            Err(io::Error::other("xmlOutputBufferWrite failed"))
        } else {
            self.written = self.written.saturating_add(n);
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn c14n_mode_from_libxml(
    mode: c_int,
    inclusive_ns_prefixes: *mut *const c_uchar,
) -> C14nMode {
    match mode {
        1 => {
            // Exclusive C14N — read the inclusive prefix list (NULL-terminated).
            let mut prefixes = Vec::new();
            if !inclusive_ns_prefixes.is_null() {
                let mut i = 0;
                loop {
                    // SAFETY: caller asserts the array is NULL-terminated.
                    let p = unsafe { *inclusive_ns_prefixes.add(i) };
                    if p.is_null() {
                        break;
                    }
                    if let Ok(s) = unsafe { CStr::from_ptr(p as *const c_char) }.to_str() {
                        prefixes.push(s.to_string());
                    }
                    i += 1;
                }
            }
            C14nMode::ExcC14n10 { inclusive_prefixes: prefixes }
        }
        // 0 = C14n10, 2 = C14n11 (we treat as C14n10).
        _ => C14nMode::C14n10,
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

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

    #[test]
    fn c14n_doc_dump_memory_round_trip() {
        let doc = parse(b"<r xmlns=\"urn:demo\"><a id=\"1\"/></r>");
        let mut out: *mut c_uchar = ptr::null_mut();
        let len = unsafe {
            xmlC14NDocDumpMemory(doc, ptr::null_mut(), 0, ptr::null_mut(), 0, &mut out)
        };
        assert!(len > 0);
        assert!(!out.is_null());
        let s = unsafe { CStr::from_ptr(out as *const c_char) }.to_str().unwrap();
        // C14N output includes the namespace declaration and uses
        // self-closing-tag → expanded form.
        assert!(s.contains("xmlns="), "missing namespace decl: {s}");
        assert!(s.contains("<r"), "missing root: {s}");
        unsafe {
            crate::parse::xml_free_impl(out as *mut std::os::raw::c_void);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn c14n_doc_save_to_file() {
        let doc = parse(b"<r/>");
        let tmp = std::env::temp_dir().join("sup-xml-c14n-test.xml");
        let cpath = CString::new(tmp.to_str().unwrap()).unwrap();
        let len = unsafe {
            xmlC14NDocSave(
                doc, ptr::null_mut(), 0, ptr::null_mut(), 0,
                cpath.as_ptr(), 0,
            )
        };
        assert!(len > 0);
        let on_disk = std::fs::read_to_string(&tmp).unwrap();
        assert!(on_disk.contains("<r"));
        let _ = std::fs::remove_file(&tmp);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn c14n_save_to_null_buffer_returns_error() {
        let doc = parse(b"<r/>");
        let rc = unsafe {
            xmlC14NDocSaveTo(
                doc, ptr::null_mut(), 0, ptr::null_mut(), 0, ptr::null_mut(),
            )
        };
        assert_eq!(rc, -1);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn c14n_save_to_streams_into_output_buffer() {
        use crate::outbuf::{
            xmlOutputBufferCreateBuffer, xmlOutputBufferClose, xmlBufferContent,
        };

        let doc = parse(b"<r a=\"1\"><c/></r>");
        let inner_buf = unsafe { crate::outbuf::xmlBufferCreate() };
        assert!(!inner_buf.is_null());
        let outbuf = unsafe { xmlOutputBufferCreateBuffer(inner_buf, ptr::null_mut()) };
        assert!(!outbuf.is_null());

        let rc = unsafe {
            xmlC14NDocSaveTo(
                doc, ptr::null_mut(), 0, ptr::null_mut(), 0, outbuf as *mut c_void,
            )
        };
        assert!(rc > 0, "expected bytes written, got {rc}");

        // Read back the bytes that flowed through the buffer.
        let inner = unsafe { (*outbuf).buffer };
        assert!(!inner.is_null());
        let s = unsafe { CStr::from_ptr(xmlBufferContent(inner)) }.to_str().unwrap();
        assert_eq!(s, "<r a=\"1\"><c></c></r>");

        unsafe {
            xmlOutputBufferClose(outbuf);
            crate::outbuf::xmlBufferFree(inner_buf);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn c14n_execute_with_visibility_skips_subtree() {
        use crate::outbuf::{
            xmlOutputBufferCreateBuffer, xmlOutputBufferClose, xmlBufferContent,
        };

        // Callback: hide any element whose name starts with `s` (so
        // `<secret>` and its subtree are excluded, but the surrounding
        // tree streams through normally).
        unsafe extern "C" fn hide_s_elements(
            _ud: *mut c_void,
            node: *mut Node<'static>,
            _parent: *mut Node<'static>,
        ) -> c_int {
            if node.is_null() { return 1; }
            // SAFETY: callback only receives valid node pointers from
            // the canonicalizer.
            let n = unsafe { &*node };
            // Type discriminator: only filter elements.
            if n.kind as u32 != sup_xml_tree::dom::NodeKind::Element as u32 {
                return 1;
            }
            if n.name().starts_with('s') { 0 } else { 1 }
        }

        let doc = parse(b"<r><keep/><secret><inner/></secret><also/></r>");
        let inner_buf = unsafe { crate::outbuf::xmlBufferCreate() };
        assert!(!inner_buf.is_null());
        let outbuf = unsafe { xmlOutputBufferCreateBuffer(inner_buf, ptr::null_mut()) };
        let rc = unsafe {
            xmlC14NExecute(
                doc,
                Some(hide_s_elements),
                ptr::null_mut(),
                0,
                ptr::null_mut(),
                0,
                outbuf,
            )
        };
        assert!(rc > 0);

        let inner = unsafe { (*outbuf).buffer };
        let s = unsafe { CStr::from_ptr(xmlBufferContent(inner)) }.to_str().unwrap();
        assert_eq!(s, "<r><keep></keep><also></also></r>");
        assert!(!s.contains("secret"), "subtree skip leaked: {s}");
        assert!(!s.contains("inner"),  "subtree skip leaked: {s}");

        unsafe {
            xmlOutputBufferClose(outbuf);
            crate::outbuf::xmlBufferFree(inner_buf);
            xmlFreeDoc(doc);
        }
    }
}
