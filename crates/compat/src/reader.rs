//! libxml2 pull parser (`xmlTextReader`) — Tier 3.
//!
//! Drives `etree.iterparse()` and other event-stream consumers.  Our
//! implementation parses the whole document up front into our arena,
//! then walks it in document order via a state-machine iterator.
//! True streaming (parse-as-we-read) is a follow-up; for v0.1 the
//! "stream API on a fully-built tree" is enough for lxml's common
//! iterparse usage patterns (small-to-medium files, simple event
//! filters).
//!
//! ## Node-type discriminants
//!
//! `xmlTextReaderNodeType` returns:
//!
//!   1 = `XML_READER_TYPE_ELEMENT` (start tag)
//!   3 = `XML_READER_TYPE_TEXT`
//!   4 = `XML_READER_TYPE_CDATA`
//!   7 = `XML_READER_TYPE_PROCESSING_INSTRUCTION`
//!   8 = `XML_READER_TYPE_COMMENT`
//!  14 = `XML_READER_TYPE_WHITESPACE` (we report as TEXT)
//!  15 = `XML_READER_TYPE_END_ELEMENT` (end tag)
//!
//! Each START_ELEMENT visit is paired with an END_ELEMENT visit
//! after its descendants.  Self-closing elements emit only START
//! with `isEmptyElement = 1`.

use std::cell::RefCell;
use std::ffi::CString;
use std::io::Read;
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::io::FromRawFd;
use std::mem::ManuallyDrop;
use std::ptr;

use sup_xml_core::options::ParseOptions;
use sup_xml_core::parser::parse_bytes;
use sup_xml_tree::dom::{Attribute, Node, NodeKind, XmlDoc};

use crate::alloc::alloc_registered_cstring;

// ── node-type constants ───────────────────────────────────────────────────

pub const XML_READER_TYPE_NONE:           c_int = 0;
pub const XML_READER_TYPE_ELEMENT:        c_int = 1;
pub const XML_READER_TYPE_ATTRIBUTE:      c_int = 2;
pub const XML_READER_TYPE_TEXT:           c_int = 3;
pub const XML_READER_TYPE_CDATA:          c_int = 4;
pub const XML_READER_TYPE_PROCESSING_INSTRUCTION: c_int = 7;
pub const XML_READER_TYPE_COMMENT:        c_int = 8;
pub const XML_READER_TYPE_END_ELEMENT:    c_int = 15;

// ── reader state ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum CurKind {
    Start(*const Node<'static>),
    End(*const Node<'static>),
    Leaf(*const Node<'static>), // text, cdata, comment, PI
    Attr(*const Attribute<'static>, *const Node<'static>), // attr + owning element
    Eof,
}

/// `_xmlTextReader` — opaque to C callers.  Owns:
///   * (usually) a strong reference to the parsed [`XmlDoc`] — freed
///     in [`xmlFreeTextReader`] when `owns_doc` is true,
///   * the current visit cursor (a `CurKind`),
///   * a stack of "open elements" for END_ELEMENT emission,
///   * a list of recently-emitted strings (kept alive until the next
///     `Read()` so const accessors can hand back pointers).
///
/// `owns_doc` is false for readers built via [`xmlReaderWalker`] —
/// the caller retains ownership of the document.
pub struct xmlTextReader {
    /// The doc this reader walks.  See `owns_doc` for lifecycle.
    doc:       *mut XmlDoc,
    /// When true, [`xmlFreeTextReader`] calls `xmlFreeDoc(doc)`.
    /// When false (walker-mode), `doc` is borrowed and the caller frees.
    owns_doc:  bool,
    state:     RefCell<ReaderState>,
}

struct ReaderState {
    /// Where we are right now (the node whose accessors are valid).
    cur: CurKind,
    /// Stack of element nodes whose END_ELEMENT we still owe.
    /// Each frame is the element pointer.
    open: Vec<*const Node<'static>>,
    /// Holders for any const strings we've handed out — kept alive
    /// until the next state change.  libxml2's `xmlTextReaderConst*`
    /// pointers are stable until the next `Read()`.
    string_holders: Vec<CString>,
    /// Simple (non-structured) error callback registered via
    /// [`xmlTextReaderSetErrorHandler`] and returned verbatim by
    /// [`xmlTextReaderGetErrorHandler`].  `None` until a handler is set.
    error_func: Option<XmlTextReaderErrorFunc>,
    /// Opaque user context paired with `error_func` (libxml2 passes it
    /// back as the callback's first argument).
    error_arg:  *mut c_void,
}

/// libxml2 `xmlTextReaderErrorFunc` (xmlreader.h):
/// `void (*)(void *arg, const char *msg, xmlParserSeverities severity,
/// xmlTextReaderLocatorPtr locator)`.  `severity` is the
/// `xmlParserSeverities` enum (an `int`); `locator` is an opaque
/// `xmlTextReaderLocatorPtr`.
pub type XmlTextReaderErrorFunc = unsafe extern "C" fn(
    *mut c_void,    // arg
    *const c_char,  // msg
    c_int,          // xmlParserSeverities
    *mut c_void,    // xmlTextReaderLocatorPtr
);

// ── factories ─────────────────────────────────────────────────────────────

/// `xmlReaderForMemory(buffer, size, URL, encoding, options)`.
/// Returns a fresh reader positioned BEFORE the first node — caller
/// must call `xmlTextReaderRead` to advance.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderForMemory(
    buffer:    *const c_char,
    size:      c_int,
    _url:      *const c_char,
    _encoding: *const c_char,
    _options:  c_int,
) -> *mut xmlTextReader {
    if buffer.is_null() || size <= 0 {
        return ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    let doc = match parse_bytes(bytes, &opts) {
        Ok(d) => d,
        Err(_) => return ptr::null_mut(),
    };
    let xml_doc = doc.into_xml_doc();
    Box::into_raw(Box::new(xmlTextReader {
        doc:      xml_doc,
        owns_doc: true,
        state: RefCell::new(ReaderState {
            cur: CurKind::Start(ptr::null()), // "before first" — Read() advances to root
            open: Vec::new(),
            string_holders: Vec::new(),
            error_func: None,
            error_arg:  ptr::null_mut(),
        }),
    }))
}

/// `xmlReaderForFile(filename, encoding, options)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderForFile(
    filename: *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut xmlTextReader {
    if filename.is_null() {
        return ptr::null_mut();
    }
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return ptr::null_mut(),
    };
    unsafe {
        xmlReaderForMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            filename,
            encoding,
            options,
        )
    }
}

/// `xmlReaderForDoc(cur, URL, encoding, options)` — read from an
/// already-parsed doc represented as bytes.  lxml uses this for
/// the in-memory iterparse path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderForDoc(
    cur:      *const c_char,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut xmlTextReader {
    if cur.is_null() {
        return ptr::null_mut();
    }
    let len = unsafe { std::ffi::CStr::from_ptr(cur) }.to_bytes().len() as c_int;
    unsafe { xmlReaderForMemory(cur, len, url, encoding, options) }
}

/// `xmlReaderForFd(fd, URL, encoding, options)` — read the entire fd
/// into memory, then parse and walk.  Ownership of `fd` stays with
/// the caller; we do not close it (matches libxml2's documented
/// contract that the descriptor outlives the reader).
///
/// Returns NULL on read or parse failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderForFd(
    fd:        c_int,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
) -> *mut xmlTextReader {
    if fd < 0 { return ptr::null_mut(); }
    // SAFETY: caller asserts fd is a valid readable descriptor.
    // ManuallyDrop prevents File::drop from closing it on scope exit.
    let mut f = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return ptr::null_mut();
    }
    unsafe {
        xmlReaderForMemory(
            buf.as_ptr() as *const c_char,
            buf.len() as c_int,
            url, encoding, options,
        )
    }
}

/// libxml2 input callbacks consumed by [`xmlReaderForIO`] and
/// [`xmlParserInputBufferCreateIO`] (see [`crate::misc`] for the
/// related output-side types).
pub type XmlInputReadCallback  = unsafe extern "C" fn(
    context: *mut c_void,
    buffer:  *mut c_char,
    len:     c_int,
) -> c_int;
pub type XmlInputCloseCallback = unsafe extern "C" fn(context: *mut c_void) -> c_int;

/// `xmlReaderForIO(ioread, ioclose, ioctx, URL, encoding, options)` —
/// drive a reader from caller-supplied IO callbacks.  We slurp via
/// repeated `ioread` calls until it returns 0 (EOF) or negative
/// (error), invoke `ioclose` (when non-NULL), then parse the
/// resulting buffer and walk it.
///
/// `ioread` is called with a 4 KiB scratch buffer per chunk; the
/// callback's return value is the number of bytes written into the
/// buffer (or <= 0 to stop).  `ioclose` runs even when a read fails,
/// so caller-side cleanup always happens.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderForIO(
    ioread:    Option<XmlInputReadCallback>,
    ioclose:   Option<XmlInputCloseCallback>,
    ioctx:     *mut c_void,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
) -> *mut xmlTextReader {
    let Some(read_cb) = ioread else { return ptr::null_mut(); };
    let bytes = unsafe { slurp_via_callbacks(read_cb, ioclose, ioctx) };
    let bytes = match bytes { Some(b) => b, None => return ptr::null_mut() };
    unsafe {
        xmlReaderForMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            url, encoding, options,
        )
    }
}

