//! `xmlSave*` — libxml2's modern streaming serialization API.
//!
//! Wraps a destination sink (file path, raw fd, in-memory buffer) +
//! formatting options behind an opaque `xmlSaveCtxt*` handle.
//! Callers create a context, push one or more docs / subtrees
//! through it with `xmlSaveDoc` / `xmlSaveTree`, then `xmlSaveClose`
//! to finalise and free.
//!
//! Implementation: serialization is delegated to
//! `sup_xml_core::serializer::serialize_with`, which produces a
//! `String` per `xmlSaveDoc` call.  We then push the bytes through
//! the sink.  This means a doc-at-a-time materialisation in memory —
//! the API is streaming in the *interface* sense (no caller-side
//! buffering, sink decides where bytes go) but not in the *parser*
//! sense (the whole serialised doc passes through a heap buffer
//! before reaching the sink).  Real streaming would require teaching
//! the serializer to push to an `&mut dyn Write` instead of building
//! a `String`; tracked as a follow-up.
//!
//! Not implemented in v1:
//!   - **Encoding conversion** — the `encoding` parameter is
//!     accepted but ignored; output is always UTF-8.  libxml2 would
//!     transcode through iconv here; we don't ship a transcoder on
//!     the output path.
//!   - **`xmlSaveSetEscape` / `xmlSaveSetAttrEscape`** — custom
//!     escape callbacks.  Returns -1 if called.
//!   - **`XML_SAVE_NO_EMPTY`** — force `<a></a>` over `<a/>`; our
//!     serializer doesn't have this knob yet.

use std::ffi::CStr;
use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::raw::{c_char, c_int, c_long, c_void};
use std::ptr;

use sup_xml_core::serializer::{
    serialize_node_to_string, serialize_with, SerializeOptions,
};
use sup_xml_tree::dom::{Node, XmlDoc};

// ── xmlSaveOption bits (xmlsave.h) ──────────────────────────────────────────

/// Pretty-print with newlines and indentation.
const XML_SAVE_FORMAT:   c_int = 1 << 0;
/// Suppress the `<?xml ?>` declaration line.
const XML_SAVE_NO_DECL:  c_int = 1 << 1;
/// Force `<a></a>` over `<a/>` (not honoured in v1).
const _XML_SAVE_NO_EMPTY: c_int = 1 << 2;
/// Force XHTML serialization rules (`xhtmlNodeDumpOutput`).
const XML_SAVE_XHTML:    c_int = 1 << 4;
/// HTML output mode (boolean attributes, void elements, no XML decl).
const XML_SAVE_AS_HTML:  c_int = 1 << 6;
// Other XML_SAVE_* bits (NO_XHTML, AS_XML, WS_NON_SIG) are silently
// accepted-and-ignored; they're nuances of HTML/XHTML serialisation that
// don't map cleanly onto SerializeOptions yet.

fn opts_from_bits(bits: c_int) -> SerializeOptions {
    SerializeOptions {
        write_xml_decl: (bits & XML_SAVE_NO_DECL) == 0,
        format:         (bits & XML_SAVE_FORMAT)  != 0,
        indent:         if (bits & XML_SAVE_FORMAT) != 0 { "  ".to_string() } else { String::new() },
        html_mode:      (bits & XML_SAVE_AS_HTML)  != 0,
        xhtml:          (bits & XML_SAVE_XHTML)    != 0,
        out_charset:    sup_xml_core::output::OutputCharset::Utf8,
    }
}

// ── opaque context ──────────────────────────────────────────────────────────

/// Opaque save context.  C side sees a `xmlSaveCtxt*` pointer; the
/// struct is heap-allocated via `Box::into_raw` and reclaimed by
/// `xmlSaveClose`.
pub struct XmlSaveCtxt {
    sink:    Sink,
    opts:    SerializeOptions,
    /// Running byte count across all writes — `xmlSaveClose` returns
    /// this (libxml2's documented return shape).
    written: u64,
    /// Latches on the first I/O failure.  Once set, every subsequent
    /// write/flush/close returns -1, matching libxml2's sticky error
    /// behaviour.
    error:   bool,
}

