//! libxml2's `xmlTextWriter` streaming API.
//!
//! Mirrors the libxml2 surface PHP's `XMLWriter` extension is built
//! on.  Outputs XML to an `xmlOutputBuffer` (typically backed by an
//! in-memory `xmlBuffer` for `XMLWriter::openMemory()`, or a file
//! handle for `XMLWriter::openFile()`).
//!
//! ## State machine
//!
//! - After `StartElement(name)` we've emitted `<name` but **not** the
//!   closing `>` yet — that lets follow-up `StartAttribute` /
//!   `WriteAttribute` calls extend the open tag.  The first call that
//!   adds content (`WriteString`, `StartElement` for a child, etc.)
//!   triggers the close `>`.
//! - `EndElement` decides between self-closing `/>` (no children
//!   emitted) and `</name>` based on the frame's `has_content` flag.
//! - `FullEndElement` always emits `</name>` even if no content (the
//!   "preserve the open/close pair" variant).

use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::outbuf::xmlOutputBuffer;

/// Opaque writer handle returned to C callers.
#[allow(non_camel_case_types)]
pub struct xmlTextWriter {
    /// Underlying output buffer.  Owned by the writer; `xmlFreeTextWriter`
    /// closes + frees it.
    out: *mut xmlOutputBuffer,
    state: RefCell<WriterState>,
}

struct WriterState {
    stack: Vec<ElemFrame>,
    /// True iff the most recent StartElement is still open (`<foo` without `>`).
    pending_close_angle: bool,
    /// Name of the attribute being built piecewise via Start/EndAttribute.
    in_attribute: Option<String>,
    indent: bool,
    indent_str: String,
}

struct ElemFrame {
    name: String,
    /// True iff this element has emitted at least one child / text node.
    /// Decides between `/>` and `</name>` on `EndElement`.
    has_content: bool,
}

impl Default for WriterState {
    fn default() -> Self {
        Self {
            stack: Vec::new(),
            pending_close_angle: false,
            in_attribute: None,
            indent: false,
            indent_str: String::new(),
        }
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

/// Write raw bytes to the underlying output buffer.  Returns the
/// number of bytes consumed by the buffer, or -1 on error.
fn write_raw_bytes(w: &xmlTextWriter, bytes: &[u8]) -> c_int {
    if w.out.is_null() || bytes.is_empty() {
        return 0;
    }
    unsafe {
        crate::outbuf::xmlOutputBufferWrite(w.out, bytes.len() as c_int, bytes.as_ptr() as *const c_char)
    }
}

/// Close the open `<name` with `>` if we have one pending.  Idempotent.
fn flush_pending_close_angle(w: &xmlTextWriter) {
    let needs = w.state.borrow().pending_close_angle;
    if needs {
        write_raw_bytes(w, b">");
        w.state.borrow_mut().pending_close_angle = false;
        // Mark the now-current element as having content from the
        // caller's perspective (its `>` is emitted, so the next
        // operation is a child).
        if let Some(top) = w.state.borrow_mut().stack.last_mut() {
            top.has_content = true;
        }
    }
}

/// Emit indent for the current depth (count of open elements).
fn write_indent(w: &xmlTextWriter) {
    let s = w.state.borrow();
    if !s.indent || s.indent_str.is_empty() { return; }
    let depth = s.stack.len();
    let ind = s.indent_str.clone();
    drop(s);
    write_raw_bytes(w, b"\n");
    for _ in 0..depth {
        write_raw_bytes(w, ind.as_bytes());
    }
}

/// Escape `&`, `<`, `>`, `"`, `'` for content / attribute values.
fn push_escaped(s: &str, in_attr: bool, out: &mut Vec<u8>) {
    for b in s.bytes() {
        match b {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'"' if in_attr => out.extend_from_slice(b"&quot;"),
            _    => out.push(b),
        }
    }
}

/// Borrow the writer through a raw pointer with a NULL check.
fn writer_ref<'a>(w: *mut xmlTextWriter) -> Option<&'a xmlTextWriter> {
    if w.is_null() { None } else { Some(unsafe { &*w }) }
}

/// Convert a NUL-terminated C string to a Rust &str.  Returns None on
/// NULL or invalid UTF-8.
unsafe fn cstr_opt<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() { return None; }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

// ── lifecycle ───────────────────────────────────────────────────────────

/// `xmlNewTextWriter(out)` — wrap an existing output buffer as a
/// writer.  Takes ownership of `out`; `xmlFreeTextWriter` releases it.
/// NULL on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewTextWriter(out: *mut xmlOutputBuffer) -> *mut xmlTextWriter {
    if out.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(xmlTextWriter {
        out,
        state: RefCell::new(WriterState::default()),
    }))
}

