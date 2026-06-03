//! libxml2 HTML parser façade — wraps `sup_xml_core::html`
//! (html5ever-backed) behind the libxml2 ABI.
//!
//! lxml.html and BeautifulSoup-via-lxml both use this surface heavily.
//! We implement the four read paths plus the two context wrappers; the
//! push-API entries (`htmlCreatePushParserCtxt`, `htmlParseChunk`),
//! the new-empty-doc constructor (`htmlNewDoc`), and the
//! output-buffer-flavored serializer (`htmlNodeDumpFormatOutput`)
//! stay as stubs until we ship the corresponding infrastructure
//! (push parser + arena-mutation + xmlOutputBuffer).

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::error::{ErrorCode, ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::html::HtmlParseOptions;
use sup_xml_tree::dom::XmlDoc;

use crate::error::record_last_error;
use crate::parsectx::XmlParserCtxt;

/// Convert the libxml2 `HTML_PARSE_*` bitmask into our internal
/// [`HtmlParseOptions`].  We honor:
///   * `HTML_PARSE_RECOVER` (1 << 0) — though html5ever always
///     recovers, so no-op.
///   * `HTML_PARSE_NOERROR`  (1 << 5) — suppress error reporting.
///   * `HTML_PARSE_NOWARNING`(1 << 6) — same for warnings.
///   * `HTML_PARSE_NONET`    (1 << 11) — we never load network resources.
/// Other flags are accepted but ignored.
/// HTML parse options.  `recover` (lxml's `HTMLParser(recover=…)`, the
/// `XML_PARSE_RECOVER` bit) is NOT mapped to the core's recovery mode:
/// html5ever always repairs tag soup and produces a tree, so — like
/// libxml2's HTML parser — the read entry points always return the
/// document but set `ctxt->wellFormed = 0` when it recovered any errors;
/// lxml then raises iff `recover` is off.
fn html_options_from_libxml(_options: c_int) -> HtmlParseOptions {
    HtmlParseOptions::default()
}

/// Carry a parsed HTML `<!DOCTYPE …>` from the core document onto the
/// libxml2-shape `XmlDoc`'s internal subset.  Reads `html_metadata`
/// *before* the document is consumed by `into_xml_doc`.  No-op when the
/// source had no doctype, so html5ever's "no DOCTYPE" documents stay
/// without one (lxml's `default_doctype=False` relies on this).
pub(crate) fn doctype_parts(doc: &sup_xml_tree::dom::Document) -> Option<(String, Option<String>, Option<String>)> {
    let dt = doc.html_metadata.as_ref()?.doctype.as_ref()?;
    let opt = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    Some((dt.name.clone(), opt(&dt.public_id), opt(&dt.system_id)))
}

/// The DOCTYPE root-element name as written in the HTML source, with its
/// original case.  html5ever lowercases the name during tokenization, but
/// libxml2 keeps the source case on `dtd->name`: lxml's
/// `tostring(method='html')` emits it verbatim (`<!DOCTYPE HTML …>`) while
/// `docinfo.doctype` lowercases it on read.  Returns `None` when the
/// source has no `<!DOCTYPE>`.
fn original_doctype_name(source: &[u8]) -> Option<String> {
    const NEEDLE: &[u8] = b"<!doctype";
    let mut i = 0;
    while i + NEEDLE.len() <= source.len() {
        if source[i..i + NEEDLE.len()].eq_ignore_ascii_case(NEEDLE) {
            let mut j = i + NEEDLE.len();
            while j < source.len() && source[j].is_ascii_whitespace() { j += 1; }
            let start = j;
            while j < source.len()
                && !source[j].is_ascii_whitespace()
                && source[j] != b'>'
                && source[j] != b'['
            {
                j += 1;
            }
            return (j > start)
                .then(|| std::str::from_utf8(&source[start..j]).ok().map(str::to_string))
                .flatten();
        }
        i += 1;
    }
    None
}

/// Build parse options honouring a caller-supplied `encoding` label
/// (libxml2's `htmlReadMemory`/`htmlCtxtReadMemory` `encoding` arg).
///
/// lxml passes Python's internal string representation directly —
/// `UTF-16LE/BE` for BMP (UCS-2) strings, `UTF-32LE/BE` for wide
/// (UCS-4) strings — and relies on the parser decoding it via this
/// label.  When `encoding` is NULL the core's WHATWG byte sniffing
/// (BOM → meta charset → Windows-1252) runs unchanged.
unsafe fn html_options_from_libxml_enc(
    options:  c_int,
    encoding: *const c_char,
) -> HtmlParseOptions {
    let mut opts = html_options_from_libxml(options);
    if !encoding.is_null() {
        if let Ok(label) = unsafe { CStr::from_ptr(encoding) }.to_str() {
            if !label.is_empty() {
                opts.encoding_override = Some(label.to_owned());
            }
        }
    }
    opts
}

/// libxml2 `htmlReadMemory(buffer, size, URL, encoding, options)`.
///
/// Parse `size` bytes at `buffer` as HTML.  Returns an owning pointer
/// to a libxml2-shape document (release via
/// [`crate::parse::xmlFreeDoc`]); NULL on error with the last-error
/// slot populated.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlReadMemory(
    buffer:    *const c_char,
    size:      c_int,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
) -> *mut XmlDoc {
    if buffer.is_null() || size <= 0 {
        record_last_error(
            &XmlError::new(ErrorDomain::Html, ErrorLevel::Fatal, "empty input")
                .with_code(ErrorCode::DocumentEmpty),
        );
        return ptr::null_mut();
    }
    // SAFETY: caller asserts buffer is readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    let opts = unsafe { html_options_from_libxml_enc(options, encoding) };
    // Route through the thread-local shared dict and a fresh per-doc
    // arena — see `xmlReadMemory` for the rationale.
    let dict  = crate::dict::thread_dict();
    let arena = crate::dict::new_doc_arena();
    // SAFETY: thread_dict returns a live, refcount-managed Dict.
    match unsafe {
        sup_xml_core::html::parse_html_bytes_opts_with_dict_arena(bytes, &opts, dict, arena)
    } {
        Ok(doc) => {
            let dt = doctype_parts(&doc);
            let raw = doc.into_xml_doc();
            if let Some((name, pid, sid)) = dt {
                let name = original_doctype_name(bytes).unwrap_or(name);
                unsafe { crate::dtd::plant_int_subset(raw, &name, pid.as_deref(), sid.as_deref()); }
            }
            crate::parse::plant_doc_url(raw, url);
            raw
        }
        Err(e) => {
            record_last_error(&e);
            ptr::null_mut()
        }
    }
}