/// Caller-supplied write callback: `(context, buffer, len) -> written`
/// (or `-1` on error), matching libxml2's `xmlOutputWriteCallback`.
type IoWriteCb = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
/// Caller-supplied close callback: `(context) -> 0 | -1`.
type IoCloseCb = unsafe extern "C" fn(*mut c_void) -> c_int;

enum Sink {
    /// Owned file; closed when the context drops.
    File(File),
    /// Borrowed raw fd.  Wrapped in `ManuallyDrop<File>` so we get
    /// `Write` for free without taking ownership — `xmlSaveClose`
    /// must NOT close the fd (matches libxml2: fd ownership stays
    /// with the caller).
    Fd(ManuallyDrop<File>),
    /// Caller-owned `xmlBuffer`; serialized bytes are appended to it.
    /// The buffer's lifetime is the caller's responsibility.
    Buffer(*mut crate::outbuf::xmlBuffer),
    /// Caller-supplied write/close callbacks and opaque context, fired
    /// per chunk.  `close` runs on `xmlSaveClose`; the context is never
    /// touched by us beyond passing it back.
    Io { write: IoWriteCb, close: Option<IoCloseCb>, ctx: *mut c_void },
}

// ── construction ────────────────────────────────────────────────────────────

/// libxml2 `xmlSaveToFilename(filename, encoding, options)` — open
/// `filename` for writing and return a save context bound to it.
///
/// The file is created (truncating any existing one) when the
/// context is constructed.  `encoding` is accepted but ignored —
/// output is always UTF-8.
///
/// # Safety
///
/// `filename` must be a NUL-terminated C string or NULL.  `encoding`
/// may be NULL or NUL-terminated; we don't read its contents either
/// way.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveToFilename(
    filename:  *const c_char,
    _encoding: *const c_char,
    options:   c_int,
) -> *mut XmlSaveCtxt {
    if filename.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts NUL-terminated; CStr scans for the NUL.
    let path = match unsafe { CStr::from_ptr(filename) }.to_str() {
        Ok(s) => crate::outbuf::local_path_from_file_uri(s),
        Err(_) => return ptr::null_mut(),
    };
    let f = match File::create(&*path) {
        Ok(f) => f,
        Err(_) => return ptr::null_mut(),
    };
    Box::into_raw(Box::new(XmlSaveCtxt {
        sink:    Sink::File(f),
        opts:    opts_from_bits(options),
        written: 0,
        error:   false,
    }))
}

/// libxml2 `xmlSaveToFd(fd, encoding, options)` — bind a save
/// context to an already-open file descriptor.  Ownership of `fd`
/// stays with the caller; `xmlSaveClose` does not call `close()` on
/// it (matches libxml2's documented contract).
///
/// # Safety
///
/// `fd` must be a writable descriptor that stays open for the
/// lifetime of the returned context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveToFd(
    fd:        c_int,
    _encoding: *const c_char,
    options:   c_int,
) -> *mut XmlSaveCtxt {
    // borrow_fd returns None on a closed fd; the wrapper keeps the
    // caller's descriptor open (no `close()`) when Sink::Fd drops.
    let Some(f) = crate::rawfd::borrow_fd(fd) else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(XmlSaveCtxt {
        sink:    Sink::Fd(f),
        opts:    opts_from_bits(options),
        written: 0,
        error:   false,
    }))
}

/// libxml2 `xmlSaveToBuffer(buffer, encoding, options)` — bind a save
/// context to a caller-supplied `xmlBuffer`; serialized bytes are
/// appended to it.  `encoding` is accepted but ignored (output is
/// UTF-8).  Returns NULL on a NULL buffer.
///
/// # Safety
///
/// `buffer` must be NULL or a live `xmlBuffer` that outlives the
/// returned context.  `encoding` may be NULL or NUL-terminated.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveToBuffer(
    buffer:    *mut c_void,
    _encoding: *const c_char,
    options:   c_int,
) -> *mut XmlSaveCtxt {
    if buffer.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(XmlSaveCtxt {
        sink:    Sink::Buffer(buffer as *mut crate::outbuf::xmlBuffer),
        opts:    opts_from_bits(options),
        written: 0,
        error:   false,
    }))
}