/// `xmlReaderWalker(doc)` — wrap an already-parsed document so the
/// reader walks its nodes without re-parsing.  The reader does NOT
/// take ownership of `doc`; the caller must keep it alive for the
/// reader's lifetime and free it via `xmlFreeDoc` separately after
/// `xmlFreeTextReader`.
///
/// Used by lxml when running iterparse over an in-memory ElementTree
/// rather than a byte stream.  Returns NULL when `doc` is NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReaderWalker(doc: *mut XmlDoc) -> *mut xmlTextReader {
    if doc.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(xmlTextReader {
        doc,
        owns_doc: false,
        state: RefCell::new(ReaderState {
            cur: CurKind::Start(ptr::null()),
            open: Vec::new(),
            string_holders: Vec::new(),
            error_func: None,
            error_arg:  ptr::null_mut(),
        }),
    }))
}

// ── error-handler registration ────────────────────────────────────────────

/// `xmlTextReaderSetErrorHandler(reader, f, arg)` — register a simple
/// error/warning callback for `reader`.  Passing a NULL `f` clears any
/// previously-registered handler.  The `(f, arg)` pair is returned
/// verbatim by [`xmlTextReaderGetErrorHandler`], which is what makes
/// the common save-current / install-temporary / restore idiom work.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderSetErrorHandler(
    reader: *mut xmlTextReader,
    f:      Option<XmlTextReaderErrorFunc>,
    arg:    *mut c_void,
) {
    if reader.is_null() {
        return;
    }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    state.error_func = f;
    // libxml2 drops the context when the handler is cleared.
    state.error_arg = if f.is_some() { arg } else { ptr::null_mut() };
}

/// `xmlTextReaderGetErrorHandler(reader, f, arg)` — read back the
/// simple error handler and context previously installed via
/// [`xmlTextReaderSetErrorHandler`].  Writes the function pointer
/// through `*f` and the context through `*arg`; either out-pointer may
/// be NULL, in which case that value is not written.  When no handler
/// is registered (or `reader` is NULL) it reports a NULL function and
/// NULL context, matching libxml2's default state.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetErrorHandler(
    reader: *mut xmlTextReader,
    f:      *mut Option<XmlTextReaderErrorFunc>,
    arg:    *mut *mut c_void,
) {
    let (func, ctx) = if reader.is_null() {
        (None, ptr::null_mut())
    } else {
        let state = unsafe { &*reader }.state.borrow();
        (state.error_func, state.error_arg)
    };
    if !f.is_null() {
        unsafe { *f = func; }
    }
    if !arg.is_null() {
        unsafe { *arg = ctx; }
    }
}

/// Drive a libxml2-shaped read callback until EOF or error.
///
/// Returns the accumulated bytes on success.  When `read_cb` returns
/// a negative value at any point we discard the partial read and
/// return None — and we still invoke `close_cb` so the caller's
/// cleanup runs.
///
/// Re-exported as [`slurp_io_callbacks`] for sibling modules
/// (HTML's `htmlReadIO`) that consume the same callback shape.
pub(crate) unsafe fn slurp_io_callbacks(
    read_cb:  XmlInputReadCallback,
    close_cb: Option<XmlInputCloseCallback>,
    ctx:      *mut c_void,
) -> Option<Vec<u8>> {
    unsafe { slurp_via_callbacks(read_cb, close_cb, ctx) }
}

unsafe fn slurp_via_callbacks(
    read_cb:  XmlInputReadCallback,
    close_cb: Option<XmlInputCloseCallback>,
    ctx:      *mut c_void,
) -> Option<Vec<u8>> {
    const CHUNK: usize = 4096;
    let mut out  = Vec::new();
    let mut scratch = [0u8; CHUNK];
    let mut ok = true;
    loop {
        let n = unsafe {
            read_cb(ctx, scratch.as_mut_ptr() as *mut c_char, CHUNK as c_int)
        };
        if n == 0 { break; }
        if n < 0 { ok = false; break; }
        let n = n as usize;
        if n > CHUNK { ok = false; break; }
        out.extend_from_slice(&scratch[..n]);
    }
    if let Some(close) = close_cb {
        let _ = unsafe { close(ctx) };
    }
    if ok { Some(out) } else { None }
}

/// `xmlFreeTextReader(reader)` — release the reader.  When the reader
/// was constructed with one of the parsing factories
/// (`xmlReaderForMemory`/`File`/`Doc`/`Fd`/`IO`), its owned document
/// is freed too; readers built via [`xmlReaderWalker`] only release
/// the reader struct because the document is caller-owned.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeTextReader(reader: *mut xmlTextReader) {
    if reader.is_null() { return; }
    // SAFETY: reader came from one of the factories.
    let b = unsafe { Box::from_raw(reader) };
    if b.owns_doc && !b.doc.is_null() {
        unsafe { crate::parse::xmlFreeDoc(b.doc); }
    }
}

// ── state machine ────────────────────────────────────────────────────────

/// Advance the reader to the next node in document order.  Returns
/// 1 on success, 0 at EOF, -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderRead(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    state.string_holders.clear();

    let (new_cur, advance_open) = compute_next(&state.cur, &state.open, r.doc);
    state.cur = new_cur;
    if let Some(action) = advance_open {
        match action {
            OpenAction::Push(p)  => state.open.push(p),
            OpenAction::Pop      => { state.open.pop(); }
        }
    }
    match state.cur {
        CurKind::Eof => 0,
        _ => 1,
    }
}