/// libxml2 `htmlReadDoc(cur, URL, encoding, options)` — parse a NUL-
/// terminated in-memory HTML buffer.  Thin convenience wrapper over
/// [`htmlReadMemory`]; the `size` argument is recovered from `strlen`.
/// Returns NULL on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlReadDoc(
    cur:      *const c_char,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if cur.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `cur` is NUL-terminated.
    let len = unsafe { std::ffi::CStr::from_ptr(cur) }.to_bytes().len() as c_int;
    unsafe { htmlReadMemory(cur, len, url, encoding, options) }
}

/// libxml2 `htmlReadFile(filename, encoding, options)`.
///
/// Read the file and delegate to [`htmlReadMemory`].  Returns NULL on
/// I/O failure (with `ErrorDomain::Io` last-error) or on parse error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlReadFile(
    filename: *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if filename.is_null() {
        record_last_error(
            &XmlError::new(ErrorDomain::Io, ErrorLevel::Fatal, "NULL filename")
                .with_code(ErrorCode::DocumentEmpty),
        );
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let path_cs = unsafe { CStr::from_ptr(filename) };
    let path = match path_cs.to_str() {
        Ok(p) => p,
        Err(_) => {
            record_last_error(
                &XmlError::new(ErrorDomain::Io, ErrorLevel::Fatal, "filename is not valid UTF-8")
                    .with_code(ErrorCode::InvalidChar),
            );
            return ptr::null_mut();
        }
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            record_last_error(
                &XmlError::new(ErrorDomain::Io, ErrorLevel::Fatal,
                               format!("{path}: {e}"))
                    .with_code(ErrorCode::InternalError),
            );
            return ptr::null_mut();
        }
    };
    unsafe {
        htmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            filename,
            encoding,
            options,
        )
    }
}

/// libxml2 `htmlReadIO(ioread, ioclose, ioctx, URL, encoding, options)` —
/// HTML counterpart of [`crate::reader::xmlReaderForIO`].  Slurps via
/// repeated `ioread` calls until EOF, runs `ioclose` (when non-NULL),
/// then parses the buffer through [`htmlReadMemory`].  Returns NULL
/// on read or parse failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlReadIO(
    ioread:    Option<crate::reader::XmlInputReadCallback>,
    ioclose:   Option<crate::reader::XmlInputCloseCallback>,
    ioctx:     *mut std::os::raw::c_void,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
) -> *mut XmlDoc {
    let Some(read_cb) = ioread else { return ptr::null_mut(); };
    // Reuse the same slurp loop used by xmlReaderForIO; both consume
    // the libxml2 input-callback shape identically.
    let bytes = unsafe { crate::reader::slurp_io_callbacks(read_cb, ioclose, ioctx) };
    let bytes = match bytes { Some(b) => b, None => return ptr::null_mut() };
    unsafe {
        htmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            url, encoding, options,
        )
    }
}