/// `xmlNewTextWriterFilename(uri, compression)` — open a file for
/// writing.  `compression` is ignored (no gzip in this build).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewTextWriterFilename(
    uri:         *const c_char,
    _compression: c_int,
) -> *mut xmlTextWriter {
    if uri.is_null() { return ptr::null_mut(); }
    let out = unsafe { crate::outbuf::xmlOutputBufferCreateFilename(uri, ptr::null_mut(), 0) };
    if out.is_null() { return ptr::null_mut(); }
    unsafe { xmlNewTextWriter(out) }
}

/// `xmlFreeTextWriter(writer)` — close the writer and the underlying
/// output buffer.  Idempotent on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeTextWriter(writer: *mut xmlTextWriter) {
    if writer.is_null() { return; }
    // SAFETY: writer came from Box::into_raw above.
    let w = unsafe { Box::from_raw(writer) };
    if !w.out.is_null() {
        unsafe { crate::outbuf::xmlOutputBufferClose(w.out); }
    }
}

// ── document scaffolding ────────────────────────────────────────────────

/// `xmlTextWriterStartDocument(version, encoding, standalone)`.
/// Writes the XML declaration.  Args may all be NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartDocument(
    writer:     *mut xmlTextWriter,
    version:    *const c_char,
    encoding:   *const c_char,
    standalone: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let v = unsafe { cstr_opt(version) }.unwrap_or("1.0");
    let e = unsafe { cstr_opt(encoding) }.unwrap_or("UTF-8");
    let mut s = format!("<?xml version=\"{v}\" encoding=\"{e}\"");
    if let Some(sa) = unsafe { cstr_opt(standalone) } {
        s.push_str(&format!(" standalone=\"{sa}\""));
    }
    s.push_str("?>");
    write_raw_bytes(w, s.as_bytes())
}

/// `xmlTextWriterEndDocument(writer)` — close all still-open elements
/// and flush.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndDocument(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let mut total = 0;
    loop {
        let n_open = w.state.borrow().stack.len();
        if n_open == 0 { break; }
        let r = unsafe { xmlTextWriterEndElement(writer) };
        if r < 0 { return -1; }
        total += r;
    }
    let f = unsafe { xmlTextWriterFlush(writer) };
    if f < 0 { return -1; }
    total + f
}

/// `xmlTextWriterFlush(writer)` — force the underlying buffer to its
/// IO sink.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterFlush(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    if w.out.is_null() { return -1; }
    unsafe { crate::outbuf::xmlOutputBufferFlush(w.out) }
}

// ── element scaffolding ────────────────────────────────────────────────

/// `xmlTextWriterStartElement(writer, name)` — open a new element.
/// Emits `<name` without the trailing `>`; subsequent attribute calls
/// extend the open tag.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartElement(
    writer: *mut xmlTextWriter,
    name:   *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let name = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    flush_pending_close_angle(w);
    write_indent(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"<");
    total += write_raw_bytes(w, name.as_bytes());
    let mut s = w.state.borrow_mut();
    s.stack.push(ElemFrame { name: name.to_string(), has_content: false });
    s.pending_close_angle = true;
    total
}

/// `xmlTextWriterStartElementNS(writer, prefix, name, namespace_uri)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartElementNS(
    writer:        *mut xmlTextWriter,
    prefix:        *const c_char,
    name:          *const c_char,
    namespace_uri: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let name = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    let prefix = unsafe { cstr_opt(prefix) };
    let ns_uri = unsafe { cstr_opt(namespace_uri) };
    flush_pending_close_angle(w);
    write_indent(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"<");
    let qname = match prefix {
        Some(p) => format!("{p}:{name}"),
        None    => name.to_string(),
    };
    total += write_raw_bytes(w, qname.as_bytes());
    if let Some(uri) = ns_uri {
        let mut buf = Vec::new();
        match prefix {
            Some(p) => {
                buf.extend_from_slice(b" xmlns:");
                buf.extend_from_slice(p.as_bytes());
                buf.extend_from_slice(b"=\"");
            }
            None => buf.extend_from_slice(b" xmlns=\""),
        }
        push_escaped(uri, /*in_attr=*/ true, &mut buf);
        buf.push(b'"');
        total += write_raw_bytes(w, &buf);
    }
    let mut s = w.state.borrow_mut();
    s.stack.push(ElemFrame { name: qname, has_content: false });
    s.pending_close_angle = true;
    total
}