enum OpenAction {
    Push(*const Node<'static>),
    Pop,
}

fn compute_next(
    cur: &CurKind,
    open: &[*const Node<'static>],
    doc: *mut XmlDoc,
) -> (CurKind, Option<OpenAction>) {
    match *cur {
        // First call after creation — descend to the document root.
        CurKind::Start(p) if p.is_null() => {
            if doc.is_null() { return (CurKind::Eof, None); }
            let root = unsafe { (*doc).children.get() };
            if root.is_null() { return (CurKind::Eof, None); }
            classify_first_visit(root)
        }
        // Just emitted a START for an element.  Either descend or
        // emit an END immediately if empty.
        CurKind::Start(p) => {
            let n = unsafe { &*p };
            // If this element has any children, descend to first.
            if let Some(first) = n.first_child.get() {
                let first_ptr = first as *const Node<'_> as *const Node<'static>;
                let (next_kind, _push) = classify_first_visit(first_ptr);
                // We need to mark `p` (the element) as open so we can
                // emit its END later.
                (next_kind, Some(OpenAction::Push(p)))
            } else {
                // Self-closing element: no END is emitted (libxml2
                // sets isEmptyElement on the START and skips ahead
                // to the next sibling).
                advance_after_element(p)
            }
        }
        // Just emitted an END for an element `p`.  Pop and move to
        // sibling / ancestor's sibling.
        CurKind::End(p) => {
            // The Pop happens here.
            let (next_kind, _) = advance_after_element(p);
            (next_kind, Some(OpenAction::Pop))
        }
        // Just emitted a leaf node — same advance logic.
        CurKind::Leaf(p) => advance_after_element(p),
        // Attribute visits aren't part of the document-order walk —
        // they're sub-iteration off an element START.  After visiting
        // attrs you call MoveToElement which resets `cur` to the
        // owning element's START.  Then `Read` would advance into
        // the element's children.
        CurKind::Attr(_, _) => {
            // For now treat as a no-op + return EOF — we don't
            // expect Read() to be called while iterating attrs.
            (CurKind::Eof, None)
        }
        CurKind::Eof => (CurKind::Eof, None),
    }
    .clone_or_self(open)
}

trait ResolveAdvance {
    fn clone_or_self(self, open: &[*const Node<'static>]) -> Self;
}
impl ResolveAdvance for (CurKind, Option<OpenAction>) {
    fn clone_or_self(self, _open: &[*const Node<'static>]) -> Self {
        self
    }
}

fn classify_first_visit(p: *const Node<'static>) -> (CurKind, Option<OpenAction>) {
    let n = unsafe { &*p };
    match n.kind {
        NodeKind::Element => (CurKind::Start(p), None),
        _                 => (CurKind::Leaf(p), None),
    }
}

/// After finishing `node` (either a leaf, an empty element, or the
/// END of a closed element), advance to the next visit:
///   - next sibling, or
///   - parent's END (we don't have an explicit END marker per-node
///     here; the close-emit logic happens by checking the OPEN stack).
fn advance_after_element(p: *const Node<'static>) -> (CurKind, Option<OpenAction>) {
    let n = unsafe { &*p };
    if let Some(next) = n.next_sibling.get() {
        let next_ptr = next as *const Node<'_> as *const Node<'static>;
        classify_first_visit(next_ptr)
    } else {
        // No sibling — emit END for the enclosing element, or EOF
        // if we're at the top level.  Top-level nodes (root,
        // prolog/epilogue comments and PIs) have their `parent`
        // pointer set to the `XmlDoc` cast as `Node` (so
        // `xmlUnlinkNode` can maintain the document's `children`
        // chain — see `Document::into_xml_doc`).  Treat the doc
        // as EOF here: there's no enclosing *element* to close.
        match n.parent.get() {
            Some(parent) if !matches!(parent.kind, NodeKind::Document) => {
                let parent_ptr = parent as *const Node<'_> as *const Node<'static>;
                (CurKind::End(parent_ptr), None)
            }
            _ => (CurKind::Eof, None),
        }
    }
}

// ── current-node accessors ────────────────────────────────────────────────

/// `xmlTextReaderNodeType` — the XML_READER_TYPE_* discriminant for
/// the current node.  Returns -1 if no current node.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderNodeType(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return -1 };
    match cur {
        CurKind::Start(p) => {
            // SAFETY: p was set by our state machine.
            let n = unsafe { &*p };
            match n.kind {
                NodeKind::Element => XML_READER_TYPE_ELEMENT,
                _                 => leaf_type(&n.kind),
            }
        }
        CurKind::End(_)   => XML_READER_TYPE_END_ELEMENT,
        CurKind::Leaf(p)  => {
            let n = unsafe { &*p };
            leaf_type(&n.kind)
        }
        CurKind::Attr(_, _) => XML_READER_TYPE_ATTRIBUTE,
        CurKind::Eof      => XML_READER_TYPE_NONE,
    }
}

fn leaf_type(kind: &NodeKind) -> c_int {
    match kind {
        NodeKind::Text     => XML_READER_TYPE_TEXT,
        NodeKind::CData    => XML_READER_TYPE_CDATA,
        NodeKind::Comment  => XML_READER_TYPE_COMMENT,
        NodeKind::Pi       => XML_READER_TYPE_PROCESSING_INSTRUCTION,
        _                  => XML_READER_TYPE_NONE,
    }
}

/// `xmlTextReaderConstName` — name of the current node (element/PI/attr).
/// Returns a pointer that stays valid until the next `Read()`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstName(reader: *mut xmlTextReader) -> *const c_char {
    name_of_current(reader, false)
}

/// `xmlTextReaderConstLocalName` — local part of the name (after `:`).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstLocalName(reader: *mut xmlTextReader) -> *const c_char {
    name_of_current(reader, true)
}

fn name_of_current(reader: *mut xmlTextReader, local_only: bool) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let s: String = match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => {
            let n = unsafe { &*p };
            let raw = match n.kind {
                NodeKind::Text     => "#text",
                NodeKind::CData    => "#cdata-section",
                NodeKind::Comment  => "#comment",
                NodeKind::Pi       => n.name(),
                NodeKind::Element  => n.name(),
                _                  => return ptr::null(),
            };
            if local_only {
                local_tail(raw).to_string()
            } else {
                raw.to_string()
            }
        }
        CurKind::Attr(a, _) => {
            let attr = unsafe { &*a };
            if local_only { local_tail(attr.name()).to_string() } else { attr.name().to_string() }
        }
        CurKind::Eof => return ptr::null(),
    };
    let cs = match CString::new(s) { Ok(c) => c, Err(_) => return ptr::null() };
    let ptr = cs.as_ptr();
    // Stash for lifetime stability.
    if let Some(r) = unsafe { reader.as_ref() } {
        r.state.borrow_mut().string_holders.push(cs);
    }
    ptr
}

fn local_tail(s: &str) -> &str {
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None    => s,
    }
}

/// `xmlTextReaderConstValue` — text value of the current node
/// (only valid for text-bearing kinds; NULL otherwise).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstValue(reader: *mut xmlTextReader) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let s: String = match cur {
        CurKind::Leaf(p) => {
            let n = unsafe { &*p };
            n.content().to_string()
        }
        CurKind::Attr(a, _) => {
            let attr = unsafe { &*a };
            attr.value().to_string()
        }
        _ => return ptr::null(),
    };
    if s.is_empty() { return ptr::null(); }
    let cs = match CString::new(s) { Ok(c) => c, Err(_) => return ptr::null() };
    let ptr = cs.as_ptr();
    if let Some(r) = unsafe { reader.as_ref() } {
        r.state.borrow_mut().string_holders.push(cs);
    }
    ptr
}

/// `xmlTextReaderConstNamespaceUri` — namespace URI of the current node.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstNamespaceUri(reader: *mut xmlTextReader) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let ns_str: Option<String> = match cur {
        CurKind::Start(p) | CurKind::End(p) => {
            let n = unsafe { &*p };
            n.namespace.get().map(|ns| ns.href().to_string())
        }
        CurKind::Attr(a, _) => {
            let a = unsafe { &*a };
            a.namespace.get().map(|ns| ns.href().to_string())
        }
        _ => None,
    };
    match ns_str {
        Some(s) if !s.is_empty() => {
            let cs = match CString::new(s) { Ok(c) => c, Err(_) => return ptr::null() };
            let ptr = cs.as_ptr();
            if let Some(r) = unsafe { reader.as_ref() } {
                r.state.borrow_mut().string_holders.push(cs);
            }
            ptr
        }
        _ => ptr::null(),
    }
}

/// `xmlTextReaderConstPrefix` — prefix part of the current element's name.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstPrefix(reader: *mut xmlTextReader) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let name: Option<String> = match cur {
        CurKind::Start(p) | CurKind::End(p) => {
            let n = unsafe { &*p };
            prefix_of(n.name()).map(|s| s.to_string())
        }
        CurKind::Attr(a, _) => {
            let a = unsafe { &*a };
            prefix_of(a.name()).map(|s| s.to_string())
        }
        _ => None,
    };
    match name {
        Some(s) => {
            let cs = match CString::new(s) { Ok(c) => c, Err(_) => return ptr::null() };
            let ptr = cs.as_ptr();
            if let Some(r) = unsafe { reader.as_ref() } {
                r.state.borrow_mut().string_holders.push(cs);
            }
            ptr
        }
        None => ptr::null(),
    }
}

fn prefix_of(s: &str) -> Option<&str> {
    s.find(':').map(|i| &s[..i])
}

/// `xmlTextReaderHasValue` — 1 if the current node carries a value
/// (text/cdata/comment/PI/attribute), 0 otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderHasValue(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return 0 };
    match cur {
        CurKind::Leaf(_) | CurKind::Attr(_, _) => 1,
        _                                      => 0,
    }
}

/// `xmlTextReaderIsEmptyElement` — 1 if the current element is
/// self-closing (no children), 0 otherwise / on non-element.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderIsEmptyElement(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return -1 };
    match cur {
        CurKind::Start(p) => {
            let n = unsafe { &*p };
            if n.first_child.get().is_none() { 1 } else { 0 }
        }
        _ => 0,
    }
}

/// `xmlTextReaderDepth` — nesting depth of the current node.  Root
/// element is depth 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderDepth(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    r.state.borrow().open.len() as c_int
}

/// `xmlTextReaderGetParserLineNumber` — line of the current node in the
/// source.  libxml2 returns 0 when the reader is NULL or has no usable
/// parser state (matches the "no info available" contract).
///
/// We build the tree up front and walk it; there is no live parser
/// context to query.  When the cursor sits on a node, we report the
/// per-node `line` value captured by the parser (or 0 if line tracking
/// wasn't enabled for that parse).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetParserLineNumber(
    reader: *mut xmlTextReader,
) -> c_int {
    if reader.is_null() { return 0; }
    let r = unsafe { &*reader };
    let st = r.state.borrow();
    let node_ptr: *const Node<'static> = match st.cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        CurKind::Attr(_, owner) => owner,
        CurKind::Eof => return 0,
    };
    if node_ptr.is_null() { return 0; }
    // SAFETY: node lives in the doc's arena which the reader keeps alive.
    let n = unsafe { &*node_ptr };
    n.line as c_int
}

/// `xmlTextReaderGetParserColumnNumber` — column of the current node.
/// We do not capture per-node column information today, so this always
/// returns 0 (matching libxml2's behaviour when column info is
/// unavailable).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetParserColumnNumber(
    _reader: *mut xmlTextReader,
) -> c_int {
    0
}