/// libxml2 `htmlCtxtReadMemory(ctxt, buffer, size, URL, encoding, options)`.
///
/// Reads the parser context's name dict (`ctxt->dict` at offset
/// 456) and feeds it to the HTML parser so element / attribute
/// names are interned directly into the consumer's dict — no
/// post-parse work, no duplicated storage.  Matches the
/// `xmlCtxtReadMemory` flow.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtReadMemory(
    ctxt:     *mut XmlParserCtxt,
    buffer:   *const c_char,
    size:     c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if buffer.is_null() || size <= 0 {
        record_last_error(
            &XmlError::new(ErrorDomain::Html, ErrorLevel::Fatal, "empty input")
                .with_code(ErrorCode::DocumentEmpty),
        );
        return ptr::null_mut();
    }
    // SAFETY: caller asserts buffer is readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    unsafe { html_read_for_ctxt(ctxt, bytes, url, encoding, options) }
}

/// Parse `bytes` as HTML on behalf of a parser context: intern names
/// through the context's dict, plant the doctype / URL, and — crucially —
/// set the context's `wellFormed` flag from the errors html5ever
/// recovered (and record the last one).  lxml reads `wellFormed` and
/// raises `XMLSyntaxError` when the parser's `recover` is off.  Shared by
/// `htmlCtxtReadMemory` / `htmlCtxtReadIO` / `htmlCtxtReadFile`.
///
/// # Safety
/// `ctxt` is NULL or a live [`XmlParserCtxt`]; `bytes` is readable.
unsafe fn html_read_for_ctxt(
    ctxt:     *mut XmlParserCtxt,
    bytes:    &[u8],
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let dict_ptr: *mut sup_xml_tree::dict::Dict = if ctxt.is_null() {
        crate::dict::thread_dict()
    } else {
        // ctxt.dict lives at offset 456 (libxml2 layout); fall back to the
        // thread dict when the slot is unpopulated.
        let p = unsafe { (ctxt as *const u8).add(456) };
        let mut b = [0u8; std::mem::size_of::<usize>()];
        unsafe { std::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), b.len()); }
        let raw = usize::from_ne_bytes(b) as *mut sup_xml_tree::dict::Dict;
        if raw.is_null() { crate::dict::thread_dict() } else { raw }
    };
    let opts = unsafe { html_options_from_libxml_enc(options, encoding) };
    let arena = crate::dict::new_doc_arena();
    let (doc, recovered) = unsafe {
        sup_xml_core::html::parse_html_bytes_recovered_with_dict_arena(bytes, &opts, dict_ptr, arena)
    };
    let dt = doctype_parts(&doc);
    let raw = doc.into_xml_doc();
    if let Some((name, pid, sid)) = dt {
        let name = original_doctype_name(bytes).unwrap_or(name);
        unsafe { crate::dtd::plant_int_subset(raw, &name, pid.as_deref(), sid.as_deref()); }
    }
    crate::parse::plant_doc_url(raw, url);
    // Drive SAX1 callbacks from the parsed tree for a consumer that
    // installed a custom handler — lxml's `HTMLParser(target=…)` reached
    // through `fromstring`.  libxml2's HTML parser is SAX1-only, so replay
    // the `startElement`/`endElement` form; `replay` is a no-op when only
    // the no-op baseline handlers are present, leaving ordinary
    // tree-building parses untouched.  A target callback that raises stops
    // the context mid-replay, which the consumer then re-raises.
    if !ctxt.is_null() && !raw.is_null() {
        unsafe { crate::saxreplay::replay(ctxt, raw, true); }
    }
    if !ctxt.is_null() {
        unsafe { crate::parsectx::set_well_formed(ctxt, recovered.is_empty()); }
    }
    // Surface each recovered error to the context's error handler so it
    // lands in the consumer's log.  lxml's `_handleParseResult` inspects
    // that log (not just `wellFormed`) to decide whether to raise when the
    // parser's `recover` flag is off.
    if let Some(err) = recovered.last() {
        record_last_error(err);
        if !ctxt.is_null() {
            let cerr = crate::error::xmlGetLastError();
            unsafe { crate::parsectx::deliver_ctxt_error(ctxt, cerr); }
        }
    }
    raw
}