/// `xmlTextWriterEndElement(writer)` — close the current element.
/// Self-closes (`/>`) if no content was emitted; emits `</name>`
/// otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndElement(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let frame = w.state.borrow_mut().stack.pop();
    let frame = match frame { Some(f) => f, None => return -1 };
    let pending = std::mem::replace(&mut w.state.borrow_mut().pending_close_angle, false);
    if pending {
        // No children emitted — self-close.
        return write_raw_bytes(w, b"/>");
    }
    write_indent(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"</");
    total += write_raw_bytes(w, frame.name.as_bytes());
    total += write_raw_bytes(w, b">");
    total
}

/// `xmlTextWriterFullEndElement(writer)` — close with `</name>` even
/// when the element has no content (the "explicit close" variant).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterFullEndElement(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    flush_pending_close_angle(w);
    let frame = w.state.borrow_mut().stack.pop();
    let frame = match frame { Some(f) => f, None => return -1 };
    write_indent(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"</");
    total += write_raw_bytes(w, frame.name.as_bytes());
    total += write_raw_bytes(w, b">");
    total
}

// ── attributes ──────────────────────────────────────────────────────────

/// `xmlTextWriterStartAttribute(writer, name)` — begin a piecewise
/// attribute.  Emits ` name="`; subsequent `WriteString` / `WriteRaw`
/// goes into the value until `EndAttribute`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartAttribute(
    writer: *mut xmlTextWriter,
    name:   *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let name = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    // We must be inside an open tag (after StartElement, before content).
    if !w.state.borrow().pending_close_angle { return -1; }
    let mut total = 0;
    total += write_raw_bytes(w, b" ");
    total += write_raw_bytes(w, name.as_bytes());
    total += write_raw_bytes(w, b"=\"");
    w.state.borrow_mut().in_attribute = Some(name.to_string());
    total
}

/// `xmlTextWriterStartAttributeNS(writer, prefix, name, namespace_uri)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartAttributeNS(
    writer:        *mut xmlTextWriter,
    prefix:        *const c_char,
    name:          *const c_char,
    _namespace_uri: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let name = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    let prefix = unsafe { cstr_opt(prefix) };
    if !w.state.borrow().pending_close_angle { return -1; }
    let qname = match prefix {
        Some(p) => format!("{p}:{name}"),
        None    => name.to_string(),
    };
    let mut total = 0;
    total += write_raw_bytes(w, b" ");
    total += write_raw_bytes(w, qname.as_bytes());
    total += write_raw_bytes(w, b"=\"");
    w.state.borrow_mut().in_attribute = Some(qname);
    total
}

/// `xmlTextWriterEndAttribute(writer)` — close a piecewise attribute.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndAttribute(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    if w.state.borrow().in_attribute.is_none() { return -1; }
    w.state.borrow_mut().in_attribute = None;
    write_raw_bytes(w, b"\"")
}

/// `xmlTextWriterWriteAttribute(writer, name, content)` — one-shot
/// attribute write: ` name="value"`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteAttribute(
    writer:  *mut xmlTextWriter,
    name:    *const c_char,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartAttribute(writer, name) };
    if total < 0 { return -1; }
    total += unsafe { xmlTextWriterWriteString(writer, content) };
    let e = unsafe { xmlTextWriterEndAttribute(writer) };
    if e < 0 { return -1; }
    total + e
}

/// `xmlTextWriterWriteAttributeNS(writer, prefix, name, namespace_uri, content)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteAttributeNS(
    writer:        *mut xmlTextWriter,
    prefix:        *const c_char,
    name:          *const c_char,
    namespace_uri: *const c_char,
    content:       *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartAttributeNS(writer, prefix, name, namespace_uri) };
    if total < 0 { return -1; }
    total += unsafe { xmlTextWriterWriteString(writer, content) };
    let e = unsafe { xmlTextWriterEndAttribute(writer) };
    if e < 0 { return -1; }
    total + e
}