/// libxml2 `xmlSaveToIO(iowrite, ioclose, ioctx, encoding, options)` —
/// bind a save context to caller-supplied write/close callbacks.  Each
/// serialized chunk is passed to `iowrite(ioctx, buf, len)`; `ioclose`
/// (if non-NULL) fires on `xmlSaveClose`.  `encoding` is accepted but
/// ignored (output is UTF-8).  Returns NULL when `iowrite` is NULL.
///
/// # Safety
///
/// `iowrite` must be NULL or a valid `xmlOutputWriteCallback`; `ioclose`
/// NULL or a valid `xmlOutputCloseCallback`.  `ioctx` is passed back to
/// both verbatim and must stay valid for the context's lifetime.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveToIO(
    iowrite:   *mut c_void,
    ioclose:   *mut c_void,
    ioctx:     *mut c_void,
    _encoding: *const c_char,
    options:   c_int,
) -> *mut XmlSaveCtxt {
    if iowrite.is_null() { return ptr::null_mut(); }
    // SAFETY: the C ABI passes these as `void*`; libxml2's header types
    // them as `xmlOutputWriteCallback` / `xmlOutputCloseCallback`, which
    // are exactly `IoWriteCb` / `IoCloseCb`.
    let write: IoWriteCb = unsafe { std::mem::transmute(iowrite) };
    let close: Option<IoCloseCb> = if ioclose.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute(ioclose) })
    };
    Box::into_raw(Box::new(XmlSaveCtxt {
        sink:    Sink::Io { write, close, ctx: ioctx },
        opts:    opts_from_bits(options),
        written: 0,
        error:   false,
    }))
}

// ── writes ──────────────────────────────────────────────────────────────────

/// libxml2 `xmlSaveDoc(ctxt, doc)` — serialise `doc` into the
/// context's sink and return the number of bytes written, or -1 on
/// error.  Multiple `xmlSaveDoc` calls on the same context
/// concatenate into the sink.
///
/// # Safety
///
/// `ctxt` must be a pointer returned by an `xmlSaveTo*` constructor
/// and not yet `xmlSaveClose`d.  `doc` must be a valid `xmlDoc`
/// pointer or NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveDoc(
    ctxt: *mut XmlSaveCtxt,
    doc:  *mut XmlDoc,
) -> c_long {
    if ctxt.is_null() || doc.is_null() { return -1; }
    // SAFETY: caller asserts both pointers are valid + uniquely owned
    // for this call.  We hold the &mut for the duration of the write.
    let c = unsafe { &mut *ctxt };
    if c.error { return -1; }
    let d = unsafe { &*doc };
    let s = serialize_with(&d._doc, &c.opts);
    write_chunk(c, s.as_bytes())
}

/// libxml2 `xmlSaveTree(ctxt, node)` — serialise a single subtree
/// without the surrounding XML declaration.  Bytes written, or -1.
///
/// # Safety
///
/// `ctxt` and `node` must each be valid + non-NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveTree(
    ctxt: *mut XmlSaveCtxt,
    cur:  *mut Node<'static>,
) -> c_long {
    if ctxt.is_null() || cur.is_null() { return -1; }
    // SAFETY: see xmlSaveDoc.
    let c = unsafe { &mut *ctxt };
    if c.error { return -1; }
    let n = unsafe { &*cur };
    // A subtree never carries a `<?xml ?>` declaration regardless of
    // the context's options (libxml2 behaviour: NO_DECL is implicit
    // for xmlSaveTree).
    let mut opts = c.opts.clone();
    opts.write_xml_decl = false;
    let s = serialize_node_to_string(n, &opts);
    write_chunk(c, s.as_bytes())
}

fn write_chunk(c: &mut XmlSaveCtxt, bytes: &[u8]) -> c_long {
    let ok = match &mut c.sink {
        Sink::File(f) => f.write_all(bytes).is_ok(),
        // ManuallyDrop<File> derefs to File, which impls Write.
        Sink::Fd(f)   => (&mut **f).write_all(bytes).is_ok(),
        // SAFETY: `Sink::Buffer` holds a live caller-owned xmlBuffer.
        Sink::Buffer(buf) => { unsafe { crate::outbuf::buffer_append(*buf, bytes); } true }
        Sink::Io { write, ctx, .. } => unsafe { write_via_io(*write, *ctx, bytes) },
    };
    if !ok {
        c.error = true;
        return -1;
    }
    c.written += bytes.len() as u64;
    bytes.len() as c_long
}