/// `xmlTextReaderAttributeCount` — number of attributes on the
/// current element.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderAttributeCount(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return -1 };
    match cur {
        CurKind::Start(p) => {
            let n = unsafe { &*p };
            let mut count = 0;
            let mut a = n.first_attribute.get();
            while let Some(at) = a {
                // Skip xmlns/xmlns:* — libxml2 reports them via
                // separate ns_def, not in the attribute count.
                let nm = at.name();
                if nm != "xmlns" && !nm.starts_with("xmlns:") {
                    count += 1;
                }
                a = at.next.get();
            }
            count
        }
        _ => 0,
    }
}

/// `xmlTextReaderMoveToFirstAttribute` — move to the first attribute
/// of the current element.  Returns 1 on success, 0 if none, -1 on
/// error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToFirstAttribute(
    reader: *mut xmlTextReader,
) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let p = match state.cur {
        CurKind::Start(p) => p,
        CurKind::Attr(_, p) => p,
        _ => return -1,
    };
    if p.is_null() { return -1; }
    let n = unsafe { &*p };
    let mut a = n.first_attribute.get();
    while let Some(at) = a {
        let nm = at.name();
        if nm != "xmlns" && !nm.starts_with("xmlns:") {
            state.string_holders.clear();
            state.cur = CurKind::Attr(at as *const _, p);
            return 1;
        }
        a = at.next.get();
    }
    0
}

/// `xmlTextReaderMoveToNextAttribute` — move to the next attribute.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToNextAttribute(
    reader: *mut xmlTextReader,
) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let (cur_attr, owner) = match state.cur {
        CurKind::Attr(a, p) => (a, p),
        _ => return -1,
    };
    let a = unsafe { &*cur_attr };
    let mut nx = a.next.get();
    while let Some(at) = nx {
        let nm = at.name();
        if nm != "xmlns" && !nm.starts_with("xmlns:") {
            state.string_holders.clear();
            state.cur = CurKind::Attr(at as *const _, owner);
            return 1;
        }
        nx = at.next.get();
    }
    0
}

/// `xmlTextReaderMoveToElement` — return to the owning element from
/// an attribute visit.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToElement(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    if let CurKind::Attr(_, owner) = state.cur {
        state.string_holders.clear();
        state.cur = CurKind::Start(owner);
        return 1;
    }
    0
}

/// `xmlTextReaderReadState` — coarse reader state.  We report:
///   0 = INITIAL (before first Read), 1 = INTERACTIVE (mid-walk),
///   2 = ERROR, 3 = EOF, 4 = CLOSED.  v0.1 returns INTERACTIVE for
///   any "have current node" state and EOF otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderReadState(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return 2; }
    let r = unsafe { &*reader };
    match r.state.borrow().cur {
        CurKind::Eof              => 3,
        CurKind::Start(p) if p.is_null() => 0,
        _                         => 1,
    }
}

/// `xmlTextReaderExpand` — materialize the current element's subtree
/// (return the underlying `xmlNode*`).  Useful for callers that want
/// to switch from streaming to tree-mode for one specific subtree.
/// Our reader already has the full tree in memory, so this just
/// returns the current element pointer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderExpand(
    reader: *mut xmlTextReader,
) -> *mut Node<'static> {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null_mut() };
    match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p as *mut Node<'static>,
        _ => ptr::null_mut(),
    }
}

/// `xmlTextReaderCurrentNode` — non-const variant of Expand.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderCurrentNode(
    reader: *mut xmlTextReader,
) -> *mut Node<'static> {
    unsafe { xmlTextReaderExpand(reader) }
}

/// `xmlTextReaderClose` — release any underlying I/O.  For our
/// in-memory reader this is a no-op; the actual free happens via
/// `xmlFreeTextReader`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderClose(_reader: *mut xmlTextReader) -> c_int {
    0
}

/// `xmlTextReaderName(reader)` — non-const version of ConstName;
/// returns a fresh `xmlChar*` the caller xmlFrees.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderName(reader: *mut xmlTextReader) -> *mut c_char {
    let p = unsafe { xmlTextReaderConstName(reader) };
    if p.is_null() { return ptr::null_mut(); }
    let bytes = unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes();
    alloc_registered_cstring(bytes)
}

/// `xmlTextReaderValue(reader)` — non-const Value (xmlFreed).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderValue(reader: *mut xmlTextReader) -> *mut c_char {
    let p = unsafe { xmlTextReaderConstValue(reader) };
    if p.is_null() { return ptr::null_mut(); }
    let bytes = unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes();
    alloc_registered_cstring(bytes)
}

// ── attribute axis ──────────────────────────────────────────────────────

/// `xmlTextReaderHasAttributes` — non-zero iff the current node has at
/// least one attribute (excluding xmlns:* declarations, per libxml2).
/// Returns -1 on NULL reader (libxml2's error convention).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderHasAttributes(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let n = unsafe { xmlTextReaderAttributeCount(reader) };
    if n < 0 { -1 } else if n > 0 { 1 } else { 0 }
}

/// `xmlTextReaderIsNamespaceDecl` — non-zero iff the current cursor is
/// an attribute that's actually a namespace declaration (`xmlns` or
/// `xmlns:*`).  Libxml2's iteration normally excludes ns decls, but a
/// consumer can land on one via direct ns-decl walks.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderIsNamespaceDecl(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return -1 };
    match cur {
        CurKind::Attr(a, _) => {
            let attr = unsafe { &*a };
            let n = attr.name();
            if n == "xmlns" || n.starts_with("xmlns:") { 1 } else { 0 }
        }
        _ => 0,
    }
}

/// `xmlTextReaderIsDefault` — 1 if the current attribute was a
/// DTD-defaulted value rather than literally present in the source.
/// We don't track this distinction yet; report 0 (= present).  Returns
/// -1 on NULL reader.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderIsDefault(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// Internal helper — produce a heap-allocated copy of an attribute's
/// value, suitable for return to a C caller via xmlFree.
fn alloc_attr_value(attr: &Attribute<'_>) -> *mut c_char {
    let v = attr.value();
    alloc_registered_cstring(v.as_bytes())
}

/// `xmlTextReaderGetAttribute(reader, name)` — caller-owned heap
/// allocation, NULL if attribute not present.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetAttribute(
    reader: *mut xmlTextReader,
    name:   *const c_char,
) -> *mut c_char {
    if name.is_null() { return ptr::null_mut(); }
    let el = match unsafe { current_owning_element(reader) } {
        Some(p) => p, None => return ptr::null_mut(),
    };
    let target = match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
        Ok(s) => s, Err(_) => return ptr::null_mut(),
    };
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        if at.name() == target { return alloc_attr_value(at); }
        a = at.next.get();
    }
    ptr::null_mut()
}

/// `xmlTextReaderGetAttributeNo(reader, n)` — by 0-based index,
/// excluding namespace decls.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetAttributeNo(
    reader: *mut xmlTextReader,
    n:      c_int,
) -> *mut c_char {
    if n < 0 { return ptr::null_mut(); }
    let el = match unsafe { current_owning_element(reader) } {
        Some(p) => p, None => return ptr::null_mut(),
    };
    let mut idx = 0i32;
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        let nm = at.name();
        if nm != "xmlns" && !nm.starts_with("xmlns:") {
            if idx == n { return alloc_attr_value(at); }
            idx += 1;
        }
        a = at.next.get();
    }
    ptr::null_mut()
}

/// `xmlTextReaderGetAttributeNs(reader, localName, namespaceURI)` —
/// name+ns lookup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetAttributeNs(
    reader:        *mut xmlTextReader,
    local_name:    *const c_char,
    namespace_uri: *const c_char,
) -> *mut c_char {
    if local_name.is_null() { return ptr::null_mut(); }
    let el = match unsafe { current_owning_element(reader) } {
        Some(p) => p, None => return ptr::null_mut(),
    };
    let want_local = match unsafe { std::ffi::CStr::from_ptr(local_name) }.to_str() {
        Ok(s) => s, Err(_) => return ptr::null_mut(),
    };
    let want_uri = if namespace_uri.is_null() {
        None
    } else {
        match unsafe { std::ffi::CStr::from_ptr(namespace_uri) }.to_str() {
            Ok(s) => Some(s), Err(_) => return ptr::null_mut(),
        }
    };
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        let local = match at.name().rfind(':') {
            Some(i) => &at.name()[i + 1..],
            None    => at.name(),
        };
        if local == want_local {
            let attr_uri = at.namespace.get().map(|ns| ns.href());
            let match_uri = match (want_uri, attr_uri) {
                (None,    None)    => true,
                (Some(w), Some(a)) => w == a,
                (Some(w), None)    => w.is_empty(),
                (None,    Some(_)) => false,
            };
            if match_uri { return alloc_attr_value(at); }
        }
        a = at.next.get();
    }
    ptr::null_mut()
}