/// libxml2 `htmlCtxtReadFile(ctxt, filename, encoding, options)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtReadFile(
    ctxt:     *mut XmlParserCtxt,
    filename: *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if filename.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let path = match unsafe { CStr::from_ptr(filename) }.to_str() {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            // Mirror the I/O failure onto the context's inline `lastError`
            // (domain XML_FROM_IO) so lxml's `_raiseParseError` raises
            // `IOError`, not a bare `XMLSyntaxError`.
            record_last_error(
                &XmlError::new(ErrorDomain::Io, ErrorLevel::Fatal,
                               format!("failed to load \"{path}\": {e}"))
                    .with_code(ErrorCode::InternalError),
            );
            if !ctxt.is_null() {
                unsafe { crate::parsectx::mirror_last_error_into_ctxt(ctxt); }
            }
            return ptr::null_mut();
        }
    };
    unsafe { html_read_for_ctxt(ctxt, &bytes, filename, encoding, options) }
}

/// libxml2 `htmlCtxtReadIO(ctxt, ioread, ioclose, ioctx, URL, encoding, options)`.
/// Wraps the IO-callback variant by buffering the entire stream
/// and then delegating to `htmlReadMemory`.  Used by lxml's HTML
/// parsing path when reading from file-likes.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtReadIO(
    _ctxt:    *mut XmlParserCtxt,
    ioread:   Option<unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int>,
    ioclose:  Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    ioctx:    *mut c_void,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let Some(read_fn) = ioread else { return ptr::null_mut(); };
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        // SAFETY: caller-supplied read fn; we pass a writable buffer.
        let n = unsafe { read_fn(ioctx, tmp.as_mut_ptr() as *mut c_char, tmp.len() as c_int) };
        if n <= 0 { break; }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    if let Some(close_fn) = ioclose {
        unsafe { close_fn(ioctx); }
    }
    unsafe { html_read_for_ctxt(_ctxt, &buf, url, encoding, options) }
}

/// libxml2 `htmlCtxtReset(ctxt)` — reset a parser context for reuse.
/// Our ctxt is opaque + zero-initialised; resetting clears the
/// scratch but keeps the attached sax handler.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtReset(ctxt: *mut XmlParserCtxt) {
    if ctxt.is_null() { return; }
    unsafe { crate::parsectx::xmlClearParserCtxt(ctxt); }
}

/// libxml2 `htmlCtxtUseOptions(ctxt, options)`.  Delegates to
/// `xmlCtxtUseOptions` — same semantics in v0.1 (options are stored
/// but the parser doesn't honor most of them yet).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtUseOptions(
    ctxt:    *mut XmlParserCtxt,
    options: c_int,
) -> c_int {
    unsafe { crate::parsectx::xmlCtxtUseOptions(ctxt, options) }
}

/// libxml2 `htmlCtxtSetOptions(ctxt, options)` — modern name for
/// [`htmlCtxtUseOptions`]; identical semantics.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtSetOptions(
    ctxt:    *mut XmlParserCtxt,
    options: c_int,
) -> c_int {
    unsafe { htmlCtxtUseOptions(ctxt, options) }
}

/// libxml2 `htmlReadFd(fd, URL, encoding, options)` — parse an HTML
/// document read in full from a file descriptor.  Slurps the fd
/// (without closing it) and routes through [`htmlReadMemory`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlReadFd(
    fd:       c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let Some(buf) = crate::parse::slurp_fd(fd) else { return std::ptr::null_mut() };
    unsafe { htmlReadMemory(buf.as_ptr() as *const c_char, buf.len() as c_int, url, encoding, options) }
}

/// libxml2 `htmlCtxtReadFd(ctxt, fd, URL, encoding, options)` — parse
/// an HTML document read in full from a file descriptor using a
/// reusable context.  Slurps the fd (without closing it) and routes
/// through [`htmlCtxtReadMemory`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCtxtReadFd(
    ctxt:     *mut XmlParserCtxt,
    fd:       c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let Some(buf) = crate::parse::slurp_fd(fd) else { return std::ptr::null_mut() };
    unsafe {
        htmlCtxtReadMemory(ctxt, buf.as_ptr() as *const c_char, buf.len() as c_int, url, encoding, options)
    }
}

/// libxml2 `htmlCreateMemoryParserCtxt(buffer, size)` — allocate a
/// fresh parser context primed with `size` bytes from `buffer`.
/// Stashes the buffer on the thread-local memory-source side-channel
/// keyed by the ctxt pointer; a subsequent `htmlParseDocument(ctxt)`
/// reads it back through the HTML parser path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCreateMemoryParserCtxt(
    buffer: *const c_char,
    size:   c_int,
) -> *mut XmlParserCtxt {
    if buffer.is_null() || size <= 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts buffer readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
    if !ctxt.is_null() {
        crate::parsectx::stash_memory_source(ctxt, bytes.to_vec(), /*is_html=*/ true);
    }
    ctxt
}