/// Drive a caller's write callback to completion, looping over partial
/// writes and chunking at `c_int::MAX`.  Returns `false` on a callback
/// error (`<= 0` return) so the caller can latch the sticky error.
///
/// # Safety
/// `write` must be a valid callback and `ctx` the context it expects.
unsafe fn write_via_io(write: IoWriteCb, ctx: *mut c_void, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let chunk = bytes.len().min(c_int::MAX as usize);
        let n = unsafe { write(ctx, bytes.as_ptr() as *const c_char, chunk as c_int) };
        if n <= 0 {
            return false; // callback error or no forward progress
        }
        bytes = &bytes[(n as usize).min(bytes.len())..];
    }
    true
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// libxml2 `xmlSaveFlush(ctxt)` — flush any sink-side buffering.
/// Returns 0 on success, -1 on error.
///
/// # Safety
///
/// `ctxt` must be a valid live context pointer or NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFlush(ctxt: *mut XmlSaveCtxt) -> c_int {
    if ctxt.is_null() { return -1; }
    // SAFETY: caller asserts the pointer.
    let c = unsafe { &mut *ctxt };
    if c.error { return -1; }
    // Buffer/Io sinks write each chunk eagerly in `write_chunk`, so
    // there is nothing staged to flush for them.
    let res = match &mut c.sink {
        Sink::File(f) => f.flush(),
        Sink::Fd(f)   => (&mut **f).flush(),
        Sink::Buffer(_) | Sink::Io { .. } => Ok(()),
    };
    if res.is_err() { c.error = true; -1 } else { 0 }
}

/// libxml2 `xmlSaveClose(ctxt)` — flush, close, and free the
/// context.  Returns the cumulative byte count written, or -1 if the
/// context ever hit an error.
///
/// # Safety
///
/// `ctxt` must be a valid live context pointer or NULL.  After this
/// call the pointer is invalid; reusing it is undefined behaviour
/// (matches libxml2).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveClose(ctxt: *mut XmlSaveCtxt) -> c_int {
    if ctxt.is_null() { return -1; }
    // SAFETY: we only hand out pointers via `Box::into_raw`; this
    // reclaims one.  Box drop runs the destructor — including
    // closing the owned File for Sink::File and *not* closing the
    // fd for Sink::Fd (ManuallyDrop suppresses it).
    let mut c = unsafe { Box::from_raw(ctxt) };
    // Best-effort flush, then fire the caller's close callback for an
    // Io sink (libxml2 calls `ioclose` exactly once, on close).  We
    // continue teardown even on failure so resources still release.
    match &mut c.sink {
        Sink::File(f) => { let _ = f.flush(); }
        Sink::Fd(f)   => { let _ = (&mut **f).flush(); }
        Sink::Buffer(_) => {}
        Sink::Io { close, ctx, .. } => {
            if let Some(close_fn) = close {
                // SAFETY: `close_fn`/`ctx` are the caller's own callback
                // and context, valid for the context's lifetime.
                if unsafe { close_fn(*ctx) } < 0 {
                    c.error = true;
                }
            }
        }
    }
    if c.error { -1 } else { c.written as c_int }
}

/// libxml2 `xmlSaveFinish(ctxt)` — newer (2.13+) flush+close that
/// returns an `xmlParserErrors` code rather than a byte count.  We
/// implement it by delegating to `xmlSaveClose` and mapping the
/// outcome onto `XML_ERR_OK` / `XML_ERR_INTERNAL_ERROR`.
///
/// # Safety
///
/// Same as [`xmlSaveClose`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFinish(ctxt: *mut XmlSaveCtxt) -> c_int {
    // SAFETY: same contract as xmlSaveClose.
    let rc = unsafe { xmlSaveClose(ctxt) };
    if rc < 0 { 1 /* XML_ERR_INTERNAL_ERROR */ } else { 0 /* XML_ERR_OK */ }
}

// ── option tweaks ───────────────────────────────────────────────────────────