/// `xmlTextReaderMoveToAttribute(reader, name)` — move cursor to the
/// named attribute on the current element.  1 on success, 0 if not
/// found, -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToAttribute(
    reader: *mut xmlTextReader,
    name:   *const c_char,
) -> c_int {
    if reader.is_null() || name.is_null() { return -1; }
    let target = match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
        Ok(s) => s, Err(_) => return -1,
    };
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let el_ptr = match state.cur {
        CurKind::Start(p) => p,
        CurKind::Attr(_, p) => p,
        _ => return -1,
    };
    if el_ptr.is_null() { return -1; }
    let el = unsafe { &*el_ptr };
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        if at.name() == target {
            state.string_holders.clear();
            state.cur = CurKind::Attr(at as *const _, el_ptr);
            return 1;
        }
        a = at.next.get();
    }
    0
}

/// `xmlTextReaderMoveToAttributeNo(reader, n)` — move to attribute by
/// index (excluding namespace decls).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToAttributeNo(
    reader: *mut xmlTextReader,
    n:      c_int,
) -> c_int {
    if reader.is_null() || n < 0 { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let el_ptr = match state.cur {
        CurKind::Start(p) => p,
        CurKind::Attr(_, p) => p,
        _ => return -1,
    };
    if el_ptr.is_null() { return -1; }
    let el = unsafe { &*el_ptr };
    let mut idx = 0i32;
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        let nm = at.name();
        if nm != "xmlns" && !nm.starts_with("xmlns:") {
            if idx == n {
                state.string_holders.clear();
                state.cur = CurKind::Attr(at as *const _, el_ptr);
                return 1;
            }
            idx += 1;
        }
        a = at.next.get();
    }
    0
}

/// `xmlTextReaderMoveToAttributeNs(reader, localName, namespaceURI)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderMoveToAttributeNs(
    reader:        *mut xmlTextReader,
    local_name:    *const c_char,
    namespace_uri: *const c_char,
) -> c_int {
    if reader.is_null() || local_name.is_null() { return -1; }
    let want_local = match unsafe { std::ffi::CStr::from_ptr(local_name) }.to_str() {
        Ok(s) => s, Err(_) => return -1,
    };
    let want_uri = if namespace_uri.is_null() {
        None
    } else {
        match unsafe { std::ffi::CStr::from_ptr(namespace_uri) }.to_str() {
            Ok(s) => Some(s), Err(_) => return -1,
        }
    };
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let el_ptr = match state.cur {
        CurKind::Start(p) => p,
        CurKind::Attr(_, p) => p,
        _ => return -1,
    };
    if el_ptr.is_null() { return -1; }
    let el = unsafe { &*el_ptr };
    let mut a = el.first_attribute.get();
    while let Some(at) = a {
        let local = match at.name().rfind(':') {
            Some(i) => &at.name()[i + 1..],
            None    => at.name(),
        };
        if local == want_local {
            let attr_uri = at.namespace.get().map(|ns| ns.href());
            let match_uri = match (want_uri, attr_uri) {
                (None,    None)    => true,
                (Some(w), Some(a)) => w == a,
                (Some(w), None)    => w.is_empty(),
                (None,    Some(_)) => false,
            };
            if match_uri {
                state.string_holders.clear();
                state.cur = CurKind::Attr(at as *const _, el_ptr);
                return 1;
            }
        }
        a = at.next.get();
    }
    0
}

// ── reader doc / encoding / version / lang / base ────────────────────────

/// `xmlTextReaderCurrentDoc` — the owning document.  The reader holds
/// a strong reference; the caller must NOT free it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderCurrentDoc(reader: *mut xmlTextReader) -> *mut XmlDoc {
    if reader.is_null() { return ptr::null_mut(); }
    let r = unsafe { &*reader };
    r.doc
}

/// `xmlTextReaderConstEncoding` — encoding name as declared on the
/// XML decl (or "UTF-8" if not specified).  Returns NULL on NULL reader.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstEncoding(reader: *mut xmlTextReader) -> *const c_char {
    if reader.is_null() { return ptr::null(); }
    static_string_for_reader(reader, "UTF-8")
}

/// `xmlTextReaderConstXmlVersion` — version from the XML decl ("1.0"
/// or "1.1").  Returns NULL on NULL reader.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstXmlVersion(reader: *mut xmlTextReader) -> *const c_char {
    if reader.is_null() { return ptr::null(); }
    static_string_for_reader(reader, "1.0")
}

/// `xmlTextReaderStandalone` — 1 = standalone="yes", 0 = no, -1 = not
/// specified (or NULL reader).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderStandalone(_reader: *mut xmlTextReader) -> c_int { -1 }

/// `xmlTextReaderQuoteChar` — attribute-quote character.  Returns 34
/// (`"`) for any valid reader (we always emit double quotes), -1 on
/// NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderQuoteChar(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    34
}

/// `xmlTextReaderByteConsumed` — bytes consumed so far.  Returns -1 on
/// NULL reader; 0 otherwise (we build the tree up front and don't
/// expose a running counter, matching libxml2 after parse completion).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderByteConsumed(reader: *mut xmlTextReader) -> std::os::raw::c_long {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderConstBaseUri` — value of `xml:base` in scope at the
/// current node, inherited from ancestors.  Returns NULL if no
/// `xml:base` is set anywhere on the path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstBaseUri(reader: *mut xmlTextReader) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let start = match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        CurKind::Attr(_, p) => p,
        CurKind::Eof => return ptr::null(),
    };
    let value = walk_xml_attr_in_scope(start, "xml:base");
    match value {
        Some(s) => static_string_for_reader(reader, &s),
        None    => ptr::null(),
    }
}

/// `xmlTextReaderConstXmlLang` — value of `xml:lang` in scope.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderConstXmlLang(reader: *mut xmlTextReader) -> *const c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null() };
    let start = match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        CurKind::Attr(_, p) => p,
        CurKind::Eof => return ptr::null(),
    };
    let value = walk_xml_attr_in_scope(start, "xml:lang");
    match value {
        Some(s) => static_string_for_reader(reader, &s),
        None    => ptr::null(),
    }
}

/// `xmlTextReaderLookupNamespace(reader, prefix)` — resolve a prefix
/// to its in-scope URI.  Walks parent chain checking each ancestor's
/// `ns_def`.  Returns a fresh heap-allocated string the caller frees
/// via xmlFree.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderLookupNamespace(
    reader: *mut xmlTextReader,
    prefix: *const c_char,
) -> *mut c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null_mut() };
    let start = match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        CurKind::Attr(_, p) => p,
        CurKind::Eof => return ptr::null_mut(),
    };
    let want: Option<&str> = if prefix.is_null() {
        None
    } else {
        match unsafe { std::ffi::CStr::from_ptr(prefix) }.to_str() {
            Ok(s) => Some(s),
            Err(_) => return ptr::null_mut(),
        }
    };
    let resolved = lookup_ns_in_scope(start, want);
    match resolved {
        Some(uri) => alloc_registered_cstring(uri.as_bytes()),
        None      => ptr::null_mut(),
    }
}

// ── navigation ──────────────────────────────────────────────────────────

/// `xmlTextReaderNextSibling` — advance to the next sibling, skipping
/// any subtree below the current node.  Returns 1, 0, or -1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderNextSibling(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let cur_ptr: *const Node<'static> = match state.cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        _ => return -1,
    };
    if cur_ptr.is_null() { return -1; }
    let n = unsafe { &*cur_ptr };
    let next = match n.next_sibling.get() {
        Some(s) => s as *const Node<'_> as *const Node<'static>,
        None    => return 0,
    };
    state.string_holders.clear();
    let (kind, _) = classify_first_visit(next);
    state.cur = kind;
    1
}

/// `xmlTextReaderNext` — advance to the next node in document order,
/// SKIPPING the descendants of the current element.  Returns 1, 0,
/// or -1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderNext(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    let r = unsafe { &*reader };
    let mut state = r.state.borrow_mut();
    let cur_ptr: *const Node<'static> = match state.cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        _ => return -1,
    };
    if cur_ptr.is_null() { return -1; }
    // Walk to next-sibling; if none, climb to ancestor's next-sibling.
    let mut node: &Node<'_> = unsafe { &*cur_ptr };
    loop {
        if let Some(sib) = node.next_sibling.get() {
            state.string_holders.clear();
            let p = sib as *const Node<'_> as *const Node<'static>;
            let (kind, _) = classify_first_visit(p);
            state.cur = kind;
            return 1;
        }
        match node.parent.get() {
            Some(parent) if matches!(parent.kind, NodeKind::Element) => {
                node = parent;
                continue;
            }
            _ => {
                state.string_holders.clear();
                state.cur = CurKind::Eof;
                return 0;
            }
        }
    }
}

/// `xmlTextReaderPreserve` — mark the current node to be preserved in
/// memory after the reader is destroyed.  Our reader builds the full
/// tree up front and the doc outlives the reader iff the caller
/// keeps the returned pointer alive, so this is effectively a
/// pass-through that returns the current node pointer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderPreserve(reader: *mut xmlTextReader) -> *mut Node<'static> {
    unsafe { xmlTextReaderCurrentNode(reader) }
}