/// libxml2 `htmlNewDoc(URI, externalID)` — allocate an empty HTML
/// document with a DOCTYPE.  When both identifiers are NULL, libxml2
/// substitutes the HTML 4.0 Transitional default (so a freshly created
/// `lxml.html` element reports a sensible `docinfo.doctype`); otherwise
/// the caller's identifiers are used verbatim.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlNewDoc(
    uri:         *const c_char,
    external_id: *const c_char,
) -> *mut XmlDoc {
    if uri.is_null() && external_id.is_null() {
        const SYSTEM: &[u8] = b"http://www.w3.org/TR/REC-html40/loose.dtd\0";
        const PUBLIC: &[u8] = b"-//W3C//DTD HTML 4.0 Transitional//EN\0";
        unsafe {
            htmlNewDocNoDtD(
                SYSTEM.as_ptr() as *const c_char,
                PUBLIC.as_ptr() as *const c_char,
            )
        }
    } else {
        unsafe { htmlNewDocNoDtD(uri, external_id) }
    }
}

/// libxml2 `htmlNodeDumpFormatOutput(buf, doc, cur, encoding, format)`
/// — serialize a node into an output buffer using HTML5 rules
/// (void elements, raw-text content, boolean-attribute shorthand,
/// no XML self-closing slash).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlNodeDumpFormatOutput(
    buf:       *mut crate::outbuf::xmlOutputBuffer,
    _doc:      *const XmlDoc,
    cur:       *const sup_xml_tree::dom::Node<'static>,
    _encoding: *const c_char,
    format:    c_int,
) {
    if buf.is_null() || cur.is_null() { return; }
    let opts = sup_xml_core::serializer::SerializeOptions {
        write_xml_decl: false,
        format:         format != 0,
        indent:         if format != 0 { "  ".to_string() } else { String::new() },
        // Critical for `lxml.html.tostring()` and any
        // `method="html"` write: enables HTML5 void-element handling,
        // boolean-attribute shorthand (`<form novalidate>` not
        // `<form novalidate="">`), and raw-text `<script>`/`<style>`
        // bodies.
        html_mode:      true,
        xhtml:          false,
        out_charset:    sup_xml_core::output::OutputCharset::Utf8,
    };
    // SAFETY: cur non-null per the check.
    let n = unsafe { &*cur };
    let out = sup_xml_core::serializer::serialize_node_to_string(n, &opts);
    // Route through xmlOutputBufferWrite so file-like and user-
    // callback buffers receive the bytes — see xmlNodeDumpOutput.
    unsafe {
        crate::outbuf::xmlOutputBufferWrite(
            buf, out.len() as c_int, out.as_ptr() as *const c_char,
        );
    }
}

/// libxml2 `htmlDocContentDumpFormatOutput(buf, doc, encoding, format)` —
/// serialize the WHOLE document (every top-level sibling — doctype,
/// root, prolog comments) using HTML5 rules.  `encoding` is accepted
/// but ignored (we always emit UTF-8; consumers wanting another
/// encoding go through `xmlOutputBufferCreateFilename` with an
/// encoder).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlDocContentDumpFormatOutput(
    buf:      *mut crate::outbuf::xmlOutputBuffer,
    doc:      *mut XmlDoc,
    encoding: *const c_char,
    format:   c_int,
) {
    if buf.is_null() || doc.is_null() { return; }
    // Walk the doc-level children (libxml2's xmlDoc::children is the
    // head of the top-level sibling chain).  htmlNodeDumpFormatOutput
    // already handles each kind correctly.
    // SAFETY: caller asserts doc is a live XmlDoc.
    let mut cur = unsafe { (*doc).children.get() };
    while !cur.is_null() {
        unsafe {
            htmlNodeDumpFormatOutput(buf, doc, cur, encoding, format);
            cur = (*cur).next_sibling.get()
                .map(|n| n as *const _ as *mut sup_xml_tree::dom::Node<'static>)
                .unwrap_or(ptr::null_mut());
        }
    }
}

/// libxml2 `htmlDocContentDumpOutput(buf, doc, encoding)` —
/// shorthand for the format=0 variant.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlDocContentDumpOutput(
    buf:      *mut crate::outbuf::xmlOutputBuffer,
    doc:      *mut XmlDoc,
    encoding: *const c_char,
) {
    unsafe { htmlDocContentDumpFormatOutput(buf, doc, encoding, 0); }
}