/// libxml2 `xmlSaveSetIndentString(ctxt, indent)` — replace the
/// indent string used when `XML_SAVE_FORMAT` is on.  Returns 0 on
/// success, -1 on invalid args.
///
/// # Safety
///
/// `ctxt` must be a live context; `indent` must be a NUL-terminated
/// C string or NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveSetIndentString(
    ctxt:   *mut XmlSaveCtxt,
    indent: *const c_char,
) -> c_int {
    if ctxt.is_null() || indent.is_null() { return -1; }
    // SAFETY: caller asserts both pointers.
    let c = unsafe { &mut *ctxt };
    let s = match unsafe { CStr::from_ptr(indent) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    c.opts.indent = s;
    0
}

/// libxml2 `xmlSaveSetEscape(ctxt, escape)` — install a custom
/// character-escape callback.  Not implemented; returns -1.
///
/// # Safety
///
/// Inputs unused.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveSetEscape(
    _ctxt: *mut XmlSaveCtxt,
    _esc:  *mut c_void,
) -> c_int { -1 }

/// libxml2 `xmlSaveSetAttrEscape(ctxt, escape)` — install a custom
/// attribute-value escape callback.  Not implemented; returns -1.
///
/// # Safety
///
/// Inputs unused.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveSetAttrEscape(
    _ctxt: *mut XmlSaveCtxt,
    _esc:  *mut c_void,
) -> c_int { -1 }

// ── one-shot file/memory dump helpers (PHP-needed) ─────────────────────

/// libxml2 `xmlSaveFormatFileEnc(filename, doc, encoding, format)` —
/// serialize `doc` to `filename`, optionally with indented output.
///
/// `encoding` is accepted for API parity — our serializer always
/// emits UTF-8, so the resulting file declares whatever the doc
/// declared at parse time.  Returns the number of bytes written or
/// -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFormatFileEnc(
    filename:  *const c_char,
    doc:       *const sup_xml_tree::dom::XmlDoc,
    _encoding: *const c_char,
    format:    c_int,
) -> c_int {
    if filename.is_null() || doc.is_null() { return -1; }
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(s)  => s,
        Err(_) => return -1,
    };
    // Dump to memory first, then write the file.
    let mut mem:  *mut c_char = ptr::null_mut();
    let mut size: c_int       = 0;
    unsafe { crate::serialize::xmlDocDumpFormatMemory(doc, &mut mem, &mut size, format); }
    if mem.is_null() || size <= 0 { return -1; }
    // SAFETY: serializer returns a valid C string of `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size as usize) };
    let result = std::fs::write(path, bytes);
    unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
    match result {
        Ok(())  => size,
        Err(_)  => -1,
    }
}

/// libxml2 `xmlSaveFile(filename, doc)` — legacy save entry point.
/// Equivalent to `xmlSaveFormatFileEnc(filename, doc, NULL, 0)`.
/// Returns the byte count written or -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFile(
    filename: *const c_char,
    doc:      *const sup_xml_tree::dom::XmlDoc,
) -> c_int {
    unsafe { xmlSaveFormatFileEnc(filename, doc, ptr::null(), 0) }
}

/// libxml2 `xmlSaveFormatFile(filename, doc, format)` — legacy save
/// with pretty-printing toggle.  Equivalent to
/// `xmlSaveFormatFileEnc(filename, doc, NULL, format)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFormatFile(
    filename: *const c_char,
    doc:      *const sup_xml_tree::dom::XmlDoc,
    format:   c_int,
) -> c_int {
    unsafe { xmlSaveFormatFileEnc(filename, doc, ptr::null(), format) }
}