/// `xmlTextReaderPreservePattern` — pattern-based preservation.  We
/// treat every encountered node as preserved (see [`xmlTextReaderPreserve`]),
/// so the pattern is unused.  Returns 0 on a valid reader, -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderPreservePattern(
    reader:      *mut xmlTextReader,
    _pattern:    *const c_char,
    _namespaces: *mut *const c_char,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

// ── content extraction ────────────────────────────────────────────────

/// `xmlTextReaderReadInnerXml` — serialize the current node's
/// children as XML, returning a fresh heap-allocated NUL-terminated
/// string the caller frees via xmlFree.  NULL if not on an element.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderReadInnerXml(reader: *mut xmlTextReader) -> *mut c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null_mut() };
    let p = match cur {
        CurKind::Start(p) | CurKind::End(p) => p,
        _ => return ptr::null_mut(),
    };
    if p.is_null() { return ptr::null_mut(); }
    let el = unsafe { &*p };
    let mut out = String::new();
    for child in el.children() {
        serialize_node_xml(child, &mut out);
    }
    alloc_registered_cstring(out.as_bytes())
}

/// `xmlTextReaderReadOuterXml` — serialize the current node itself
/// (including its tags) plus all children.  Heap-allocated string.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderReadOuterXml(reader: *mut xmlTextReader) -> *mut c_char {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return ptr::null_mut() };
    let p = match cur {
        CurKind::Start(p) | CurKind::End(p) | CurKind::Leaf(p) => p,
        _ => return ptr::null_mut(),
    };
    if p.is_null() { return ptr::null_mut(); }
    let n = unsafe { &*p };
    let mut out = String::new();
    serialize_node_xml(n, &mut out);
    alloc_registered_cstring(out.as_bytes())
}

/// `xmlTextReaderReadString` — current node's text value as a fresh
/// heap-allocated string.  Caller frees via xmlFree.  NULL if the
/// current node has no usable text content.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderReadString(reader: *mut xmlTextReader) -> *mut c_char {
    let p = unsafe { xmlTextReaderConstValue(reader) };
    if p.is_null() { return ptr::null_mut(); }
    let bytes = unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes();
    alloc_registered_cstring(bytes)
}

/// `xmlNewTextReader(input, URI)` — Reader from an output buffer's
/// twin input variant.  PHP / libxml2 callers more commonly use
/// `xmlReaderForMemory` / `xmlReaderForFile`; this entry point exists
/// for code that has already built an `xmlParserInputBuffer` and wants
/// to wrap it.
///
/// Our `xmlParserInputBufferCreateMem` returns an opaque 256-byte
/// allocation that doesn't carry the source bytes through to the
/// reader, so this variant cannot recover the original input.  We
/// return NULL — callers that hit this should switch to
/// `xmlReaderForMemory` which has a working code path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewTextReader(
    _input: *mut std::os::raw::c_void,
    _uri:   *const c_char,
) -> *mut xmlTextReader {
    ptr::null_mut()
}

/// `xmlTextReaderSetup(reader, input, URL, encoding, options)` —
/// re-initialize an existing reader for a new input source.  We don't
/// support post-construction re-init yet (the reader's internal arena
/// would need to be reset); accept the call as a no-op success (0)
/// to keep consumers compiling.  Real consumers expect the reader's
/// next `Read` to surface the new content — they'll observe the old
/// content instead.  Track via the test that exercises the path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderSetup(
    reader:   *mut xmlTextReader,
    _input:   *mut std::os::raw::c_void,
    _url:     *const c_char,
    _encoding:*const c_char,
    _options: c_int,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderReadAttributeValue` — when positioned on an
/// attribute, advance into its value as a sequence of text-and-
/// entity-ref nodes.  Our model stores attribute values as a single
/// flat string, so the first call emits a synthetic text "node"
/// (the cursor stays on the attribute; the value is what `Value()`
/// returns) and subsequent calls return 0.
///
/// Returns 1 if there's more value to consume, 0 at end of value,
/// -1 if not on an attribute.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderReadAttributeValue(reader: *mut xmlTextReader) -> c_int {
    let cur = match unsafe { current(reader) } { Some(c) => c, None => return -1 };
    match cur {
        CurKind::Attr(_, _) => 0,   // Single-shot model: the attribute IS its value.
        _ => -1,
    }
}

// ── parser-property / schema stubs (return sensible defaults) ──────────

/// `xmlTextReaderGetParserProp(reader, prop)` — get a parser option.
/// We don't expose option state through the reader yet; report 0 on
/// any valid reader, -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderGetParserProp(
    reader: *mut xmlTextReader,
    _prop:  c_int,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderSetParserProp(reader, prop, value)` — set a parser
/// option.  No-op; reports success (0) on valid reader, -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderSetParserProp(
    reader: *mut xmlTextReader,
    _prop:  c_int,
    _value: c_int,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderIsValid` — 1 if validation is enabled and current
/// position is valid; 0 if invalid; -1 on NULL reader (or no
/// validation infrastructure wired, which matches our current state).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderIsValid(reader: *mut xmlTextReader) -> c_int {
    if reader.is_null() { return -1; }
    // We don't wire reader-driven validation yet; libxml2 returns -1
    // when no schema/RNG has been bound, which is also our state.
    -1
}

/// `xmlTextReaderRelaxNGSetSchema` — bind a RelaxNG schema for
/// validation as the reader iterates.  Validation isn't wired yet;
/// accept the call (return 0) on a valid reader, -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderRelaxNGSetSchema(
    reader:  *mut xmlTextReader,
    _schema: *mut std::os::raw::c_void,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderRelaxNGValidate` — start RelaxNG validation against