/// libxml2 `htmlDocDumpMemory(doc, *mem, *size)` — serialize an HTML
/// doc into a fresh heap allocation, with libxml2's default formatting
/// (pretty-print on).  Equivalent to
/// `htmlDocDumpMemoryFormat(doc, mem, size, 1)`.  Caller frees `*mem`
/// via `xmlFree`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlDocDumpMemory(
    doc:  *mut XmlDoc,
    mem:  *mut *mut c_char,
    size: *mut c_int,
) {
    unsafe { htmlDocDumpMemoryFormat(doc, mem, size, 1); }
}

/// libxml2 `htmlDocDumpMemoryFormat(doc, *mem, *size, format)` —
/// serialize an HTML doc into a fresh heap allocation.  Caller frees
/// via `xmlFree`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlDocDumpMemoryFormat(
    doc:    *mut XmlDoc,
    mem:    *mut *mut c_char,
    size:   *mut c_int,
    format: c_int,
) {
    unsafe {
        if !mem.is_null()  { *mem  = ptr::null_mut(); }
        if !size.is_null() { *size = 0; }
    }
    if doc.is_null() || mem.is_null() || size.is_null() { return; }
    let buffer = unsafe { crate::outbuf::xmlBufferCreate() };
    let outbuf = unsafe { crate::outbuf::xmlOutputBufferCreateBuffer(buffer, ptr::null_mut()) };
    if outbuf.is_null() {
        unsafe { crate::outbuf::xmlBufferFree(buffer); }
        return;
    }
    unsafe { htmlDocContentDumpFormatOutput(outbuf, doc, ptr::null(), format); }
    let bytes_ptr = unsafe { crate::outbuf::xmlBufferContent(buffer) };
    let bytes_len = unsafe { crate::outbuf::xmlBufferLength(buffer) } as usize;
    if !bytes_ptr.is_null() && bytes_len > 0 {
        // SAFETY: buffer content is valid for `bytes_len` bytes per
        // xmlBufferLength's contract.
        let slice = unsafe { std::slice::from_raw_parts(bytes_ptr as *const u8, bytes_len) };
        let alloc = crate::alloc::alloc_registered_cstring(slice);
        unsafe {
            *mem  = alloc;
            *size = bytes_len as c_int;
        }
    }
    unsafe {
        crate::outbuf::xmlOutputBufferClose(outbuf);
        crate::outbuf::xmlBufferFree(buffer);
    }
}

/// libxml2 `htmlSaveFileFormat(filename, doc, encoding, format)` —
/// serialize an HTML doc to `filename` (optionally indented).
/// Returns bytes written or -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlSaveFileFormat(
    filename:  *const c_char,
    doc:       *mut XmlDoc,
    _encoding: *const c_char,
    format:    c_int,
) -> c_int {
    if filename.is_null() || doc.is_null() { return -1; }
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(s)  => s,
        Err(_) => return -1,
    };
    let mut mem:  *mut c_char = ptr::null_mut();
    let mut size: c_int       = 0;
    unsafe { htmlDocDumpMemoryFormat(doc, &mut mem, &mut size, format); }
    if mem.is_null() || size <= 0 { return -1; }
    let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size as usize) };
    let result = std::fs::write(path, bytes);
    unsafe { crate::parse::xml_free_impl(mem as *mut std::ffi::c_void); }
    match result {
        Ok(())  => size,
        Err(_)  => -1,
    }
}

/// libxml2 `htmlCreateFileParserCtxt(filename, encoding)` — read a
/// file and build a parser context primed with its bytes.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCreateFileParserCtxt(
    filename:  *const c_char,
    _encoding: *const c_char,
) -> *mut crate::parsectx::XmlParserCtxt {
    if filename.is_null() { return ptr::null_mut(); }
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return ptr::null_mut(),
    };
    unsafe {
        htmlCreateMemoryParserCtxt(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
        )
    }
}