/// `xmlTextWriterWriteElement(writer, name, content)` — write a
/// complete element with text content.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteElement(
    writer:  *mut xmlTextWriter,
    name:    *const c_char,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartElement(writer, name) };
    if total < 0 { return -1; }
    if !content.is_null() {
        let s = unsafe { xmlTextWriterWriteString(writer, content) };
        if s < 0 { return -1; }
        total += s;
    }
    let e = unsafe { xmlTextWriterEndElement(writer) };
    if e < 0 { return -1; }
    total + e
}

/// `xmlTextWriterWriteElementNS(writer, prefix, name, namespace_uri, content)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteElementNS(
    writer:        *mut xmlTextWriter,
    prefix:        *const c_char,
    name:          *const c_char,
    namespace_uri: *const c_char,
    content:       *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartElementNS(writer, prefix, name, namespace_uri) };
    if total < 0 { return -1; }
    if !content.is_null() {
        let s = unsafe { xmlTextWriterWriteString(writer, content) };
        if s < 0 { return -1; }
        total += s;
    }
    let e = unsafe { xmlTextWriterEndElement(writer) };
    if e < 0 { return -1; }
    total + e
}

// ── text content ────────────────────────────────────────────────────────

/// `xmlTextWriterWriteString(writer, content)` — write character data,
/// applying XML escaping.  Closes any pending open tag.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteString(
    writer:  *mut xmlTextWriter,
    content: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let s = match unsafe { cstr_opt(content) } { Some(s) => s, None => return 0 };
    let in_attr = w.state.borrow().in_attribute.is_some();
    if !in_attr {
        flush_pending_close_angle(w);
    }
    let mut esc = Vec::with_capacity(s.len());
    push_escaped(s, in_attr, &mut esc);
    write_raw_bytes(w, &esc)
}

/// `xmlTextWriterWriteRaw(writer, content)` — write `content` verbatim,
/// no escaping.  Caller is responsible for well-formedness.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteRaw(
    writer:  *mut xmlTextWriter,
    content: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let s = match unsafe { cstr_opt(content) } { Some(s) => s, None => return 0 };
    if w.state.borrow().in_attribute.is_none() {
        flush_pending_close_angle(w);
    }
    write_raw_bytes(w, s.as_bytes())
}

// ── CDATA ───────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartCDATA(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    flush_pending_close_angle(w);
    write_raw_bytes(w, b"<![CDATA[")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndCDATA(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b"]]>")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteCDATA(
    writer:  *mut xmlTextWriter,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartCDATA(writer) };
    if total < 0 { return -1; }
    if !content.is_null() {
        let r = unsafe { xmlTextWriterWriteRaw(writer, content) };
        if r < 0 { return -1; }
        total += r;
    }
    let e = unsafe { xmlTextWriterEndCDATA(writer) };
    if e < 0 { return -1; }
    total + e
}

// ── comments ────────────────────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartComment(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    flush_pending_close_angle(w);
    write_indent(w);
    write_raw_bytes(w, b"<!--")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndComment(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b"-->")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteComment(
    writer:  *mut xmlTextWriter,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartComment(writer) };
    if total < 0 { return -1; }
    if !content.is_null() {
        let r = unsafe { xmlTextWriterWriteRaw(writer, content) };
        if r < 0 { return -1; }
        total += r;
    }
    let e = unsafe { xmlTextWriterEndComment(writer) };
    if e < 0 { return -1; }
    total + e
}

// ── processing instructions ────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartPI(
    writer: *mut xmlTextWriter,
    target: *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let tgt = match unsafe { cstr_opt(target) } { Some(s) => s, None => return -1 };
    flush_pending_close_angle(w);
    write_indent(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"<?");
    total += write_raw_bytes(w, tgt.as_bytes());
    total += write_raw_bytes(w, b" ");
    total
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndPI(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b"?>")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWritePI(
    writer:  *mut xmlTextWriter,
    target:  *const c_char,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartPI(writer, target) };
    if total < 0 { return -1; }
    if !content.is_null() {
        let r = unsafe { xmlTextWriterWriteRaw(writer, content) };
        if r < 0 { return -1; }
        total += r;
    }
    let e = unsafe { xmlTextWriterEndPI(writer) };
    if e < 0 { return -1; }
    total + e
}