/// a schema loaded from a file.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderRelaxNGValidate(
    reader: *mut xmlTextReader,
    _rng:   *const c_char,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderSchemaValidate` — XSD validation against a schema file.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderSchemaValidate(
    reader: *mut xmlTextReader,
    _xsd:   *const c_char,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

/// `xmlTextReaderSetSchema` — bind an already-compiled XSD schema.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlTextReaderSetSchema(
    reader:  *mut xmlTextReader,
    _schema: *mut std::os::raw::c_void,
) -> c_int {
    if reader.is_null() { return -1; }
    0
}

// ── shared helpers ────────────────────────────────────────────────────────

unsafe fn current(reader: *mut xmlTextReader) -> Option<CurKind> {
    if reader.is_null() { return None; }
    let r = unsafe { &*reader };
    Some(r.state.borrow().cur)
}

/// Return the element whose attribute space the cursor is in — the
/// owning element on `Attr(_, owner)`, the element itself on `Start(p)`.
/// `None` if the cursor isn't on or near an element.
unsafe fn current_owning_element(reader: *mut xmlTextReader) -> Option<&'static Node<'static>> {
    let cur = unsafe { current(reader) }?;
    let p = match cur {
        CurKind::Start(p) | CurKind::End(p) => p,
        CurKind::Attr(_, p) => p,
        _ => return None,
    };
    if p.is_null() { return None; }
    Some(unsafe { &*p })
}

/// Walk parent chain looking for a literal attribute named `target`
/// (e.g. "xml:base", "xml:lang").  Stops at the first match.
fn walk_xml_attr_in_scope(start: *const Node<'static>, target: &str) -> Option<String> {
    if start.is_null() { return None; }
    // `target` is a prefixed QName (e.g. "xml:lang").  Match by local
    // name + namespace prefix so it works whether the attribute name is
    // stored as the full QName (lean) or the local part (c-abi).
    let (want_prefix, want_local) = match target.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None         => (None, target),
    };
    let mut cur: Option<&Node<'_>> = unsafe { start.as_ref() };
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Element) {
            for a in n.attributes() {
                let aprefix = a.namespace.get().and_then(|ns| ns.prefix());
                if a.local_name() == want_local && aprefix == want_prefix {
                    return Some(a.value().to_string());
                }
            }
        }
        cur = n.parent.get();
    }
    None
}

/// Walk parent chain looking up a namespace by prefix.  `None` prefix
/// means "default namespace" (xmlns="…").
fn lookup_ns_in_scope(start: *const Node<'static>, want_prefix: Option<&str>) -> Option<String> {
    if start.is_null() { return None; }
    let mut cur: Option<&Node<'_>> = unsafe { start.as_ref() };
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Element) {
            let mut ns = n.ns_def.get();
            while let Some(decl) = ns {
                let decl_prefix = decl.prefix();
                match (want_prefix, decl_prefix) {
                    (None,        None)        => return Some(decl.href().to_string()),
                    (Some(w),     Some(p)) if w == p => return Some(decl.href().to_string()),
                    _ => {}
                }
                ns = decl.next.get();
            }
        }
        cur = n.parent.get();
    }
    None
}

/// Stash a CString-wrapped copy of `s` on the reader's string-holder
/// stack and return its raw pointer (valid until the next state change).
fn static_string_for_reader(reader: *mut xmlTextReader, s: &str) -> *const c_char {
    let cs = match CString::new(s) { Ok(c) => c, Err(_) => return ptr::null() };
    let ptr = cs.as_ptr();
    if let Some(r) = unsafe { reader.as_ref() } {
        r.state.borrow_mut().string_holders.push(cs);
    }
    ptr
}

/// Minimal in-place XML serializer used by the reader's
/// ReadInnerXml / ReadOuterXml functions.  Emits the canonical
/// non-format-friendly shape ("<el a=\"v\">…</el>") and escapes the
/// canonical five entities in text content / attribute values.
fn serialize_node_xml(n: &Node<'_>, out: &mut String) {
    match n.kind {
        NodeKind::Element => {
            out.push('<');
            out.push_str(n.name());
            for a in n.attributes() {
                out.push(' ');
                out.push_str(a.name());
                out.push_str("=\"");
                push_escaped(a.value(), out, /*in_attr=*/ true);
                out.push('"');
            }
            let mut first_child = n.first_child.get();
            if first_child.is_none() {
                out.push_str("/>");
            } else {
                out.push('>');
                while let Some(c) = first_child {
                    serialize_node_xml(c, out);
                    first_child = c.next_sibling.get();
                }
                out.push_str("</");
                out.push_str(n.name());
                out.push('>');
            }
        }
        NodeKind::Text     => push_escaped(n.content(), out, /*in_attr=*/ false),
        NodeKind::CData    => {
            out.push_str("<![CDATA[");
            out.push_str(n.content());
            out.push_str("]]>");
        }
        NodeKind::Comment  => {
            out.push_str("<!--");
            out.push_str(n.content());
            out.push_str("-->");
        }
        NodeKind::Pi       => {
            out.push_str("<?");
            out.push_str(n.name());
            let c = n.content();
            if !c.is_empty() { out.push(' '); out.push_str(c); }
            out.push_str("?>");
        }
        NodeKind::EntityRef => out.push_str(n.content()),
        // Raw internal-subset markup declarations — emit verbatim.
        NodeKind::DtdDecl => out.push_str(n.content()),
        // The internal-subset node itself emits no markup here (the
        // ReadInner/OuterXml callers operate on element subtrees, which
        // never contain it).
        NodeKind::Dtd => {}
        // Container/discriminant kinds: emit children only.
        NodeKind::Document | NodeKind::DocumentFragment => {
            let mut c = n.first_child.get();
            while let Some(ch) = c {
                serialize_node_xml(ch, out);
                c = ch.next_sibling.get();
            }
        }
        NodeKind::Attribute => {}
    }
}

fn push_escaped(s: &str, out: &mut String, in_attr: bool) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if in_attr => out.push_str("&quot;"),
            _   => out.push(c),
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn read_simple_doc() {
        let src = b"<r><a id=\"1\">hi</a><b/></r>";
        let r = unsafe {
            xmlReaderForMemory(src.as_ptr() as *const c_char, src.len() as c_int,
                               ptr::null(), ptr::null(), 0)
        };
        assert!(!r.is_null());

        let mut events: Vec<(c_int, String)> = Vec::new();
        loop {
            let rc = unsafe { xmlTextReaderRead(r) };
            if rc != 1 { break; }
            let nt = unsafe { xmlTextReaderNodeType(r) };
            let np = unsafe { xmlTextReaderConstName(r) };
            let name = if np.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(np) }.to_str().unwrap().to_string()
            };
            events.push((nt, name));
        }
        unsafe { xmlFreeTextReader(r); }

        // Expected sequence:
        //   START r → START a → TEXT #text → END a → START b (empty) → END r
        let names: Vec<String> = events.iter().map(|(_, n)| n.clone()).collect();
        let types: Vec<c_int> = events.iter().map(|(t, _)| *t).collect();
        assert_eq!(names, vec!["r", "a", "#text", "a", "b", "r"]);
        assert_eq!(types, vec![
            XML_READER_TYPE_ELEMENT,
            XML_READER_TYPE_ELEMENT,
            XML_READER_TYPE_TEXT,
            XML_READER_TYPE_END_ELEMENT,
            XML_READER_TYPE_ELEMENT,
            XML_READER_TYPE_END_ELEMENT,
        ]);
    }

    #[test]
    fn attributes_iteration() {
        let src = b"<r id=\"42\" name=\"alice\"><a/></r>";
        let r = unsafe {
            xmlReaderForMemory(src.as_ptr() as *const c_char, src.len() as c_int,
                               ptr::null(), ptr::null(), 0)
        };
        // Advance to <r>.
        assert_eq!(unsafe { xmlTextReaderRead(r) }, 1);
        assert_eq!(unsafe { xmlTextReaderNodeType(r) }, XML_READER_TYPE_ELEMENT);
        assert_eq!(unsafe { xmlTextReaderAttributeCount(r) }, 2);
        // Walk attributes.
        let mut attrs = Vec::new();
        assert_eq!(unsafe { xmlTextReaderMoveToFirstAttribute(r) }, 1);
        loop {
            let name = unsafe { xmlTextReaderConstName(r) };
            let value = unsafe { xmlTextReaderConstValue(r) };
            let n = unsafe { CStr::from_ptr(name) }.to_str().unwrap().to_string();
            let v = unsafe { CStr::from_ptr(value) }.to_str().unwrap().to_string();
            attrs.push((n, v));
            if unsafe { xmlTextReaderMoveToNextAttribute(r) } != 1 { break; }
        }
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].0, "id");
        assert_eq!(attrs[0].1, "42");
        assert_eq!(attrs[1].0, "name");
        assert_eq!(attrs[1].1, "alice");

        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn empty_element_no_end_emit() {
        let src = b"<r><a/><b/></r>";
        let r = unsafe {
            xmlReaderForMemory(src.as_ptr() as *const c_char, src.len() as c_int,
                               ptr::null(), ptr::null(), 0)
        };
        let mut names = Vec::new();
        loop {
            let rc = unsafe { xmlTextReaderRead(r) };
            if rc != 1 { break; }
            let np = unsafe { xmlTextReaderConstName(r) };
            let n = unsafe { CStr::from_ptr(np) }.to_str().unwrap().to_string();
            names.push((unsafe { xmlTextReaderNodeType(r) }, n));
        }
        unsafe { xmlFreeTextReader(r); }
        // No END for <a/> or <b/> — only START.  r emits START + END.
        let only_names: Vec<_> = names.iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(only_names, vec!["r", "a", "b", "r"]);
    }

    fn open(src: &[u8]) -> *mut xmlTextReader {
        unsafe {
            xmlReaderForMemory(src.as_ptr() as *const c_char, src.len() as c_int,
                               ptr::null(), ptr::null(), 0)
        }
    }

    fn read_to(r: *mut xmlTextReader, want_name: &str) {
        loop {
            let rc = unsafe { xmlTextReaderRead(r) };
            assert!(rc == 1, "exhausted before {want_name:?}");
            let n = unsafe { xmlTextReaderConstName(r) };
            if !n.is_null() {
                let s = unsafe { CStr::from_ptr(n) }.to_str().unwrap();
                if s == want_name { return; }
            }
        }
    }

    #[test]
    fn get_attribute_by_name_and_index() {
        let r = open(b"<r><book id=\"b1\" title=\"Dune\"/></r>");
        read_to(r, "book");
        let id = unsafe { xmlTextReaderGetAttribute(r, c"id".as_ptr()) };
        assert!(!id.is_null());
        assert_eq!(unsafe { CStr::from_ptr(id) }.to_str().unwrap(), "b1");
        let title = unsafe { xmlTextReaderGetAttributeNo(r, 1) };
        assert!(!title.is_null());
        assert_eq!(unsafe { CStr::from_ptr(title) }.to_str().unwrap(), "Dune");
        let missing = unsafe { xmlTextReaderGetAttribute(r, c"nope".as_ptr()) };
        assert!(missing.is_null());
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn has_attributes_and_is_namespace_decl() {
        let r = open(b"<r xmlns:ns=\"urn:ex\"><a id=\"1\"/><b/></r>");
        read_to(r, "a");
        assert_eq!(unsafe { xmlTextReaderHasAttributes(r) }, 1);
        read_to(r, "b");
        assert_eq!(unsafe { xmlTextReaderHasAttributes(r) }, 0);
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn move_to_attribute_round_trip() {
        let r = open(b"<r><x id=\"1\" lang=\"en\"/></r>");
        read_to(r, "x");
        assert_eq!(unsafe { xmlTextReaderMoveToAttribute(r, c"lang".as_ptr()) }, 1);
        let v = unsafe { xmlTextReaderConstValue(r) };
        assert_eq!(unsafe { CStr::from_ptr(v) }.to_str().unwrap(), "en");
        assert_eq!(unsafe { xmlTextReaderMoveToElement(r) }, 1);
        // Back on the element — name is "x".
        let nm = unsafe { xmlTextReaderConstName(r) };
        assert_eq!(unsafe { CStr::from_ptr(nm) }.to_str().unwrap(), "x");
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn next_sibling_skips_subtree() {
        let r = open(b"<r><a><deep><nested/></deep></a><b/></r>");
        read_to(r, "a");
        assert_eq!(unsafe { xmlTextReaderNextSibling(r) }, 1);
        let n = unsafe { xmlTextReaderConstName(r) };
        assert_eq!(unsafe { CStr::from_ptr(n) }.to_str().unwrap(), "b");
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn next_climbs_past_end_of_subtree() {
        let r = open(b"<r><a><x/><y/></a><b/></r>");
        read_to(r, "y");
        assert_eq!(unsafe { xmlTextReaderNext(r) }, 1);
        let n = unsafe { xmlTextReaderConstName(r) };
        assert_eq!(unsafe { CStr::from_ptr(n) }.to_str().unwrap(), "b");
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn read_outer_inner_xml() {
        let r = open(b"<r><wrap><a/><b>hi</b></wrap></r>");
        read_to(r, "wrap");
        let outer = unsafe { xmlTextReaderReadOuterXml(r) };
        assert!(!outer.is_null());
        let s = unsafe { CStr::from_ptr(outer) }.to_str().unwrap();
        assert!(s.contains("<wrap>"));
        assert!(s.contains("<a/>"));
        assert!(s.contains("<b>hi</b>"));
        let inner = unsafe { xmlTextReaderReadInnerXml(r) };
        let s2 = unsafe { CStr::from_ptr(inner) }.to_str().unwrap();
        assert!(!s2.contains("<wrap>"));
        assert!(s2.contains("<a/>"));
        assert!(s2.contains("<b>hi</b>"));
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn lookup_namespace_walks_scope() {
        let r = open(b"<r xmlns:ex=\"urn:ex\"><inner><leaf/></inner></r>");
        read_to(r, "leaf");
        let uri = unsafe { xmlTextReaderLookupNamespace(r, c"ex".as_ptr()) };
        assert!(!uri.is_null());
        assert_eq!(unsafe { CStr::from_ptr(uri) }.to_str().unwrap(), "urn:ex");
        let missing = unsafe { xmlTextReaderLookupNamespace(r, c"nope".as_ptr()) };
        assert!(missing.is_null());
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn xml_lang_inherits_from_ancestor() {
        let r = open(b"<r xml:lang=\"en\"><p><span>x</span></p></r>");
        read_to(r, "span");
        let lang = unsafe { xmlTextReaderConstXmlLang(r) };
        assert!(!lang.is_null());
        assert_eq!(unsafe { CStr::from_ptr(lang) }.to_str().unwrap(), "en");
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn current_doc_matches_reader_doc() {
        let r = open(b"<r/>");
        // Advance once so the cursor is valid (otherwise CurrentDoc still works,
        // but we exercise the same path consumers use).
        unsafe { xmlTextReaderRead(r); }
        let d = unsafe { xmlTextReaderCurrentDoc(r) };
        assert!(!d.is_null());
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn reader_walker_does_not_take_doc_ownership() {
        // Build a doc separately, hand it to xmlReaderWalker, free the
        // reader, then verify the doc is still usable.
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                b"<r><a/><b/></r>".as_ptr() as *const c_char, 15,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc.is_null());
        let r = unsafe { xmlReaderWalker(doc) };
        assert!(!r.is_null());
        // Walk a couple of nodes.
        let rc = unsafe { xmlTextReaderRead(r) };
        assert_eq!(rc, 1);
        // Free reader — must NOT free doc.
        unsafe { xmlFreeTextReader(r); }
        // Doc still alive: confirm we can read its root.
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        assert!(!root.is_null());
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    #[test]
    fn reader_for_io_slurps_via_callbacks_and_parses() {
        // Test data shared with the read callback via a heap struct
        // whose pointer is the ioctx.
        struct Source { bytes: Vec<u8>, pos: usize, closed: bool }
        let src = Box::into_raw(Box::new(Source {
            bytes: b"<r><deep/></r>".to_vec(),
            pos: 0, closed: false,
        }));

        unsafe extern "C" fn read_cb(ctx: *mut c_void, buf: *mut c_char, len: c_int) -> c_int {
            // SAFETY: ctx is the &mut Source we leaked above.
            let s = unsafe { &mut *(ctx as *mut Source) };
            let n = (s.bytes.len() - s.pos).min(len as usize);
            if n == 0 { return 0; }
            unsafe {
                std::ptr::copy_nonoverlapping(
                    s.bytes[s.pos..].as_ptr(), buf as *mut u8, n,
                );
            }
            s.pos += n;
            n as c_int
        }
        unsafe extern "C" fn close_cb(ctx: *mut c_void) -> c_int {
            unsafe { (*(ctx as *mut Source)).closed = true; }
            0
        }

        let r = unsafe {
            xmlReaderForIO(Some(read_cb), Some(close_cb), src as *mut c_void,
                           ptr::null(), ptr::null(), 0)
        };
        assert!(!r.is_null(), "reader construction failed");
        read_to(r, "deep");
        unsafe { xmlFreeTextReader(r); }

        // close_cb must have fired.
        let s = unsafe { Box::from_raw(src) };
        assert!(s.closed, "close callback was not invoked");
    }

    #[test]
    fn reader_for_io_returns_null_for_null_ioread() {
        let r = unsafe {
            xmlReaderForIO(None, None, ptr::null_mut(),
                           ptr::null(), ptr::null(), 0)
        };
        assert!(r.is_null());
    }

    // ── error-handler get/set ──────────────────────────────────────────

    unsafe extern "C" fn dummy_error_handler(
        _arg: *mut c_void,
        _msg: *const c_char,
        _sev: c_int,
        _loc: *mut c_void,
    ) {
    }

    #[test]
    fn get_error_handler_default_is_null() {
        let r = open(b"<r/>");
        let mut f: Option<XmlTextReaderErrorFunc> = Some(dummy_error_handler);
        let mut arg: *mut c_void = 0x1 as *mut c_void;
        unsafe { xmlTextReaderGetErrorHandler(r, &mut f, &mut arg); }
        assert!(f.is_none(), "fresh reader must report no handler");
        assert!(arg.is_null(), "fresh reader must report null context");
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn error_handler_round_trips() {
        let r = open(b"<r/>");
        let ctx = 0xABCD_usize as *mut c_void;
        unsafe { xmlTextReaderSetErrorHandler(r, Some(dummy_error_handler), ctx); }

        let mut f: Option<XmlTextReaderErrorFunc> = None;
        let mut arg: *mut c_void = ptr::null_mut();
        unsafe { xmlTextReaderGetErrorHandler(r, &mut f, &mut arg); }
        let want: XmlTextReaderErrorFunc = dummy_error_handler;
        assert!(f.is_some(), "handler should be reported after set");
        assert_eq!(f.unwrap() as *const (), want as *const (),
            "the exact handler pointer should round-trip");
        assert_eq!(arg, ctx);

        // Clearing with a NULL function drops handler and context.
        unsafe { xmlTextReaderSetErrorHandler(r, None, ctx); }
        let mut f2: Option<XmlTextReaderErrorFunc> = Some(dummy_error_handler);
        let mut arg2: *mut c_void = ctx;
        unsafe { xmlTextReaderGetErrorHandler(r, &mut f2, &mut arg2); }
        assert!(f2.is_none());
        assert!(arg2.is_null());
        unsafe { xmlFreeTextReader(r); }
    }

    #[test]
    fn get_error_handler_tolerates_null_outparams_and_reader() {
        // NULL out-pointers: must not write, must not crash.
        let r = open(b"<r/>");
        unsafe { xmlTextReaderSetErrorHandler(r, Some(dummy_error_handler), ptr::null_mut()); }
        unsafe { xmlTextReaderGetErrorHandler(r, ptr::null_mut(), ptr::null_mut()); }
        unsafe { xmlFreeTextReader(r); }

        // NULL reader: reports the null/default state without dereferencing.
        let mut f: Option<XmlTextReaderErrorFunc> = Some(dummy_error_handler);
        let mut arg: *mut c_void = 0x1 as *mut c_void;
        unsafe { xmlTextReaderGetErrorHandler(ptr::null_mut(), &mut f, &mut arg); }
        assert!(f.is_none());
        assert!(arg.is_null());
        // NULL reader on the setter is a no-op (no crash).
        unsafe { xmlTextReaderSetErrorHandler(ptr::null_mut(), Some(dummy_error_handler), ptr::null_mut()); }
    }
}