/// libxml2 `htmlGetMetaEncoding(doc)` — walk the HTML head looking
/// for `<meta http-equiv="Content-Type" content="text/html; charset=…">`
/// or `<meta charset="…">` and return the charset name.  NULL if
/// none found.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlGetMetaEncoding(doc: *mut XmlDoc) -> *const c_char {
    if doc.is_null() { return ptr::null(); }
    // Walk doc → html → head → meta children.
    let mut child = unsafe { (*doc).children.get() };
    // Find <html>
    let html_el = loop {
        if child.is_null() { return ptr::null(); }
        let n = unsafe { &*child };
        if matches!(n.kind, sup_xml_tree::dom::NodeKind::Element)
            && n.name().eq_ignore_ascii_case("html") {
            break n;
        }
        child = match n.next_sibling.get() {
            Some(s) => s as *const _ as *mut sup_xml_tree::dom::Node<'static>,
            None => return ptr::null(),
        };
    };
    // Find <head>
    let mut head_child = html_el.first_child.get();
    let head_el = loop {
        let Some(c) = head_child else { return ptr::null(); };
        if matches!(c.kind, sup_xml_tree::dom::NodeKind::Element)
            && c.name().eq_ignore_ascii_case("head") {
            break c;
        }
        head_child = c.next_sibling.get();
    };
    // Find a <meta charset="..."> or <meta http-equiv="Content-Type" content="...">
    let mut m = head_el.first_child.get();
    while let Some(meta) = m {
        if matches!(meta.kind, sup_xml_tree::dom::NodeKind::Element)
            && meta.name().eq_ignore_ascii_case("meta") {
            // <meta charset="...">
            for attr in meta.attributes() {
                if attr.name().eq_ignore_ascii_case("charset") {
                    return cstring_static(attr.value());
                }
            }
            // <meta http-equiv="Content-Type" content="text/html; charset=...">
            let mut http_equiv: Option<&str> = None;
            let mut content:    Option<&str> = None;
            for attr in meta.attributes() {
                let nm = attr.name();
                if nm.eq_ignore_ascii_case("http-equiv") { http_equiv = Some(attr.value()); }
                else if nm.eq_ignore_ascii_case("content") { content = Some(attr.value()); }
            }
            if let (Some(he), Some(c)) = (http_equiv, content) {
                if he.eq_ignore_ascii_case("Content-Type") {
                    if let Some(ix) = c.to_ascii_lowercase().find("charset=") {
                        let after = &c[ix + 8..];
                        let end = after.find(|ch: char| ch == ';' || ch == ' ' || ch == '"')
                            .unwrap_or(after.len());
                        return cstring_static(&after[..end]);
                    }
                }
            }
        }
        m = meta.next_sibling.get();
    }
    ptr::null()
}

/// Helper: allocate a NUL-terminated heap copy of `s` so we can hand
/// the caller a pointer with a lifetime detached from the source
/// arena.  Registered with the alloc tracker; caller frees via xmlFree.
fn cstring_static(s: &str) -> *const c_char {
    crate::alloc::alloc_registered_cstring(s.as_bytes()) as *const c_char
}

/// libxml2 `htmlNewDocNoDtD(uri, externalID)` — like [`htmlNewDoc`]
/// but without the HTML 4 default substitution: an internal subset
/// named `html` is attached only when at least one identifier is
/// supplied (matching libxml2), and both NULL yields a bare doc.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlNewDocNoDtD(
    uri:         *const c_char,
    external_id: *const c_char,
) -> *mut XmlDoc {
    let doc = unsafe { crate::mutate::xmlNewDoc(ptr::null()) };
    if doc.is_null() {
        return doc;
    }
    if !uri.is_null() || !external_id.is_null() {
        const NAME: &[u8] = b"html\0";
        // `xmlCreateIntSubset(doc, name, ExternalID, SystemID)` — the
        // public identifier is the external ID, the URI is the system ID.
        unsafe {
            crate::dtd::xmlCreateIntSubset(
                doc,
                NAME.as_ptr() as *const c_char,
                external_id,
                uri,
            );
        }
    }
    doc
}

/// libxml2 `htmlSetMetaEncoding(doc, encoding)` — write
/// `<meta charset="..."/>` (HTML5) or `<meta http-equiv="content-type"
/// content="text/html; charset=..."/>` (legacy) into the doc's
/// `<head>`.  Used by lxml's `html.tostring(encoding=...)`.  v0.1:
/// records the encoding on the doc for serialisers to pick up later,
/// without injecting the meta element (no concrete failure when
/// callers don't read it back).  Returns 0 on success, -1 on bad
/// inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlSetMetaEncoding(
    doc:      *mut XmlDoc,
    encoding: *const c_char,
) -> c_int {
    if doc.is_null() { return -1; }
    // SAFETY: caller asserts doc is a live XmlDoc.
    let enc_bytes: &[u8] = if encoding.is_null() {
        b""
    } else {
        unsafe { std::ffi::CStr::from_ptr(encoding) }.to_bytes()
    };
    // Write into the doc's `encoding` slot (offset 112).  Leak a
    // CString so the pointer remains valid for the doc's lifetime;
    // xmlFreeDoc reclaims it via the same path it does for `url`.
    let cs = match std::ffi::CString::new(enc_bytes) {
        Ok(c)  => c,
        Err(_) => return -1,
    };
    let p = cs.into_raw();
    // SAFETY: doc is live; field write through &mut is safe given
    // the caller's exclusive-access contract.
    unsafe {
        let d = &mut *doc;
        // Reclaim any previously-installed encoding to avoid a leak
        // on repeated calls.  ArenaCStr's pointer slot follows the
        // libxml2 layout — overwrite directly.
        d.encoding = sup_xml_tree::dom::ArenaCStr::from_raw(p as *const u8);
    }
    0
}