// ── DTD (minimal coverage) ─────────────────────────────────────────────

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartDTD(
    writer:    *mut xmlTextWriter,
    name:      *const c_char,
    pubid:     *const c_char,
    sysid:     *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let name = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    flush_pending_close_angle(w);
    let mut total = 0;
    total += write_raw_bytes(w, b"<!DOCTYPE ");
    total += write_raw_bytes(w, name.as_bytes());
    if let Some(p) = unsafe { cstr_opt(pubid) } {
        total += write_raw_bytes(w, format!(" PUBLIC \"{p}\"").as_bytes());
    }
    if let Some(s) = unsafe { cstr_opt(sysid) } {
        total += write_raw_bytes(w, format!(" \"{s}\"").as_bytes());
    }
    total
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndDTD(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b">")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteDTD(
    writer:    *mut xmlTextWriter,
    name:      *const c_char,
    pubid:     *const c_char,
    sysid:     *const c_char,
    subset:    *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartDTD(writer, name, pubid, sysid) };
    if total < 0 { return -1; }
    if let Some(s) = unsafe { cstr_opt(subset) } {
        let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
        total += write_raw_bytes(w, b" [");
        total += write_raw_bytes(w, s.as_bytes());
        total += write_raw_bytes(w, b"]");
    }
    let e = unsafe { xmlTextWriterEndDTD(writer) };
    if e < 0 { return -1; }
    total + e
}