/// libxml2 `xmlSaveFormatFileTo(buf, doc, encoding, format)` —
/// serialize `doc` into the supplied `xmlOutputBuffer`.  Whatever the
/// buffer was created with (in-memory, file-backed, user IO callback)
/// receives the bytes.  Returns the byte count written or -1 on error.
///
/// `encoding` is accepted for API parity but ignored — output is
/// always UTF-8.  The buffer takes ownership of its sink and is NOT
/// closed by this call; the caller releases via
/// `xmlOutputBufferClose`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveFormatFileTo(
    buf:       *mut crate::outbuf::xmlOutputBuffer,
    doc:       *const sup_xml_tree::dom::XmlDoc,
    _encoding: *const c_char,
    format:    c_int,
) -> c_int {
    if buf.is_null() || doc.is_null() { return -1; }
    // Dump to memory, then pipe through the output buffer.  Streaming
    // straight out of the serializer would be a separate refactor on
    // crate::serialize; the buffer's destination (file / user IO /
    // memory) still gets bytes as a single write.
    let mut mem:  *mut c_char = ptr::null_mut();
    let mut size: c_int       = 0;
    unsafe { crate::serialize::xmlDocDumpFormatMemory(doc, &mut mem, &mut size, format); }
    if mem.is_null() || size <= 0 { return -1; }
    let written = unsafe {
        crate::outbuf::xmlOutputBufferWrite(buf, size, mem as *const c_char)
    };
    unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
    if written < 0 { -1 } else { written }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;

    fn parse(src: &[u8]) -> *mut XmlDoc {
        // SAFETY: borrowed slice → null-terminated url/encoding.
        unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        }
    }

    #[test]
    fn save_to_filename_writes_doc() {
        let tmp = std::env::temp_dir().join(format!("xmlsave_test_{}.xml", std::process::id()));
        let cstr = CString::new(tmp.to_str().unwrap()).unwrap();
        let doc = parse(b"<r><a/></r>");
        assert!(!doc.is_null());

        // SAFETY: filename is NUL-terminated; doc came from xmlReadMemory.
        let ctx = unsafe { xmlSaveToFilename(cstr.as_ptr(), ptr::null(), 0) };
        assert!(!ctx.is_null());
        let n = unsafe { xmlSaveDoc(ctx, doc) };
        assert!(n > 0, "xmlSaveDoc returned {n}");
        let rc = unsafe { xmlSaveClose(ctx) };
        assert!(rc >= 0);

        let written = fs::read_to_string(&tmp).expect("output file");
        assert!(written.contains("<r"), "output={written:?}");
        assert!(written.contains("<a"), "output={written:?}");
        let _ = fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_to_fd_writes_and_does_not_close_fd() {
        use crate::rawfd::testfd;
        // Use a temp file, get its fd, ensure xmlSaveClose doesn't
        // close it (we can still write to it afterwards).
        let tmp = std::env::temp_dir().join(format!("xmlsave_fd_{}.xml", std::process::id()));
        let fd = testfd::open_w(&tmp);

        let doc = parse(b"<r/>");
        // SAFETY: fd is open + writable for this scope.
        let ctx = unsafe { xmlSaveToFd(fd, ptr::null(), 0) };
        let n = unsafe { xmlSaveDoc(ctx, doc) };
        assert!(n > 0);
        unsafe { xmlSaveClose(ctx); }

        // fd must still be writable — xmlSaveClose must not have closed it.
        let w = testfd::write(fd, b"<!-- after -->");
        assert_eq!(w, 14, "fd should remain open and writable");

        testfd::close(fd);
        let _ = fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_format_flag_pretty_prints() {
        let tmp = std::env::temp_dir().join(format!("xmlsave_fmt_{}.xml", std::process::id()));
        let cstr = CString::new(tmp.to_str().unwrap()).unwrap();
        let doc = parse(b"<r><a/><b/></r>");
        let ctx = unsafe { xmlSaveToFilename(cstr.as_ptr(), ptr::null(), XML_SAVE_FORMAT) };
        let _ = unsafe { xmlSaveDoc(ctx, doc) };
        unsafe { xmlSaveClose(ctx); }
        let s = fs::read_to_string(&tmp).unwrap();
        // Pretty-print inserts newlines between siblings.
        assert!(s.contains('\n'), "format output should contain newlines, got {s:?}");
        let _ = fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_no_decl_suppresses_xml_declaration() {
        let tmp = std::env::temp_dir().join(format!("xmlsave_nodecl_{}.xml", std::process::id()));
        let cstr = CString::new(tmp.to_str().unwrap()).unwrap();
        let doc = parse(b"<r/>");
        let ctx = unsafe { xmlSaveToFilename(cstr.as_ptr(), ptr::null(), XML_SAVE_NO_DECL) };
        let _ = unsafe { xmlSaveDoc(ctx, doc) };
        unsafe { xmlSaveClose(ctx); }
        let s = fs::read_to_string(&tmp).unwrap();
        assert!(!s.contains("<?xml"), "NO_DECL should suppress the XML decl, got {s:?}");
        let _ = fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_close_on_null_returns_minus_one() {
        let rc = unsafe { xmlSaveClose(ptr::null_mut()) };
        assert_eq!(rc, -1);
    }

    #[test]
    fn save_to_buffer_and_io_reject_null() {
        // A NULL buffer / NULL write callback has no destination.
        let b = unsafe { xmlSaveToBuffer(ptr::null_mut(), ptr::null(), 0) };
        assert!(b.is_null());
        let i = unsafe { xmlSaveToIO(ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null(), 0) };
        assert!(i.is_null());
    }

    #[test]
    fn save_to_buffer_writes_doc() {
        use crate::outbuf::{xmlBufferContent, xmlBufferCreate, xmlBufferFree};
        let buf = unsafe { xmlBufferCreate() };
        let doc = parse(b"<r><a/></r>");
        let ctx = unsafe { xmlSaveToBuffer(buf as *mut c_void, ptr::null(), 0) };
        assert!(!ctx.is_null());
        let n = unsafe { xmlSaveDoc(ctx, doc) };
        assert!(n > 0, "xmlSaveDoc returned {n}");
        let rc = unsafe { xmlSaveClose(ctx) };
        assert!(rc >= 0);
        let s = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(buf)) }.to_str().unwrap();
        assert!(s.contains("<r"), "buffer={s:?}");
        assert!(s.contains("<a"), "buffer={s:?}");
        unsafe { xmlBufferFree(buf); crate::parse::xmlFreeDoc(doc); }
    }

    struct IoCapture { bytes: Vec<u8>, closed: bool }

    unsafe extern "C" fn cap_write(ctx: *mut c_void, buf: *const c_char, len: c_int) -> c_int {
        let cap = unsafe { &mut *(ctx as *mut IoCapture) };
        let slice = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
        cap.bytes.extend_from_slice(slice);
        len
    }

    unsafe extern "C" fn cap_close(ctx: *mut c_void) -> c_int {
        let cap = unsafe { &mut *(ctx as *mut IoCapture) };
        cap.closed = true;
        0
    }

    #[test]
    fn save_to_io_invokes_write_and_close() {
        let mut cap = IoCapture { bytes: Vec::new(), closed: false };
        let ctx_ptr = &mut cap as *mut IoCapture as *mut c_void;
        let doc = parse(b"<r><a/></r>");
        let ctx = unsafe {
            xmlSaveToIO(
                cap_write as *mut c_void,
                cap_close as *mut c_void,
                ctx_ptr,
                ptr::null(),
                0,
            )
        };
        assert!(!ctx.is_null());
        let n = unsafe { xmlSaveDoc(ctx, doc) };
        assert!(n > 0);
        let rc = unsafe { xmlSaveClose(ctx) };
        assert!(rc >= 0);
        let s = String::from_utf8(cap.bytes.clone()).unwrap();
        assert!(s.contains("<r"), "io output={s:?}");
        assert!(s.contains("<a"), "io output={s:?}");
        assert!(cap.closed, "ioclose callback should fire on xmlSaveClose");
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_file_legacy_writes_to_disk() {
        let tmp = std::env::temp_dir().join(format!("xmlsave_legacy_{}.xml", std::process::id()));
        let cstr = CString::new(tmp.to_str().unwrap()).unwrap();
        let doc = parse(b"<r><a id=\"1\"/></r>");
        let n = unsafe { xmlSaveFile(cstr.as_ptr(), doc) };
        assert!(n > 0);
        let s = fs::read_to_string(&tmp).unwrap();
        assert!(s.contains("<r"));
        assert!(s.contains("id=\"1\""));
        let _ = fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn save_format_file_to_streams_through_output_buffer() {
        use crate::outbuf::{xmlBufferContent, xmlBufferCreate, xmlBufferFree,
                            xmlOutputBufferClose, xmlOutputBufferCreateBuffer};
        let inner = unsafe { xmlBufferCreate() };
        let outbuf = unsafe { xmlOutputBufferCreateBuffer(inner, ptr::null_mut()) };
        let doc = parse(b"<r><a/></r>");
        let n = unsafe { xmlSaveFormatFileTo(outbuf, doc, ptr::null(), 0) };
        assert!(n > 0, "bytes returned = {n}");
        let s = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(inner)) }.to_str().unwrap();
        assert!(s.contains("<r"));
        unsafe { xmlOutputBufferClose(outbuf); xmlBufferFree(inner); crate::parse::xmlFreeDoc(doc); }
    }
}