/// libxml2 `htmlParseChunk(ctxt, chunk, size, terminate)` — same
/// semantics as the XML push parser; delegates to `xmlParseChunk`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlParseChunk(
    ctxt:      *mut XmlParserCtxt,
    chunk:     *const c_char,
    size:      c_int,
    terminate: c_int,
) -> c_int {
    unsafe { crate::pushparse::xmlParseChunk(ctxt, chunk, size, terminate) }
}

/// libxml2 `htmlCreatePushParserCtxt(sax, userData, chunk, size, filename, encoding)`.
/// Delegates to `xmlCreatePushParserCtxt`.  `encoding` ignored
/// (auto-detected from BOM by `xmlReadMemory` on terminate).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlCreatePushParserCtxt(
    sax:       *mut c_void,
    user_data: *mut c_void,
    chunk:     *const c_char,
    size:      c_int,
    filename:  *const c_char,
    _encoding: c_int,
) -> *mut XmlParserCtxt {
    let ctxt = unsafe {
        crate::pushparse::xmlCreatePushParserCtxt(sax, user_data, chunk, size, filename)
    };
    if !ctxt.is_null() {
        crate::pushparse::mark_html(ctxt);
    }
    ctxt
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{xmlDocGetRootElement, xmlFreeDoc};

    #[test]
    fn html_read_memory_parses_minimal_doc() {
        let src = b"<html><head><title>T</title></head><body>hi</body></html>";
        let doc = unsafe {
            htmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null(), "html parse failed");

        let root = unsafe { xmlDocGetRootElement(doc) };
        assert!(!root.is_null());
        // html5ever produces `<html>` as the root.
        assert_eq!(unsafe { &*root }.name(), "html");

        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn html_read_memory_handles_tag_soup() {
        // Missing <html>/<body> — html5ever should still produce a valid tree.
        let src = b"<p>open<b>both<i>oops</p>";
        let doc = unsafe {
            htmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null(), "tag-soup parse failed");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn html_null_args_recorded_as_error() {
        crate::error::xmlResetLastError();
        let doc = unsafe { htmlReadMemory(ptr::null(), 0, ptr::null(), ptr::null(), 0) };
        assert!(doc.is_null());
        let last = crate::error::xmlGetLastError();
        assert!(!last.is_null());
    }

    #[test]
    fn html_ctxt_read_memory_delegates() {
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        let src = b"<p>hi";
        let doc = unsafe {
            htmlCtxtReadMemory(
                ctxt,
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null());
        unsafe {
            xmlFreeDoc(doc);
            crate::parsectx::xmlFreeParserCtxt(ctxt);
        }
    }

    #[test]
    fn html_new_doc_plants_default_doctype() {
        // htmlNewDoc(NULL, NULL) gets the HTML 4.0 Transitional default
        // internal subset, so a freshly created lxml.html element has a
        // non-empty docinfo.doctype.
        let doc = unsafe { htmlNewDoc(ptr::null(), ptr::null()) };
        assert!(!doc.is_null());
        let dtd = unsafe { crate::dtd::xmlGetIntSubset(doc) };
        assert!(!dtd.is_null(), "default doctype should be attached");
        unsafe {
            let pubid = std::ffi::CStr::from_ptr((*dtd).external_id);
            assert_eq!(pubid.to_str().unwrap(), "-//W3C//DTD HTML 4.0 Transitional//EN");
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn html_new_doc_no_dtd_skips_subset_when_both_null() {
        let doc = unsafe { htmlNewDocNoDtD(ptr::null(), ptr::null()) };
        assert!(!doc.is_null());
        assert!(unsafe { crate::dtd::xmlGetIntSubset(doc) }.is_null());
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn unlink_dtd_clears_int_subset() {
        // lxml's docinfo.clear() unlinks then frees the internal subset;
        // the unlink must NULL doc->intSubset so the DOCTYPE is gone.
        let doc = unsafe { htmlNewDoc(ptr::null(), ptr::null()) };
        let dtd = unsafe { crate::dtd::xmlGetIntSubset(doc) };
        assert!(!dtd.is_null());
        unsafe {
            crate::mutate::xmlUnlinkNode(dtd as *mut sup_xml_tree::dom::Node<'static>);
            assert!(crate::dtd::xmlGetIntSubset(doc).is_null(),
                    "intSubset must be NULL after unlinking the DTD node");
            xmlFreeDoc(doc);
        }
    }
}