// Element / attlist / entity DTD entries — minimal "name + content".

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartDTDElement(
    writer: *mut xmlTextWriter,
    name:   *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let n = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    let mut total = 0;
    total += write_raw_bytes(w, b"<!ELEMENT ");
    total += write_raw_bytes(w, n.as_bytes());
    total += write_raw_bytes(w, b" ");
    total
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndDTDElement(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b">")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteDTDElement(
    writer:  *mut xmlTextWriter,
    name:    *const c_char,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartDTDElement(writer, name) };
    if total < 0 { return -1; }
    if let Some(s) = unsafe { cstr_opt(content) } {
        let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
        total += write_raw_bytes(w, s.as_bytes());
    }
    let e = unsafe { xmlTextWriterEndDTDElement(writer) };
    if e < 0 { return -1; }
    total + e
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartDTDAttlist(
    writer: *mut xmlTextWriter,
    name:   *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let n = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    let mut total = 0;
    total += write_raw_bytes(w, b"<!ATTLIST ");
    total += write_raw_bytes(w, n.as_bytes());
    total += write_raw_bytes(w, b" ");
    total
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndDTDAttlist(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b">")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteDTDAttlist(
    writer:  *mut xmlTextWriter,
    name:    *const c_char,
    content: *const c_char,
) -> c_int {
    let mut total = unsafe { xmlTextWriterStartDTDAttlist(writer, name) };
    if total < 0 { return -1; }
    if let Some(s) = unsafe { cstr_opt(content) } {
        let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
        total += write_raw_bytes(w, s.as_bytes());
    }
    let e = unsafe { xmlTextWriterEndDTDAttlist(writer) };
    if e < 0 { return -1; }
    total + e
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterStartDTDEntity(
    writer:        *mut xmlTextWriter,
    parameter_ent: c_int,
    name:          *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let n = match unsafe { cstr_opt(name) } { Some(s) => s, None => return -1 };
    let mut total = 0;
    total += write_raw_bytes(w, b"<!ENTITY ");
    if parameter_ent != 0 {
        total += write_raw_bytes(w, b"% ");
    }
    total += write_raw_bytes(w, n.as_bytes());
    total += write_raw_bytes(w, b" \"");
    total
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterEndDTDEntity(writer: *mut xmlTextWriter) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    write_raw_bytes(w, b"\">")
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterWriteDTDEntity(
    writer:        *mut xmlTextWriter,
    parameter_ent: c_int,
    name:          *const c_char,
    pubid:         *const c_char,
    sysid:         *const c_char,
    ndataid:       *const c_char,
    content:       *const c_char,
) -> c_int {
    let _ = (pubid, sysid, ndataid); // External ID variants not fully implemented.
    let mut total = unsafe { xmlTextWriterStartDTDEntity(writer, parameter_ent, name) };
    if total < 0 { return -1; }
    if let Some(s) = unsafe { cstr_opt(content) } {
        let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
        let mut buf = Vec::with_capacity(s.len());
        push_escaped(s, /*in_attr=*/ true, &mut buf);
        total += write_raw_bytes(w, &buf);
    }
    let e = unsafe { xmlTextWriterEndDTDEntity(writer) };
    if e < 0 { return -1; }
    total + e
}

// ── indent settings ────────────────────────────────────────────────────

/// `xmlTextWriterSetIndent(writer, indent)` — toggle indented output.
/// `indent != 0` turns indentation on with the current indent string
/// (default empty — call [`xmlTextWriterSetIndentString`] to choose
/// the indent characters).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterSetIndent(
    writer: *mut xmlTextWriter,
    indent: c_int,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let mut s = w.state.borrow_mut();
    s.indent = indent != 0;
    if s.indent && s.indent_str.is_empty() {
        // Pick a sensible default the moment indenting is enabled
        // (libxml2 hard-codes two spaces).
        s.indent_str = "  ".to_string();
    }
    0
}

/// `xmlTextWriterSetIndentString(writer, str)` — set the per-level
/// indent characters.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextWriterSetIndentString(
    writer: *mut xmlTextWriter,
    s:      *const c_char,
) -> c_int {
    let w = match writer_ref(writer) { Some(w) => w, None => return -1 };
    let s = match unsafe { cstr_opt(s) } { Some(s) => s, None => return -1 };
    w.state.borrow_mut().indent_str = s.to_string();
    0
}

// ── tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn fresh_writer() -> (*mut xmlTextWriter, *mut crate::outbuf::xmlBuffer) {
        let buf = unsafe { crate::outbuf::xmlBufferCreate() };
        let out = unsafe { crate::outbuf::xmlOutputBufferCreateBuffer(buf, ptr::null_mut()) };
        let w   = unsafe { xmlNewTextWriter(out) };
        (w, buf)
    }

    fn finish(w: *mut xmlTextWriter, buf: *mut crate::outbuf::xmlBuffer) -> String {
        unsafe { xmlTextWriterFlush(w); }
        let ptr = unsafe { crate::outbuf::xmlBufferContent(buf) };
        let len = unsafe { crate::outbuf::xmlBufferLength(buf) } as usize;
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let s = String::from_utf8_lossy(bytes).into_owned();
        unsafe { xmlFreeTextWriter(w); }
        unsafe { crate::outbuf::xmlBufferFree(buf); }
        s
    }

    fn c(s: &str) -> CString { CString::new(s).unwrap() }

    #[test]
    fn empty_element_self_closes() {
        let (w, buf) = fresh_writer();
        let n = c("r");
        unsafe { xmlTextWriterStartElement(w, n.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        let out = finish(w, buf);
        assert_eq!(out, "<r/>");
    }

    #[test]
    fn element_with_text() {
        let (w, buf) = fresh_writer();
        let n = c("r");
        let t = c("hi");
        unsafe { xmlTextWriterStartElement(w, n.as_ptr()); }
        unsafe { xmlTextWriterWriteString(w, t.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        assert_eq!(finish(w, buf), "<r>hi</r>");
    }

    #[test]
    fn element_with_attribute() {
        let (w, buf) = fresh_writer();
        let n = c("r");
        let an = c("id");
        let av = c("b1");
        unsafe { xmlTextWriterStartElement(w, n.as_ptr()); }
        unsafe { xmlTextWriterWriteAttribute(w, an.as_ptr(), av.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        assert_eq!(finish(w, buf), "<r id=\"b1\"/>");
    }

    #[test]
    fn nested_elements() {
        let (w, buf) = fresh_writer();
        let r = c("catalog"); let b = c("book"); let id = c("id"); let v = c("1");
        unsafe { xmlTextWriterStartElement(w, r.as_ptr()); }
        unsafe { xmlTextWriterStartElement(w, b.as_ptr()); }
        unsafe { xmlTextWriterWriteAttribute(w, id.as_ptr(), v.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        unsafe { xmlTextWriterEndElement(w); }
        assert_eq!(finish(w, buf), "<catalog><book id=\"1\"/></catalog>");
    }

    #[test]
    fn escapes_text_content() {
        let (w, buf) = fresh_writer();
        let n = c("r"); let t = c("a < b & c > d");
        unsafe { xmlTextWriterStartElement(w, n.as_ptr()); }
        unsafe { xmlTextWriterWriteString(w, t.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        assert_eq!(finish(w, buf), "<r>a &lt; b &amp; c &gt; d</r>");
    }

    #[test]
    fn end_document_closes_all() {
        let (w, buf) = fresh_writer();
        let r = c("r"); let a = c("a"); let b = c("b");
        unsafe { xmlTextWriterStartDocument(w, ptr::null(), ptr::null(), ptr::null()); }
        unsafe { xmlTextWriterStartElement(w, r.as_ptr()); }
        unsafe { xmlTextWriterStartElement(w, a.as_ptr()); }
        unsafe { xmlTextWriterStartElement(w, b.as_ptr()); }
        unsafe { xmlTextWriterEndDocument(w); }
        let out = finish(w, buf);
        assert!(out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"), "got: {out}");
        assert!(out.contains("<r><a><b/></a></r>"), "got: {out}");
    }

    #[test]
    fn cdata_comment_pi() {
        let (w, buf) = fresh_writer();
        let r = c("r"); let cdata = c("a < b"); let cm = c("note"); let tgt = c("php"); let pi = c("echo 1");
        unsafe { xmlTextWriterStartElement(w, r.as_ptr()); }
        unsafe { xmlTextWriterWriteCDATA(w, cdata.as_ptr()); }
        unsafe { xmlTextWriterWriteComment(w, cm.as_ptr()); }
        unsafe { xmlTextWriterWritePI(w, tgt.as_ptr(), pi.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        let out = finish(w, buf);
        assert!(out.contains("<![CDATA[a < b]]>"), "got: {out}");
        assert!(out.contains("<!--note-->"), "got: {out}");
        assert!(out.contains("<?php echo 1?>"), "got: {out}");
    }

    #[test]
    fn piecewise_attribute() {
        let (w, buf) = fresh_writer();
        let r = c("r"); let an = c("greeting"); let p1 = c("hello"); let p2 = c(" world");
        unsafe { xmlTextWriterStartElement(w, r.as_ptr()); }
        unsafe { xmlTextWriterStartAttribute(w, an.as_ptr()); }
        unsafe { xmlTextWriterWriteString(w, p1.as_ptr()); }
        unsafe { xmlTextWriterWriteString(w, p2.as_ptr()); }
        unsafe { xmlTextWriterEndAttribute(w); }
        unsafe { xmlTextWriterEndElement(w); }
        assert_eq!(finish(w, buf), "<r greeting=\"hello world\"/>");
    }

    #[test]
    fn full_end_emits_separate_close() {
        let (w, buf) = fresh_writer();
        let n = c("r");
        unsafe { xmlTextWriterStartElement(w, n.as_ptr()); }
        unsafe { xmlTextWriterFullEndElement(w); }
        assert_eq!(finish(w, buf), "<r></r>");
    }

    #[test]
    fn write_element_one_shot() {
        let (w, buf) = fresh_writer();
        let n = c("greeting"); let t = c("hello");
        unsafe { xmlTextWriterWriteElement(w, n.as_ptr(), t.as_ptr()); }
        assert_eq!(finish(w, buf), "<greeting>hello</greeting>");
    }

    #[test]
    fn namespace_element_and_attribute() {
        let (w, buf) = fresh_writer();
        let pf = c("x"); let n = c("r"); let ns = c("urn:ex");
        let apf = c("x"); let an = c("attr"); let av = c("v");
        unsafe { xmlTextWriterStartElementNS(w, pf.as_ptr(), n.as_ptr(), ns.as_ptr()); }
        unsafe { xmlTextWriterWriteAttributeNS(w, apf.as_ptr(), an.as_ptr(), ptr::null(), av.as_ptr()); }
        unsafe { xmlTextWriterEndElement(w); }
        let s = finish(w, buf);
        assert!(s.contains("xmlns:x=\"urn:ex\""), "got: {s}");
        assert!(s.contains("x:attr=\"v\""), "got: {s}");
    }
}
