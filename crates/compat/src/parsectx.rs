//! libxml2 parser context API — the "reusable parser" pattern.
//!
//! In libxml2, `xmlParserCtxt` is a heavy struct that owns the
//! parser's state for repeated reuse across documents.  Real libxml2
//! exposes ~50 mutable fields (entity tables, encoding handler, SAX
//! handler, error list, etc.); we treat it as opaque and keep only
//! the bits that affect a parse: the option bitmask.
//!
//! lxml exercises this surface as `xmlNewParserCtxt() → xmlCtxtUseOptions
//! → xmlCtxtReadMemory → xmlFreeParserCtxt`.  Direct field access (e.g.
//! `ctxt->errNo`) isn't on the cards in v0.1; consumers that need that
//! get back an opaque pointer and will fail loudly if they dereference.
//!
//! Allocation lifecycle: `xmlNewParserCtxt` returns a `Box::into_raw`
//! pointer; `xmlFreeParserCtxt` reclaims it with `Box::from_raw`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::ptr;

use sup_xml_tree::dom::XmlDoc;

// Side-channel: per-thread map from a memory-parser ctxt's address to
// the byte buffer it was created with.
//
// libxml2's `xmlCreateMemoryParserCtxt(buf, size)` stashes the input
// buffer on the ctxt itself (via the `input` linked list).  We don't
// mirror that field layout — instead we own a copy of the buffer
// thread-locally and look it up by ctxt pointer when
// `xmlParseDocument(ctxt)` is invoked.  The lookup cost is O(1) and
// the storage is reclaimed by `xmlFreeParserCtxt`.
thread_local! {
    static MEMORY_SOURCES: RefCell<HashMap<usize, MemorySource>> =
        RefCell::new(HashMap::new());
}

struct MemorySource {
    bytes:   Vec<u8>,
    /// True if the ctxt was created by an HTML factory
    /// (`htmlCreateMemoryParserCtxt`).  Determines whether
    /// `xmlParseDocument` routes through the HTML or XML reader.
    is_html: bool,
}

/// Internal helper used by HTML / XML memory-ctxt factories to
/// stash the source buffer.  Same side-channel; the `is_html` flag
/// steers `xmlParseDocument` to the right parser at run time.
pub(crate) fn stash_memory_source(ctxt: *mut XmlParserCtxt, bytes: Vec<u8>, is_html: bool) {
    MEMORY_SOURCES.with(|m| {
        m.borrow_mut().insert(ctxt as usize, MemorySource { bytes, is_html });
    });
}

/// Opaque parser-context handle.  Sized to match libxml2's
/// `xmlParserCtxt` (752 bytes on 64-bit, libxml2 2.9.x) so consumer
/// code that reads `ctxt->wellFormed`, `ctxt->myDoc`, etc. doesn't
/// dereference past our allocation.  All fields are zero-initialized;
/// consumer reads return 0 / NULL which most call-sites treat as
/// "no document produced" → clean parse-error path.
///
/// We DO NOT mirror the field layout precisely yet — sax/userData/
/// myDoc would each need byte-exact offsets and that's a bigger
/// design exercise (deferred to a later slice).  For lxml's import
/// path, the size-only stub is sufficient.
#[repr(C, align(8))]
pub struct XmlParserCtxt {
    /// 752 bytes of zero-initialized storage — matches libxml2's
    /// `sizeof(xmlParserCtxt)` on 64-bit so any field read by a
    /// consumer lands inside our allocation.
    pub _opaque: [u8; 752],
}

/// libxml2's `_xmlSAXHandler` — 256 bytes of function pointers.  We
/// don't invoke any of these; their slots exist so that consumer
/// writes (`ctxt->sax->startDocument = ...` — lxml does this
/// immediately after `xmlNewParserCtxt`) land in writable memory
/// rather than segfaulting on a NULL `sax`.
#[repr(C, align(8))]
struct XmlSAXHandler {
    _slots: [u8; 256],
}

/// `xmlNewParserCtxt` — return a fresh context.  Caller releases via
/// [`xmlFreeParserCtxt`].
///
/// Allocates an attached `xmlSAXHandler` block; the context's first
/// pointer-sized slot (`sax`) is set to that block's address so
/// consumers like lxml can write into `ctxt->sax->startDocument`
/// without crashing.  Both are reclaimed by [`xmlFreeParserCtxt`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewParserCtxt() -> *mut XmlParserCtxt {
    let sax = Box::into_raw(Box::new(XmlSAXHandler { _slots: [0; 256] }));
    unsafe {
        crate::saxreplay::install_noop_sax_handlers(sax as *mut u8);
    }
    let mut ctx = XmlParserCtxt { _opaque: [0; 752] };
    // sax @ offset 0.
    let sax_bytes: [u8; std::mem::size_of::<usize>()] = (sax as usize).to_ne_bytes();
    ctx._opaque[..sax_bytes.len()].copy_from_slice(&sax_bytes);
    // Adopt the thread-local shared name dict and plant a fresh
    // reference at offset 456 (libxml2's `ctxt.dict` slot).  The
    // parser interns names through this when xmlCtxtReadMemory
    // runs — and because every parse on this thread (with or
    // without a ctxt) routes through the same dict, names interned
    // here are pointer-equal to names interned by docs created
    // elsewhere.  The ctxt's planted reference is released by
    // xmlFreeParserCtxt; the underlying dict survives until every
    // holder (other docs, the thread's own slot) drops its ref.
    let dict_ptr = crate::dict::thread_dict();
    // SAFETY: thread_dict returns a live, refcount-managed Dict.
    unsafe { (*dict_ptr).add_ref(); }
    let dict_bytes: [u8; std::mem::size_of::<usize>()] = (dict_ptr as usize).to_ne_bytes();
    ctx._opaque[CTXT_DICT_OFFSET..CTXT_DICT_OFFSET + dict_bytes.len()]
        .copy_from_slice(&dict_bytes);
    Box::into_raw(Box::new(ctx))
}

/// libxml2 `xmlNewSAXParserCtxt(sax, userData)` — create a parser
/// context whose SAX handler is the caller-supplied `sax` and whose
/// callback context is `userData`.  Parsing through this context (e.g.
/// [`xmlCtxtReadMemory`]) then fires the handler's **SAX2** callbacks
/// (`startElementNs` / `endElementNs` / `characters` / `cdataBlock` /
/// `comment` / `processingInstruction` / `startDocument` /
/// `endDocument`), synthesised from the parsed tree.
///
/// The caller's handler is *copied* into the context's owned SAX block
/// (`sizeof(xmlSAXHandler)` == 256), so the caller keeps ownership of
/// their struct and [`xmlFreeParserCtxt`] frees only our copy.  Returns
/// NULL on allocation failure.
///
/// Limitations: only SAX2 callbacks are delivered (a SAX1-only handler
/// receives no events); events are synthesised after a full parse
/// (not streamed); and the callback `ctx` argument is the parser
/// context (libxml2's value when `userData` is NULL).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewSAXParserCtxt(
    sax:       *const std::os::raw::c_void,
    user_data: *mut std::os::raw::c_void,
) -> *mut XmlParserCtxt {
    let ctxt = unsafe { xmlNewParserCtxt() };
    if ctxt.is_null() {
        return ctxt;
    }
    // Copy the caller's handler over our owned (no-op) SAX block at
    // offset 0.  The block pointer itself is unchanged, so xmlFreeParserCtxt
    // still frees our Box (never the caller's struct).
    if !sax.is_null() {
        let mut block_bits = [0u8; std::mem::size_of::<usize>()];
        unsafe {
            std::ptr::copy_nonoverlapping(ctxt as *const u8, block_bits.as_mut_ptr(), block_bits.len());
        }
        let block = usize::from_ne_bytes(block_bits) as *mut u8;
        if !block.is_null() {
            // SAFETY: sizeof(xmlSAXHandler) == 256 == our block size.
            unsafe { std::ptr::copy_nonoverlapping(sax as *const u8, block, 256); }
        }
    }
    unsafe {
        // userData @ offset 8.
        let p = (ctxt as *mut u8).add(CTXT_USERDATA_OFFSET);
        std::ptr::copy_nonoverlapping((user_data as usize).to_ne_bytes().as_ptr(), p,
                                      std::mem::size_of::<usize>());
        // Mark as a SAX parser context so xmlCtxtReadMemory replays events.
        *(ctxt as *mut u8).add(CTXT_SAX_PARSER_FLAG_OFFSET) = 1;
    }
    ctxt
}

/// libxml2 `xmlCreateMemoryParserCtxt(buffer, size)` — allocate a
/// parser context primed to read XML from `buffer`.
///
/// The buffer bytes are copied into a thread-local side channel keyed
/// by the ctxt pointer; a subsequent `xmlParseDocument(ctxt)` reads
/// them back and produces the document.  Releases via
/// `xmlFreeParserCtxt` (which clears the side-channel entry).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreateMemoryParserCtxt(
    buffer: *const c_char,
    size:   c_int,
) -> *mut XmlParserCtxt {
    if buffer.is_null() || size <= 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `buffer` is readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    let ctxt = unsafe { xmlNewParserCtxt() };
    if !ctxt.is_null() {
        stash_memory_source(ctxt, bytes.to_vec(), /*is_html=*/ false);
    }
    ctxt
}

/// libxml2 `xmlCreateFileParserCtxt(filename)` — read `filename` into
/// memory and return a parser context primed to parse it via
/// [`xmlParseDocument`].  Returns NULL on I/O or allocation failure.
///
/// Equivalent to [`xmlCreateMemoryParserCtxt`] preceded by
/// `std::fs::read(filename)`.  The ctxt owns the buffered bytes; they
/// are released alongside the ctxt by [`xmlFreeParserCtxt`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreateFileParserCtxt(
    filename: *const c_char,
) -> *mut XmlParserCtxt {
    if filename.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts NUL-terminated.
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(p)  => p,
        Err(_) => return ptr::null_mut(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b)  => b,
        Err(_) => return ptr::null_mut(),
    };
    let ctxt = unsafe { xmlNewParserCtxt() };
    if !ctxt.is_null() {
        stash_memory_source(ctxt, bytes, /*is_html=*/ false);
    }
    ctxt
}

/// Offset of `ctxt.dict` in libxml2's `_xmlParserCtxt` on 64-bit.
/// Verified against `parser.h` from the macOS 15.4 SDK; identical
/// across the recent libxml2 2.9 / 2.11 / 2.14 line.
const CTXT_DICT_OFFSET: usize = 456;

/// Offset where we stash the option bitmask passed to
/// [`xmlCtxtUseOptions`].  Past the `dict` slot and inside the
/// 752-byte ctxt buffer.  Real libxml2 has `options` at a layout-
/// dependent offset; consumers shouldn't be reading it directly
/// (they use `xmlCtxtUseOptions` / `xmlCtxtGetOptions`), so an
/// internal stash slot is sufficient for compat purposes.
pub(crate) const CTXT_OPTIONS_OFFSET: usize = 484;

/// Offset where we stash the consumer's `_private` user-data pointer
/// for [`xmlCtxtGetPrivate`] / [`xmlCtxtSetPrivate`].  424 is libxml2
/// 2.15.3's `xmlParserCtxt._private` offset, so a consumer reading the
/// field directly sees the same slot the functions use; the function
/// path is self-consistent regardless.  Clear of dict@456 / options@484.
pub(crate) const CTXT_PRIVATE_OFFSET: usize = 424;

/// Offset where we stash the `(resourceLoader, resourceCtxt)` pointer
/// pair registered via [`xmlCtxtSetResourceLoader`].  libxml2 2.15.3
/// puts these near the end of its 840-byte `xmlParserCtxt`, past our
/// 752-byte mirror, so we can't use the real offset; this free in-blob
/// slot keeps the function path self-consistent (consumers use the
/// setter, not a direct field read).  16 bytes; clear of options@484.
pub(crate) const CTXT_RESOURCE_LOADER_OFFSET: usize = 500;

/// Offset for the `(errorHandler, errorCtxt)` pair from
/// [`xmlCtxtSetErrorHandler`].  Free in-blob slot (real offset is past
/// our 752-byte mirror); 16 bytes, clear of resourceLoader@500..516.
pub(crate) const CTXT_ERROR_HANDLER_OFFSET: usize = 520;

/// Offset for the `maxAmplification` value from
/// [`xmlCtxtSetMaxAmplification`].  4 bytes.
pub(crate) const CTXT_MAX_AMPL_OFFSET: usize = 536;

/// Inline NUL-terminated buffer for the encoding name set via
/// [`xmlSwitchEncodingName`].  Stored inline (no heap / no leak) since
/// encoding labels are short; the ctxt-parse reads it as a C string.
pub(crate) const CTXT_SWITCH_ENC_OFFSET: usize = 540;
/// Capacity of the inline encoding-name buffer (incl. trailing NUL).
const CTXT_SWITCH_ENC_CAP: usize = 64;

/// libxml2 `xmlParserCtxt.userData` offset — the callback context the
/// SAX handlers receive.  Set by [`xmlNewSAXParserCtxt`].
pub(crate) const CTXT_USERDATA_OFFSET: usize = 8;

/// Flag byte (1 = yes) marking a context created by
/// [`xmlNewSAXParserCtxt`].  Only such contexts replay SAX events from
/// the ctxt-parse path, so contexts from [`xmlNewParserCtxt`] (e.g.
/// lxml's) keep their existing behaviour untouched.  Free in-blob slot.
const CTXT_SAX_PARSER_FLAG_OFFSET: usize = 604;

/// Offset for the `(convImpl, convCtxt)` pair registered via
/// [`xmlCtxtSetCharEncConvImpl`] — a custom character-encoding converter
/// factory and its user data.  16 bytes on a free in-blob slot, 8-aligned
/// past the saxParserFlag byte@604; clear of switchEnc@540..604.
const CTXT_CONV_IMPL_OFFSET: usize = 608;
const CTXT_CONV_VCTXT_OFFSET: usize = 616;

/// Offset for the `xmlSchemaValidCtxt*` a schema validator plugged onto
/// this context via `xmlSchemaSAXPlug`.  When set, [`xmlCtxtReadMemory`]
/// validates its result document against it at finish time.  8 bytes on a
/// free in-blob slot past convVctxt@616.
const CTXT_SCHEMA_VCTXT_OFFSET: usize = 624;

/// Store the plugged schema validator pointer on `ctxt` (NULL clears it).
pub(crate) unsafe fn set_ctxt_schema_validator(ctxt: *mut XmlParserCtxt, vctxt: *mut c_void) {
    if ctxt.is_null() { return; }
    let bits = (vctxt as usize).to_ne_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(
            bits.as_ptr(), (ctxt as *mut u8).add(CTXT_SCHEMA_VCTXT_OFFSET), 8);
    }
}

/// Read the plugged schema validator pointer (NULL if none).
pub(crate) unsafe fn read_ctxt_schema_validator(ctxt: *const XmlParserCtxt) -> *mut c_void {
    if ctxt.is_null() { return ptr::null_mut(); }
    let mut b = [0u8; 8];
    unsafe {
        std::ptr::copy_nonoverlapping(
            (ctxt as *const u8).add(CTXT_SCHEMA_VCTXT_OFFSET), b.as_mut_ptr(), 8);
    }
    usize::from_ne_bytes(b) as *mut c_void
}

/// Offset of `ctxt.lastError` (an embedded `xmlError`, 88 bytes) in
/// libxml2's `_xmlParserCtxt` on 64-bit — verified against the same
/// `parser.h` (macOS SDK) as `dict`/`myDoc`/`wellFormed` above.
///
/// Unlike the scratch slots, this is a real field consumers read
/// directly: lxml's `_raiseParseError` inspects
/// `ctxt->lastError.domain`, raising `OSError` when it equals
/// `XML_FROM_IO` and `XMLSyntaxError` otherwise.  Document inputs that
/// fail to load mirror their I/O error here via
/// [`mirror_last_error_into_ctxt`].
pub(crate) const CTXT_LASTERROR_OFFSET: usize = 600;

/// Copy the thread's most recent error (what `xmlGetLastError` returns)
/// into `ctxt`'s inline `lastError` field, so a consumer reading
/// `ctxt->lastError` directly sees it.  libxml2 keeps the thread-local
/// and per-context error in sync inside `__xmlRaiseError`; we mirror on
/// demand at the call sites that need it.
///
/// The copied `message`/`file` pointers alias the thread-local
/// last-error storage and remain valid until the next error is recorded
/// on this thread — the same lifetime [`crate::error::xmlGetLastError`]'s
/// result carries.  Consumers read them synchronously, before issuing
/// another parse on the thread.
pub(crate) unsafe fn mirror_last_error_into_ctxt(ctxt: *mut XmlParserCtxt) {
    if ctxt.is_null() { return; }
    let last = crate::error::xmlGetLastError();
    if last.is_null() { return; }
    // SAFETY: `last` points at a live 88-byte `xmlError`; the destination
    // lives inside the 752-byte ctxt blob (600 + 88 = 688 ≤ 752).
    unsafe {
        std::ptr::copy_nonoverlapping(
            last as *const u8,
            (ctxt as *mut u8).add(CTXT_LASTERROR_OFFSET),
            std::mem::size_of::<crate::error::xmlError>(),
        );
    }
}

/// Offset of `ctxt.myDoc` (xmlDocPtr).  libxml2 layout.
pub(crate) const CTXT_MYDOC_OFFSET: usize = 16;

/// Offset of `ctxt.wellFormed` (int).  libxml2 layout.  lxml's
/// feed-parser polls this between chunks, so we have to keep it
/// truthful as the push parser progresses.
pub(crate) const CTXT_WELLFORMED_OFFSET: usize = 24;

/// Plant `doc` into `ctxt.myDoc` so consumers reading the field
/// (lxml does, after `xmlParseChunk(terminate=1)`) see the parsed
/// document.
///
/// # Safety
///
/// `ctxt` must point at a live [`XmlParserCtxt`].
pub(crate) unsafe fn write_my_doc(ctxt: *mut XmlParserCtxt, doc: *mut XmlDoc) {
    let bytes = (doc as usize).to_ne_bytes();
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_MYDOC_OFFSET);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
}

/// Read `ctxt->myDoc` (offset 16).
///
/// # Safety
/// `ctxt` must point at a live [`XmlParserCtxt`].
pub(crate) unsafe fn read_my_doc(ctxt: *const XmlParserCtxt) -> *mut XmlDoc {
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_MYDOC_OFFSET);
        std::ptr::copy_nonoverlapping(p, bytes.as_mut_ptr(), bytes.len());
    }
    usize::from_ne_bytes(bytes) as *mut XmlDoc
}

/// Set `ctxt.wellFormed` to 1 (`true`) or 0 (`false`).  Sized as a
/// C `int` to match libxml2's field width.
///
/// # Safety
///
/// `ctxt` must point at a live [`XmlParserCtxt`].
pub(crate) unsafe fn set_well_formed(ctxt: *mut XmlParserCtxt, well_formed: bool) {
    let v: i32 = if well_formed { 1 } else { 0 };
    let bytes = v.to_ne_bytes();
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_WELLFORMED_OFFSET);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    }
}

/// `xmlFreeParserCtxt` — reclaim a context.  NULL-safe.  Also
/// reclaims the attached SAX handler allocated by
/// [`xmlNewParserCtxt`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeParserCtxt(ctxt: *mut XmlParserCtxt) {
    if ctxt.is_null() {
        return;
    }
    // Evict any push-state side-channel keyed by the ctxt address.
    crate::pushparse::forget_push_state(ctxt);
    // Same eviction for the memory-source side-channel set up by
    // xmlCreateMemoryParserCtxt; xmlParseDocument typically removes
    // its entry, but callers who construct + free a ctxt without
    // parsing still need cleanup.
    MEMORY_SOURCES.with(|m| { m.borrow_mut().remove(&(ctxt as usize)); });
    // SAFETY: caller asserts ctxt came from xmlNewParserCtxt.
    unsafe {
        let boxed = Box::from_raw(ctxt);
        const W: usize = std::mem::size_of::<usize>();

        // Release sax @ offset 0.
        let mut bytes = [0u8; W];
        bytes.copy_from_slice(&boxed._opaque[..W]);
        let sax_addr = usize::from_ne_bytes(bytes);
        if sax_addr != 0 {
            let _ = Box::from_raw(sax_addr as *mut XmlSAXHandler);
        }

        // Release dict @ offset CTXT_DICT_OFFSET.  Decrements the
        // refcount; the dict survives if other holders (docs that
        // were parsed through this ctxt) still hold references.
        let mut dbytes = [0u8; W];
        dbytes.copy_from_slice(&boxed._opaque[CTXT_DICT_OFFSET..CTXT_DICT_OFFSET + W]);
        let dict_addr = usize::from_ne_bytes(dbytes);
        if dict_addr != 0 {
            sup_xml_tree::dict::Dict::release(dict_addr as *mut sup_xml_tree::dict::Dict);
        }
        drop(boxed);
    }
}

/// `xmlCtxtUseOptions` — set the option bitmask on `ctxt`.  Returns
/// 0 on success, -1 on NULL ctxt.  Real libxml2 returns the bits it
/// didn't recognize; we accept everything (and store all of them).
///
/// The stored value is read by parse entry points that don't take
/// per-call options (the push parser via [`xmlParseChunk`]).  Entry
/// points that *do* take options as an explicit argument
/// ([`xmlCtxtReadMemory`]) follow libxml2 semantics: the per-call
/// argument replaces the ctxt's stored bitmask.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtUseOptions(
    ctxt:    *mut XmlParserCtxt,
    options: c_int,
) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    // SAFETY: ctxt is non-null; CTXT_OPTIONS_OFFSET + 4 ≤ 752.
    unsafe { write_ctxt_options(ctxt, options); }
    0
}

/// libxml2 `xmlCtxtSetOptions(ctxt, options)` — the modern name for
/// configuring parser options on a context; identical semantics to
/// [`xmlCtxtUseOptions`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetOptions(
    ctxt:    *mut XmlParserCtxt,
    options: c_int,
) -> c_int {
    unsafe { xmlCtxtUseOptions(ctxt, options) }
}

/// libxml2 `xmlCtxtGetOptions(ctxt)` — read back the option bitmask
/// stored on the context by [`xmlCtxtSetOptions`] / [`xmlCtxtUseOptions`].
/// Returns `-1` if `ctxt` is NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtGetOptions(ctxt: *mut XmlParserCtxt) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    unsafe { read_ctxt_options(ctxt) }
}

/// libxml2 `xmlCtxtReadFd(ctxt, fd, URL, encoding, options)` — parse a
/// document read in full from a file descriptor using a reusable
/// context.  Slurps the fd (without closing it) and routes through
/// [`xmlCtxtReadMemory`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtReadFd(
    ctxt:     *mut XmlParserCtxt,
    fd:       c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let Some(buf) = crate::parse::slurp_fd(fd) else { return std::ptr::null_mut() };
    unsafe {
        xmlCtxtReadMemory(ctxt, buf.as_ptr() as *const c_char, buf.len() as c_int, url, encoding, options)
    }
}

/// Read the option bitmask previously stored on `ctxt` by
/// [`xmlCtxtUseOptions`].  Returns `0` if nothing was set (the
/// zero-initialised default).
///
/// # Safety
///
/// `ctxt` must be non-null and point at a live [`XmlParserCtxt`]
/// allocation.
pub(crate) unsafe fn read_ctxt_options(ctxt: *const XmlParserCtxt) -> c_int {
    let mut bytes = [0u8; 4];
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_OPTIONS_OFFSET);
        std::ptr::copy_nonoverlapping(p, bytes.as_mut_ptr(), 4);
    }
    c_int::from_ne_bytes(bytes)
}

/// Detect lxml's `remove_comments` / `remove_pis`, which it signals by
/// NULLing the ctxt's SAX `comment` / `processingInstruction` callbacks
/// (parser.pxi).  Our engine builds the tree directly rather than
/// dispatching through SAX, so we read those slots to honour the same
/// intent.  Returns `(remove_comments, remove_pis)`; both `false` when
/// the ctxt or its SAX block is NULL.  SAX layout: `sax` at ctxt+0,
/// `processingInstruction` at +152, `comment` at +160 (see `saxreplay`).
pub(crate) unsafe fn read_ctxt_sax_remove_flags(ctxt: *const XmlParserCtxt) -> (bool, bool) {
    if ctxt.is_null() { return (false, false); }
    let read_ptr = |base: *const u8, off: usize| -> usize {
        let mut b = [0u8; std::mem::size_of::<usize>()];
        unsafe { std::ptr::copy_nonoverlapping(base.add(off), b.as_mut_ptr(), b.len()); }
        usize::from_ne_bytes(b)
    };
    let sax = read_ptr(ctxt as *const u8, 0);
    if sax == 0 { return (false, false); }
    let sax_b = sax as *const u8;
    (read_ptr(sax_b, 160) == 0, read_ptr(sax_b, 152) == 0)
}

/// Whether a consumer installed a custom `getEntity` SAX callback —
/// lxml sets `ctxt->sax->getEntity = _getInternalEntityOnly` when its
/// `resolve_entities` is `'internal'`/`False` (the default), signalling
/// that external *general* entities must NOT be loaded (a reference to
/// one is reported as undefined, matching libxml2).  Our ctxt creation
/// leaves the slot NULL, so any non-NULL value is that opt-out.
/// `getEntity` is at offset 40 within `xmlSAXHandler` (after
/// `internalSubset`, `isStandalone`, `hasInternalSubset`,
/// `hasExternalSubset`, `resolveEntity`).  Returns `false` when the ctxt
/// or its SAX block is NULL.
pub(crate) unsafe fn read_ctxt_sax_restricts_entities(ctxt: *const XmlParserCtxt) -> bool {
    if ctxt.is_null() { return false; }
    let read_ptr = |base: *const u8, off: usize| -> usize {
        let mut b = [0u8; std::mem::size_of::<usize>()];
        unsafe { std::ptr::copy_nonoverlapping(base.add(off), b.as_mut_ptr(), b.len()); }
        usize::from_ne_bytes(b)
    };
    let sax = read_ptr(ctxt as *const u8, 0);
    if sax == 0 { return false; }
    read_ptr(sax as *const u8, 40) != 0
}

/// Companion to [`read_ctxt_options`] — writes `options` at the
/// ctxt's stash offset.  Kept private to `parsectx`; callers go
/// through `xmlCtxtUseOptions`.
///
/// # Safety
///
/// `ctxt` must be non-null and point at a live [`XmlParserCtxt`]
/// allocation.
unsafe fn write_ctxt_options(ctxt: *mut XmlParserCtxt, options: c_int) {
    let bytes = options.to_ne_bytes();
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_OPTIONS_OFFSET);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, 4);
    }
}

/// libxml2 `xmlCtxtSetPrivate(ctxt, priv)` — store an opaque user-data
/// pointer on the context.  libxml2 never touches `_private`; it exists
/// purely for the consumer.  No-op on a NULL `ctxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetPrivate(
    ctxt:  *mut XmlParserCtxt,
    priv_: *mut std::os::raw::c_void,
) {
    if ctxt.is_null() {
        return;
    }
    let bytes = (priv_ as usize).to_ne_bytes();
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_PRIVATE_OFFSET);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, std::mem::size_of::<usize>());
    }
}

/// libxml2 `xmlCtxtGetPrivate(ctxt)` — read back the pointer stored by
/// [`xmlCtxtSetPrivate`].  Returns NULL on a NULL `ctxt` or if unset.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtGetPrivate(
    ctxt: *mut XmlParserCtxt,
) -> *mut std::os::raw::c_void {
    if ctxt.is_null() {
        return std::ptr::null_mut();
    }
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_PRIVATE_OFFSET);
        std::ptr::copy_nonoverlapping(p, bytes.as_mut_ptr(), bytes.len());
    }
    usize::from_ne_bytes(bytes) as *mut std::os::raw::c_void
}

/// libxml2 `xmlCtxtSetResourceLoader(ctxt, loader, vctxt)` — register a
/// callback the parser invokes to load external resources (external
/// DTDs / parsed entities).  The pair is stored on the context; the
/// context-parse path ([`xmlCtxtReadMemory`]) bridges it to the
/// engine's entity resolver.  Registering a loader is the opt-in that
/// enables external-resource loading.  No-op on a NULL `ctxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetResourceLoader(
    ctxt:   *mut XmlParserCtxt,
    loader: Option<crate::parse::XmlResourceLoader>,
    vctxt:  *mut std::os::raw::c_void,
) {
    if ctxt.is_null() {
        return;
    }
    let loader_bits = loader.map_or(0usize, |f| f as usize);
    let vctxt_bits  = vctxt as usize;
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_RESOURCE_LOADER_OFFSET);
        std::ptr::copy_nonoverlapping(loader_bits.to_ne_bytes().as_ptr(), p, 8);
        std::ptr::copy_nonoverlapping(vctxt_bits.to_ne_bytes().as_ptr(), p.add(8), 8);
    }
}

/// Read back the `(loader, vctxt)` pair registered on `ctxt`.  Returns
/// `None` when none is set.
unsafe fn read_ctxt_resource_loader(
    ctxt: *const XmlParserCtxt,
) -> Option<(crate::parse::XmlResourceLoader, usize)> {
    if ctxt.is_null() {
        return None;
    }
    let (mut lb, mut vb) = ([0u8; 8], [0u8; 8]);
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_RESOURCE_LOADER_OFFSET);
        std::ptr::copy_nonoverlapping(p, lb.as_mut_ptr(), 8);
        std::ptr::copy_nonoverlapping(p.add(8), vb.as_mut_ptr(), 8);
    }
    let loader_bits = usize::from_ne_bytes(lb);
    if loader_bits == 0 {
        return None;
    }
    // SAFETY: loader_bits round-trips a real XmlResourceLoader fn
    // pointer stored by xmlCtxtSetResourceLoader (same width).
    let loader: crate::parse::XmlResourceLoader =
        unsafe { std::mem::transmute::<usize, crate::parse::XmlResourceLoader>(loader_bits) };
    Some((loader, usize::from_ne_bytes(vb)))
}

/// libxml2 `xmlCtxtSetErrorHandler(ctxt, handler, data)` — register a
/// structured error callback on the context.  Stored here and invoked
/// by [`xmlCtxtReadMemory`] with the parse's error when parsing fails.
/// A NULL `handler` clears it; no-op on a NULL `ctxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetErrorHandler(
    ctxt:    *mut XmlParserCtxt,
    handler: Option<crate::error::StructuredErrorFn>,
    data:    *mut std::os::raw::c_void,
) {
    if ctxt.is_null() {
        return;
    }
    let h_bits = handler.map_or(0usize, |f| f as usize);
    let d_bits = if handler.is_some() { data as usize } else { 0 };
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_ERROR_HANDLER_OFFSET);
        std::ptr::copy_nonoverlapping(h_bits.to_ne_bytes().as_ptr(), p, 8);
        std::ptr::copy_nonoverlapping(d_bits.to_ne_bytes().as_ptr(), p.add(8), 8);
    }
}

/// Read back the `(handler, data)` pair registered on `ctxt`.
unsafe fn read_ctxt_error_handler(
    ctxt: *const XmlParserCtxt,
) -> Option<(crate::error::StructuredErrorFn, *mut std::os::raw::c_void)> {
    if ctxt.is_null() {
        return None;
    }
    let (mut hb, mut db) = ([0u8; 8], [0u8; 8]);
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_ERROR_HANDLER_OFFSET);
        std::ptr::copy_nonoverlapping(p, hb.as_mut_ptr(), 8);
        std::ptr::copy_nonoverlapping(p.add(8), db.as_mut_ptr(), 8);
    }
    let h = usize::from_ne_bytes(hb);
    if h == 0 {
        return None;
    }
    // SAFETY: round-trips a real StructuredErrorFn fn pointer stored by
    // xmlCtxtSetErrorHandler (same width).
    let handler: crate::error::StructuredErrorFn =
        unsafe { std::mem::transmute::<usize, crate::error::StructuredErrorFn>(h) };
    Some((handler, usize::from_ne_bytes(db) as *mut std::os::raw::c_void))
}

/// Deliver a structured error to whatever handler `ctxt` has installed —
/// the newer `xmlCtxtSetErrorHandler` slot if present, else the SAX2
/// `ctxt->sax->serror` slot lxml uses.  Consumers accumulate the error in
/// their log (lxml's `error_log`), which `_handleParseResult` then
/// inspects to decide whether to raise when `recover` is off.  No-op when
/// `ctxt` is NULL or no handler is installed.
///
/// # Safety
/// `ctxt` is NULL or a live [`XmlParserCtxt`]; `err` points to a valid
/// [`xmlError`] for the duration of the call.
pub(crate) unsafe fn deliver_ctxt_error(
    ctxt: *mut XmlParserCtxt,
    err:  *const crate::error::xmlError,
) {
    if ctxt.is_null() || err.is_null() {
        return;
    }
    if let Some((handler, data)) = unsafe { read_ctxt_error_handler(ctxt) } {
        unsafe { handler(data, err); }
    } else if let Some(serror) = unsafe { read_ctxt_sax_serror(ctxt) } {
        unsafe { serror(ctxt as *mut std::os::raw::c_void, err); }
    }
}

/// `serror` offset within `xmlSAXHandler` — the structured error
/// callback slot (after the 27 SAX1 function pointers, `initialized`
/// + padding, `_private`, `startElementNs`, `endElementNs`).  This is
/// where consumers built against pre-2.13 libxml2 register their error
/// handler (lxml sets `ctxt->sax->serror = _receiveParserError`), as
/// opposed to the newer `xmlCtxtSetErrorHandler` slot.
const SAX_SERROR_OFFSET: usize = 248;

/// Read `ctxt->sax->serror`, the SAX2 structured error handler.  libxml2
/// invokes it with the parser context itself as user data (lxml's
/// `_receiveParserError` recovers its `_ParserContext` from
/// `ctxt->_private`).  Returns `None` when no SAX handler or no serror
/// is installed.
unsafe fn read_ctxt_sax_serror(
    ctxt: *const XmlParserCtxt,
) -> Option<crate::error::StructuredErrorFn> {
    if ctxt.is_null() {
        return None;
    }
    // `sax` is the first field of xmlParserCtxt (offset 0).
    let mut saxb = [0u8; 8];
    unsafe { std::ptr::copy_nonoverlapping(ctxt as *const u8, saxb.as_mut_ptr(), 8); }
    let sax = usize::from_ne_bytes(saxb);
    if sax == 0 {
        return None;
    }
    let mut hb = [0u8; 8];
    unsafe {
        std::ptr::copy_nonoverlapping(
            (sax as *const u8).add(SAX_SERROR_OFFSET), hb.as_mut_ptr(), 8);
    }
    let h = usize::from_ne_bytes(hb);
    if h == 0 {
        return None;
    }
    // SAFETY: round-trips a real xmlStructuredErrorFunc pointer.
    Some(unsafe { std::mem::transmute::<usize, crate::error::StructuredErrorFn>(h) })
}

/// libxml2 `xmlCtxtSetMaxAmplification(ctxt, maxAmpl)` — limit entity
/// expansion to roughly `maxAmpl ×` the input size.  Stored here and
/// applied by [`xmlCtxtReadMemory`] as the parse's entity-expansion
/// byte cap.  No-op on a NULL `ctxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetMaxAmplification(
    ctxt:     *mut XmlParserCtxt,
    max_ampl: std::os::raw::c_uint,
) {
    if ctxt.is_null() {
        return;
    }
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_MAX_AMPL_OFFSET);
        std::ptr::copy_nonoverlapping(max_ampl.to_ne_bytes().as_ptr(), p, 4);
    }
}

/// Read back the amplification factor registered on `ctxt` (0 / unset → None).
unsafe fn read_ctxt_max_ampl(ctxt: *const XmlParserCtxt) -> Option<u32> {
    if ctxt.is_null() {
        return None;
    }
    let mut b = [0u8; 4];
    unsafe {
        let p = (ctxt as *const u8).add(CTXT_MAX_AMPL_OFFSET);
        std::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), 4);
    }
    let v = u32::from_ne_bytes(b);
    if v == 0 { None } else { Some(v) }
}

/// libxml2 `xmlSwitchEncodingName(ctxt, encoding)` — set the input
/// encoding by name on the context, overriding auto-detection for the
/// next parse.  The name is stored inline on the context and applied
/// by [`xmlCtxtReadMemory`].  Returns 0 on success; -1 on a NULL ctxt
/// or a name too long for the inline buffer.  A NULL `encoding` clears
/// the override.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSwitchEncodingName(
    ctxt:     *mut XmlParserCtxt,
    encoding: *const c_char,
) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    let p = unsafe { (ctxt as *mut u8).add(CTXT_SWITCH_ENC_OFFSET) };
    if encoding.is_null() {
        unsafe { *p = 0; } // clear
        return 0;
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(encoding) }.to_bytes();
    if bytes.len() >= CTXT_SWITCH_ENC_CAP {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0; // NUL-terminate
    }
    0
}

/// Pointer to the inline encoding name set by [`xmlSwitchEncodingName`],
/// or NULL if none is set (empty buffer).
unsafe fn read_ctxt_switch_enc(ctxt: *const XmlParserCtxt) -> *const c_char {
    if ctxt.is_null() {
        return std::ptr::null();
    }
    let p = unsafe { (ctxt as *const u8).add(CTXT_SWITCH_ENC_OFFSET) };
    if unsafe { *p } == 0 {
        std::ptr::null()
    } else {
        p as *const c_char
    }
}

/// A custom character-encoding converter factory (libxml2 2.14+
/// `xmlCharEncConvImpl`).  Given an encoding `name`, it fills `*out` with
/// an [`xmlCharEncodingHandler`](crate::outbuf::xmlCharEncodingHandler)
/// carrying the conversion functions and returns `XML_ERR_OK` (0); a
/// non-zero return means it declines that encoding, and the parser falls
/// back to its built-in transcoders.  `flags` is a bit mask of
/// [`XML_ENC_INPUT`] / `XML_ENC_OUTPUT` / `XML_ENC_HTML`.
pub type XmlCharEncConvImpl = unsafe extern "C" fn(
    vctxt: *mut c_void,
    name:  *const c_char,
    flags: c_int,
    out:   *mut *mut crate::outbuf::xmlCharEncodingHandler,
) -> c_int;

/// `xmlCharEncFlags` — the converter is wanted for the input (decode)
/// direction.
pub const XML_ENC_INPUT: c_int = 1 << 0;

/// libxml2 `xmlCtxtSetCharEncConvImpl(ctxt, impl, vctxt)` — register a
/// custom character-encoding converter factory on the context.  This is
/// the thread-safe, per-context way to plug in an external transcoder
/// (e.g. ICU) instead of mutating global encoding-alias state.
///
/// When the next ctxt-parse needs to transcode input whose encoding is
/// named in the `<?xml encoding="…"?>` declaration (or supplied
/// explicitly), [`xmlCtxtReadMemory`] calls `impl(vctxt, name, …, &out)`
/// to obtain a handler and drives its conversion function to produce
/// UTF-8.  If `impl` declines the encoding (non-zero return), the built-in
/// transcoders handle it.  A NULL `impl` clears the registration; no-op on
/// a NULL `ctxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtSetCharEncConvImpl(
    ctxt:  *mut XmlParserCtxt,
    imp:   Option<XmlCharEncConvImpl>,
    vctxt: *mut c_void,
) {
    if ctxt.is_null() {
        return;
    }
    let i_bits = imp.map_or(0usize, |f| f as usize);
    let v_bits = if imp.is_some() { vctxt as usize } else { 0 };
    unsafe {
        let p = (ctxt as *mut u8).add(CTXT_CONV_IMPL_OFFSET);
        std::ptr::copy_nonoverlapping(i_bits.to_ne_bytes().as_ptr(), p, 8);
        let q = (ctxt as *mut u8).add(CTXT_CONV_VCTXT_OFFSET);
        std::ptr::copy_nonoverlapping(v_bits.to_ne_bytes().as_ptr(), q, 8);
    }
}

/// Read back the `(impl, vctxt)` pair registered on `ctxt` (unset → None).
unsafe fn read_ctxt_conv_impl(
    ctxt: *const XmlParserCtxt,
) -> Option<(XmlCharEncConvImpl, *mut c_void)> {
    if ctxt.is_null() {
        return None;
    }
    let (mut ib, mut vb) = ([0u8; 8], [0u8; 8]);
    unsafe {
        std::ptr::copy_nonoverlapping(
            (ctxt as *const u8).add(CTXT_CONV_IMPL_OFFSET), ib.as_mut_ptr(), 8);
        std::ptr::copy_nonoverlapping(
            (ctxt as *const u8).add(CTXT_CONV_VCTXT_OFFSET), vb.as_mut_ptr(), 8);
    }
    let i = usize::from_ne_bytes(ib);
    if i == 0 {
        return None;
    }
    // SAFETY: round-trips a real XmlCharEncConvImpl fn pointer stored by
    // xmlCtxtSetCharEncConvImpl (same width).
    let imp: XmlCharEncConvImpl =
        unsafe { std::mem::transmute::<usize, XmlCharEncConvImpl>(i) };
    Some((imp, usize::from_ne_bytes(vb) as *mut c_void))
}

/// Transcode `raw` to UTF-8 using a consumer-registered converter factory.
///
/// Resolves the encoding name to hand the factory — the explicit `encoding`
/// argument if given, otherwise the name in the document's XML declaration —
/// then asks the factory for a handler and drives its input conversion
/// function until all input is consumed.  Returns the UTF-8 bytes on
/// success, or `None` when there is no name to resolve, the factory declines
/// the encoding, or the conversion errors — in every `None` case the caller
/// falls back to the built-in transcoders, preserving existing behaviour.
unsafe fn drive_custom_transcode(
    imp:      XmlCharEncConvImpl,
    vctxt:    *mut c_void,
    encoding: *const c_char,
    buffer:   *const c_char,
    size:     c_int,
) -> Option<Vec<u8>> {
    if buffer.is_null() || size <= 0 {
        return None;
    }
    // SAFETY: caller asserts buffer is valid for `size` bytes.
    let raw = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };

    // Resolve the encoding name to hand the factory.
    let sniffed;
    let name_ptr: *const c_char = if !encoding.is_null() {
        encoding
    } else {
        sniffed = sup_xml_core::encoding::declared_encoding_name(raw)
            .and_then(|n| std::ffi::CString::new(n).ok())?;
        sniffed.as_ptr()
    };

    // Ask the factory for a handler; a non-zero return means "decline".
    let mut handler: *mut crate::outbuf::xmlCharEncodingHandler = std::ptr::null_mut();
    let rc = unsafe { imp(vctxt, name_ptr, XML_ENC_INPUT, &mut handler) };
    if rc != 0 || handler.is_null() {
        if !handler.is_null() {
            unsafe { crate::outbuf::xmlCharEncCloseFunc(handler); }
        }
        return None;
    }

    let func = unsafe { (*handler).input };
    let in_ctxt = unsafe { (*handler).input_ctxt };
    let result = func.and_then(|f| unsafe { run_conv_loop(f, in_ctxt, raw) });

    // Free the handler (invoking its ctxtDtor) whether or not it converted.
    unsafe { crate::outbuf::xmlCharEncCloseFunc(handler); }
    result
}

/// Drive a single converter's input function over `raw`, growing the output
/// buffer on `XML_ENC_ERR_SPACE` and stopping once all input is consumed.
/// Returns `None` on a converter error or if it stalls without progress.
unsafe fn run_conv_loop(
    func:    crate::outbuf::XmlCharEncConvFunc,
    in_ctxt: *mut c_void,
    raw:     &[u8],
) -> Option<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(raw.len().saturating_mul(2).max(64));
    let mut consumed = 0usize;
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(64));
        }
        let spare = out.capacity() - out.len();
        let mut outlen = spare.min(c_int::MAX as usize) as c_int;
        let mut inlen = (raw.len() - consumed).min(c_int::MAX as usize) as c_int;
        // SAFETY: in_ptr/out_ptr are valid for inlen/outlen bytes; the
        // converter writes at most `outlen` bytes and reports what it did.
        let err = unsafe {
            func(
                in_ctxt,
                out.as_mut_ptr().add(out.len()),
                &mut outlen,
                raw.as_ptr().add(consumed),
                &mut inlen,
                1, // flush: all input is present in this single buffer.
            )
        };
        // `inlen`/`outlen` carry bytes consumed/produced on every return,
        // including the partial work done before XML_ENC_ERR_SPACE.
        consumed += inlen as usize;
        // SAFETY: the converter initialised `outlen` bytes past the old len.
        unsafe { out.set_len(out.len() + outlen as usize); }

        if err == 0 {
            if consumed >= raw.len() {
                return Some(out);
            }
        } else if err == crate::outbuf::XML_ENC_ERR_SPACE {
            out.reserve(out.capacity().max(64));
        } else {
            return None; // input / internal / memory error → fall back.
        }
        if inlen == 0 && outlen == 0 {
            return None; // no forward progress → avoid spinning.
        }
    }
}

/// `xmlCtxtReadMemory(ctxt, buffer, size, url, encoding, options)` —
/// parse using a reusable context.  Currently a thin wrapper over
/// [`crate::parse::xmlReadMemory`]; the `options` arg replaces
/// whatever was set via [`xmlCtxtUseOptions`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtReadMemory(
    ctxt:     *mut XmlParserCtxt,
    buffer:   *const c_char,
    size:     c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    // Read ctxt.dict (offset 456).  If non-null we route the parse
    // through that dict so the resulting tree's names share its
    // canonical pointers — keeping pointer-equality stable across
    // multiple parses with the same ctxt and matching consumers'
    // expectations that `doc.dict == ctxt.dict`.
    let dict_ptr: *mut sup_xml_tree::dict::Dict = if ctxt.is_null() {
        crate::dict::thread_dict()
    } else {
        let p = unsafe { (ctxt as *const u8).add(CTXT_DICT_OFFSET) };
        let mut b = [0u8; std::mem::size_of::<usize>()];
        unsafe { std::ptr::copy_nonoverlapping(p, b.as_mut_ptr(), b.len()); }
        let raw = usize::from_ne_bytes(b) as *mut sup_xml_tree::dict::Dict;
        if raw.is_null() { crate::dict::thread_dict() } else { raw }
    };
    // Bridge a registered resource loader (xmlCtxtSetResourceLoader)
    // into the engine's entity resolver, so external DTDs / entities
    // referenced by this parse are loaded through the consumer's callback.
    let resolver: Option<std::sync::Arc<dyn sup_xml_core::entity_resolver::EntityResolver>> =
        unsafe { read_ctxt_resource_loader(ctxt) }.map(|(loader, vctxt)| {
            std::sync::Arc::new(crate::parse::CResourceLoaderResolver { loader, vctxt })
                as std::sync::Arc<dyn sup_xml_core::entity_resolver::EntityResolver>
        })
        // Fall back to the global external-entity loader (the hook lxml's
        // `parser.resolvers` registers via xmlSetExternalEntityLoader).  It
        // needs the parse context to find the consumer's resolvers, so pass
        // `ctxt` through.
        .or_else(|| {
            crate::parse::consumer_external_entity_loader().map(|loader| {
                std::sync::Arc::new(crate::parse::CExternalEntityLoaderResolver {
                    loader, ctxt: ctxt as usize,
                }) as std::sync::Arc<dyn sup_xml_core::entity_resolver::EntityResolver>
            })
        });
    // Amplification → absolute entity-expansion cap (maxAmpl × input size).
    let max_entity_expansion_bytes = unsafe { read_ctxt_max_ampl(ctxt) }
        .map(|a| (size.max(0) as u64).saturating_mul(a as u64));
    let (remove_comments, remove_pis) = unsafe { read_ctxt_sax_remove_flags(ctxt) };
    let restrict_external_entities = unsafe { read_ctxt_sax_restricts_entities(ctxt) };
    let extras = crate::parse::CtxtParseExtras {
        resolver, max_entity_expansion_bytes, remove_comments, remove_pis,
        restrict_external_entities,
    };
    // Effective encoding: the explicit argument wins; otherwise honour a
    // name set via xmlSwitchEncodingName.
    let eff_encoding = if encoding.is_null() {
        unsafe { read_ctxt_switch_enc(ctxt) }
    } else {
        encoding
    };
    // A custom converter registered via xmlCtxtSetCharEncConvImpl gets first
    // refusal on transcoding the raw input.  When it produces UTF-8 we parse
    // that directly, forcing UTF-8 so the engine doesn't transcode again;
    // otherwise we hand the original bytes to the built-in path unchanged.
    let custom_utf8 = unsafe { read_ctxt_conv_impl(ctxt) }
        .and_then(|(imp, vctxt)| unsafe {
            drive_custom_transcode(imp, vctxt, eff_encoding, buffer, size)
        });
    const UTF8_NAME: &[u8] = b"UTF-8\0";
    let (parse_buf, parse_len, parse_enc) = match custom_utf8 {
        Some(ref u) => (
            u.as_ptr() as *const c_char,
            u.len() as c_int,
            UTF8_NAME.as_ptr() as *const c_char,
        ),
        None => (buffer, size, eff_encoding),
    };
    // SAFETY: caller asserts buffer is valid; dict is refcount-managed.
    let doc = unsafe {
        crate::parse::xml_read_memory_with_dict_extras(
            parse_buf, parse_len, url, parse_enc, options, dict_ptr, extras,
        )
    };
    // Deliver the parse failure to the context's structured error
    // handler so consumers accumulate it in their error log (lxml's
    // `error_log`).  Newer consumers register via
    // `xmlCtxtSetErrorHandler`; lxml (built against pre-2.13 libxml2)
    // installs `ctxt->sax->serror` instead, invoked with the context
    // as user data.
    // Reflect the parse outcome on the context.  lxml's target parser
    // (`XMLParser(target=…)`, also the engine behind `etree.canonicalize`)
    // rejects the result unless `ctxt->wellFormed` is set after a clean
    // parse; the ordinary tree-building path tolerates it being unset,
    // so this only ever helps.
    if !ctxt.is_null() {
        unsafe { set_well_formed(ctxt, !doc.is_null()); }
    }
    if doc.is_null() {
        let err = crate::error::xmlGetLastError();
        unsafe { deliver_ctxt_error(ctxt, err); }
    } else if !ctxt.is_null() {
        // A normal (non-event) parse builds the tree natively and never
        // drives `ctxt->sax`, so a consumer's `startDocument` callback
        // would otherwise never run.  lxml relies on it: `_initSaxDocument`
        // creates `doc->ids` there when `collect_ids` is on.  Fire it once
        // — a target/iterparse parse instead gets `startDocument` from
        // `replay`, so gate on `has_event_handlers` to avoid double-firing.
        // The callback reads `ctxt->myDoc`, so plant the result for the
        // call and restore it afterward (libxml2 leaves `myDoc` NULL once
        // a Read returns).
        if unsafe { !crate::saxreplay::has_event_handlers(ctxt) } {
            let prev = unsafe { read_my_doc(ctxt) };
            unsafe { write_my_doc(ctxt, doc); }
            unsafe { crate::saxreplay::fire_start_document(ctxt); }
            unsafe { write_my_doc(ctxt, prev); }
        }
        // Synthesise SAX2 callbacks from the parsed tree for any custom
        // handlers the consumer installed on `ctxt->sax` — lxml's
        // iterparse (xmlNewSAXParserCtxt) and `XMLParser(target=…)`
        // (xmlNewParserCtxt with the target's handlers).  `replay` is a
        // no-op when only the no-op baseline handlers are present, so an
        // ordinary tree-building parse is untouched.
        unsafe { crate::saxreplay::replay(ctxt, doc, false); }
        // Fill the `doc->ids` table that `startDocument` (lxml's
        // `_initSaxDocument`) created; a no-op when it wasn't (e.g.
        // `collect_ids=False`, which leaves `doc->ids` NULL).
        crate::idindex::populate_doc_id_table(doc);
    }
    // If a schema validator was plugged onto this context
    // (xmlSchemaSAXPlug, as the schema-validating XMLParser does),
    // validate the freshly-parsed document against it now — before the
    // caller takes ownership and clears myDoc.  The verdict is cached on
    // the validator for xmlSchemaIsValid.
    if !doc.is_null() {
        let v = unsafe { read_ctxt_schema_validator(ctxt) };
        if !v.is_null() {
            unsafe { crate::xsd::validate_plugged(v, doc); }
        }
    }
    doc
}

/// libxml2 `xmlParseDocument(ctxt)` — run the parse using the input
/// previously stashed on the ctxt by [`xmlCreateMemoryParserCtxt`].
///
/// Returns 0 on success, -1 on a NULL ctxt or when the ctxt has no
/// registered memory source (e.g. callers who built it via
/// [`xmlNewParserCtxt`] without supplying input).  On success, the
/// resulting document is planted into `ctxt->myDoc` and
/// `ctxt->wellFormed` is set per the parse outcome.
///
/// XML::LibXML's `load_xml(string => ...)` is the canonical consumer
/// of this two-step flow.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseDocument(ctxt: *mut XmlParserCtxt) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    // Take ownership of the stashed buffer.  Doing this here (rather
    // than just borrowing) means the bytes are reclaimed even if the
    // caller forgets to call xmlFreeParserCtxt; a fresh call to
    // xmlCreateMemoryParserCtxt is the only path that repopulates.
    let src = MEMORY_SOURCES.with(|m| m.borrow_mut().remove(&(ctxt as usize)));
    let Some(src) = src else { return -1; };

    // Route through the right parser based on which factory created
    // the ctxt.  HTML ctxts (from `htmlCreateMemoryParserCtxt`) go
    // through `htmlReadMemory`; XML ctxts (from
    // `xmlCreateMemoryParserCtxt`) go through `xmlCtxtReadMemory`,
    // which honours the ctxt's options + dict.
    let doc = if src.is_html {
        unsafe {
            crate::html::htmlReadMemory(
                src.bytes.as_ptr() as *const c_char,
                src.bytes.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        }
    } else {
        unsafe {
            xmlCtxtReadMemory(
                ctxt,
                src.bytes.as_ptr() as *const c_char,
                src.bytes.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        }
    };

    unsafe {
        write_my_doc(ctxt, doc);
        set_well_formed(ctxt, !doc.is_null());
    }

    if doc.is_null() { -1 } else { 0 }
}

/// `xmlClearParserCtxt` — reset the context's mutable parsing state
/// while preserving the application-data fields libxml2's
/// `xmlCtxtReset` keeps across a reuse.
///
/// Specifically preserves:
///   * `sax` (offset 0) — the installed SAX handler; consumers (lxml's
///     `_ParserContext.cleanup`) write to `sax->serror` after a clear.
///   * `_private` (offset 424) — application data.  lxml connects its
///     `_ParserContext` here ONCE (`_initParserContext`) and reuses the
///     ctxt across parses; zeroing it would orphan the context so that
///     `startDocument` (`_initSaxDocument`) could no longer build
///     `doc->ids`.
///
/// Other configuration (dict, options, resolvers, error handler) is
/// re-applied by the consumer on each parse, so resetting it is benign.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlClearParserCtxt(ctxt: *mut XmlParserCtxt) {
    if ctxt.is_null() {
        return;
    }
    // SAFETY: ctxt is non-null per the check.
    unsafe {
        // Save sax (offset 0) and _private, zero everything, restore.
        // Avoid implicit autoref off the raw pointer by going through
        // `&raw mut`/`&raw const`.
        const W: usize = std::mem::size_of::<usize>();
        let opaque: *mut [u8; 752] = &raw mut (*ctxt)._opaque;
        let base = opaque as *mut u8;
        let mut saved_sax: [u8; W] = [0; W];
        let mut saved_private: [u8; W] = [0; W];
        std::ptr::copy_nonoverlapping(base, saved_sax.as_mut_ptr(), W);
        std::ptr::copy_nonoverlapping(
            base.add(CTXT_PRIVATE_OFFSET), saved_private.as_mut_ptr(), W);
        std::ptr::write_bytes(base, 0, 752);
        std::ptr::copy_nonoverlapping(saved_sax.as_ptr(), base, W);
        std::ptr::copy_nonoverlapping(
            saved_private.as_ptr(), base.add(CTXT_PRIVATE_OFFSET), W);
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{CStr, CString};

    // ── xmlCtxtSetCharEncConvImpl → custom transcoder ──────────────────
    //
    // A toy converter for the made-up encoding "x-toytest": it copies bytes
    // through to UTF-8 verbatim except that each '@' becomes "XY".  That
    // transform is something no built-in transcoder performs, so observing it
    // in the parsed tree proves the consumer's converter actually drove the
    // bytes.

    unsafe extern "C" fn toy_input(
        _vctxt: *mut c_void,
        out:    *mut u8,
        outlen: *mut c_int,
        input:  *const u8,
        inlen:  *mut c_int,
        _flush: c_int,
    ) -> c_int {
        let src = unsafe { std::slice::from_raw_parts(input, *inlen as usize) };
        let dst = unsafe { std::slice::from_raw_parts_mut(out, *outlen as usize) };
        let (mut i, mut o) = (0usize, 0usize);
        while i < src.len() {
            if src[i] == b'@' {
                if o + 2 > dst.len() { break; }
                dst[o] = b'X';
                dst[o + 1] = b'Y';
                o += 2;
            } else {
                if o + 1 > dst.len() { break; }
                dst[o] = src[i];
                o += 1;
            }
            i += 1;
        }
        unsafe {
            *inlen = i as c_int;
            *outlen = o as c_int;
        }
        if i < src.len() { crate::outbuf::XML_ENC_ERR_SPACE } else { 0 }
    }

    unsafe extern "C" fn toy_factory(
        _vctxt: *mut c_void,
        name:   *const c_char,
        _flags: c_int,
        out:    *mut *mut crate::outbuf::xmlCharEncodingHandler,
    ) -> c_int {
        let n = unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("");
        if n != "x-toytest" {
            unsafe { *out = std::ptr::null_mut(); }
            return 32; // decline anything but our toy encoding.
        }
        unsafe {
            crate::outbuf::xmlCharEncNewCustomHandler(
                name,
                Some(toy_input),
                None,
                None,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                out,
            )
        }
    }

    fn root_text(doc: *mut XmlDoc) -> String {
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let cp = unsafe { crate::parse::xmlNodeGetContent(root) };
        if cp.is_null() {
            return String::new();
        }
        let s = unsafe { CStr::from_ptr(cp) }.to_str().unwrap_or("").to_string();
        unsafe { crate::parse::xmlFree(cp as *mut c_void); }
        s
    }

    #[test]
    fn char_enc_new_custom_handler_fills_struct() {
        let name = CString::new("x-toytest").unwrap();
        let marker = 0x1234_usize as *mut c_void;
        let mut h: *mut crate::outbuf::xmlCharEncodingHandler = std::ptr::null_mut();
        let rc = unsafe {
            crate::outbuf::xmlCharEncNewCustomHandler(
                name.as_ptr(), Some(toy_input), None, None,
                marker, std::ptr::null_mut(), &mut h,
            )
        };
        assert_eq!(rc, 0);
        assert!(!h.is_null());
        unsafe {
            assert!((*h).input.is_some(), "input converter should be set");
            assert!((*h).output.is_none());
            assert_eq!((*h).input_ctxt, marker);
            assert_eq!(CStr::from_ptr((*h).name).to_str().unwrap(), "x-toytest");
            crate::outbuf::xmlCharEncCloseFunc(h);
        }
    }

    #[test]
    fn custom_conv_impl_drives_transcode() {
        let xml = b"<?xml version=\"1.0\" encoding=\"x-toytest\"?><doc>ab@cd</doc>";
        let ctxt = unsafe { xmlNewParserCtxt() };
        unsafe { xmlCtxtSetCharEncConvImpl(ctxt, Some(toy_factory), std::ptr::null_mut()); }
        let doc = unsafe {
            xmlCtxtReadMemory(ctxt, xml.as_ptr() as *const c_char, xml.len() as c_int,
                              ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null(), "parse should succeed through the custom converter");
        let got = root_text(doc);
        unsafe {
            crate::parse::xmlFreeDoc(doc);
            xmlFreeParserCtxt(ctxt);
        }
        assert_eq!(got, "abXYcd", "custom converter should have rewritten '@' → 'XY'");
    }

    #[test]
    fn custom_conv_impl_decline_falls_back() {
        unsafe extern "C" fn decline_factory(
            _v: *mut c_void,
            _n: *const c_char,
            _f: c_int,
            out: *mut *mut crate::outbuf::xmlCharEncodingHandler,
        ) -> c_int {
            unsafe { *out = std::ptr::null_mut(); }
            32
        }
        let xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><doc>plain</doc>";
        let ctxt = unsafe { xmlNewParserCtxt() };
        unsafe { xmlCtxtSetCharEncConvImpl(ctxt, Some(decline_factory), std::ptr::null_mut()); }
        let doc = unsafe {
            xmlCtxtReadMemory(ctxt, xml.as_ptr() as *const c_char, xml.len() as c_int,
                              ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null(), "declined converter must fall back to the built-in path");
        let got = root_text(doc);
        unsafe {
            crate::parse::xmlFreeDoc(doc);
            xmlFreeParserCtxt(ctxt);
        }
        assert_eq!(got, "plain");
    }

    // ── xmlNewSAXParserCtxt → SAX2 event delivery ──────────────────────

    #[derive(Default)]
    struct SaxRec { starts: Vec<String>, ends: Vec<String>, text: String }
    thread_local! {
        static SAX_REC: std::cell::RefCell<SaxRec> = std::cell::RefCell::new(SaxRec::default());
    }

    unsafe extern "C" fn cb_start(
        _ctx: *mut std::os::raw::c_void, localname: *const c_char,
        _prefix: *const c_char, _uri: *const c_char,
        _nb_ns: c_int, _ns: *const *const c_char,
        _nb_attr: c_int, _nb_def: c_int, _attrs: *const *const c_char,
    ) {
        let name = unsafe { CStr::from_ptr(localname) }.to_str().unwrap_or("").to_string();
        SAX_REC.with(|r| r.borrow_mut().starts.push(name));
    }
    unsafe extern "C" fn cb_end(
        _ctx: *mut std::os::raw::c_void, localname: *const c_char,
        _prefix: *const c_char, _uri: *const c_char,
    ) {
        let name = unsafe { CStr::from_ptr(localname) }.to_str().unwrap_or("").to_string();
        SAX_REC.with(|r| r.borrow_mut().ends.push(name));
    }
    unsafe extern "C" fn cb_chars(_ctx: *mut std::os::raw::c_void, ch: *const c_char, len: c_int) {
        let s = unsafe { std::slice::from_raw_parts(ch as *const u8, len as usize) };
        SAX_REC.with(|r| r.borrow_mut().text.push_str(&String::from_utf8_lossy(s)));
    }

    #[test]
    fn sax_parser_ctxt_fires_sax2_callbacks() {
        SAX_REC.with(|r| *r.borrow_mut() = SaxRec::default());

        // Build a 256-byte xmlSAXHandler with SAX2 callbacks at libxml2's
        // offsets (characters@136, startElementNs@232, endElementNs@240).
        let mut handler = [0u8; 256];
        let put = |buf: &mut [u8; 256], off: usize, p: usize| {
            buf[off..off + 8].copy_from_slice(&p.to_ne_bytes());
        };
        type StartNs = unsafe extern "C" fn(*mut std::os::raw::c_void, *const c_char, *const c_char, *const c_char, c_int, *const *const c_char, c_int, c_int, *const *const c_char);
        type EndNs   = unsafe extern "C" fn(*mut std::os::raw::c_void, *const c_char, *const c_char, *const c_char);
        type Chars   = unsafe extern "C" fn(*mut std::os::raw::c_void, *const c_char, c_int);
        let start: StartNs = cb_start;
        let end:   EndNs   = cb_end;
        let chars: Chars   = cb_chars;
        put(&mut handler, 232, start as *const () as usize);
        put(&mut handler, 240, end   as *const () as usize);
        put(&mut handler, 136, chars as *const () as usize);

        let ctxt = unsafe {
            xmlNewSAXParserCtxt(handler.as_ptr() as *const std::os::raw::c_void, std::ptr::null_mut())
        };
        assert!(!ctxt.is_null());

        let src = b"<a><b>hi</b></a>";
        let doc = unsafe {
            xmlCtxtReadMemory(ctxt, src.as_ptr() as *const c_char, src.len() as c_int,
                              std::ptr::null(), std::ptr::null(), 0)
        };
        unsafe {
            if !doc.is_null() { crate::parse::xmlFreeDoc(doc); }
            xmlFreeParserCtxt(ctxt);
        }

        SAX_REC.with(|r| {
            let rec = r.borrow();
            assert_eq!(rec.starts, vec!["a", "b"], "startElementNs fires per element in document order");
            assert_eq!(rec.ends, vec!["b", "a"], "endElementNs fires in close order");
            assert_eq!(rec.text, "hi", "characters delivers text content");
        });
    }

    #[test]
    fn private_data_round_trips() {
        let ctxt = unsafe { xmlNewParserCtxt() };
        assert!(!ctxt.is_null());

        // Default is NULL, set/get round-trips, and clearing works.
        assert!(unsafe { xmlCtxtGetPrivate(ctxt) }.is_null());
        let marker = 0xDEAD_BEEF_usize as *mut std::os::raw::c_void;
        unsafe { xmlCtxtSetPrivate(ctxt, marker); }
        assert_eq!(unsafe { xmlCtxtGetPrivate(ctxt) }, marker);
        unsafe { xmlCtxtSetPrivate(ctxt, std::ptr::null_mut()); }
        assert!(unsafe { xmlCtxtGetPrivate(ctxt) }.is_null());

        unsafe { xmlFreeParserCtxt(ctxt); }

        // NULL ctxt: get returns NULL, set is a no-op (no crash).
        assert!(unsafe { xmlCtxtGetPrivate(std::ptr::null_mut()) }.is_null());
        unsafe { xmlCtxtSetPrivate(std::ptr::null_mut(), 0x1 as *mut std::os::raw::c_void); }
    }

    #[test]
    fn new_use_read_free_round_trip() {
        let ctxt = unsafe { xmlNewParserCtxt() };
        assert!(!ctxt.is_null());

        // Set + read back options — verify the bits round-trip
        // through the ctxt's stash slot.
        let rc = unsafe { xmlCtxtUseOptions(ctxt, 0x42) };
        assert_eq!(rc, 0);
        let got = unsafe { read_ctxt_options(ctxt) };
        assert_eq!(got, 0x42);

        // Parse a tiny doc via the context.
        let src = CString::new("<r/>").unwrap();
        let doc = unsafe {
            xmlCtxtReadMemory(
                ctxt,
                src.as_ptr(),
                4,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null());
        unsafe { crate::parse::xmlFreeDoc(doc); }

        // Clear is a no-op now — zero state stays zero.
        unsafe { xmlClearParserCtxt(ctxt); }

        unsafe { xmlFreeParserCtxt(ctxt); }
    }

    #[test]
    fn null_safety() {
        // All entry points must be NULL-safe.
        unsafe { xmlFreeParserCtxt(ptr::null_mut()); }
        unsafe { xmlClearParserCtxt(ptr::null_mut()); }
        assert_eq!(unsafe { xmlCtxtUseOptions(ptr::null_mut(), 0) }, -1);
        // xmlCtxtReadMemory with NULL ctxt should still work — context
        // doesn't carry meaningful state in v0.1.
        let src = CString::new("<r/>").unwrap();
        let doc = unsafe {
            xmlCtxtReadMemory(ptr::null_mut(), src.as_ptr(), 4,
                              ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null());
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

    /// libxml2 semantics: `xmlCtxtReadMemory`'s `options` argument
    /// replaces whatever was previously set via
    /// `xmlCtxtUseOptions`.  A caller who set `XML_PARSE_DTDLOAD`
    /// on the ctxt and then passes `options=0` to `xmlCtxtReadMemory`
    /// must NOT see the external entity loaded — per-call wins,
    /// closing the door on "I forgot to clear DTDLOAD" XXE.
    #[test]
    fn ctxt_read_memory_per_call_options_override_stored() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-ctxt-override-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"CTXT_OVERRIDE_SECRET").unwrap();
        }
        let src_str = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = CString::new(src_str.as_str()).unwrap();
        let len = src_str.len() as c_int;

        let ctxt = unsafe { xmlNewParserCtxt() };
        // Stored options: DTDLOAD.  Per-call options: 0 — must win.
        unsafe { xmlCtxtUseOptions(ctxt, 4) };
        let doc = unsafe {
            xmlCtxtReadMemory(ctxt, src.as_ptr(), len,
                              ptr::null(), ptr::null(), 0)
        };
        let got = if doc.is_null() {
            String::new()
        } else {
            let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
            let cp = unsafe { crate::parse::xmlNodeGetContent(root) };
            let s = if cp.is_null() {
                String::new()
            } else {
                let s = unsafe { std::ffi::CStr::from_ptr(cp) }
                    .to_str().unwrap_or("").to_string();
                unsafe { crate::parse::xmlFree(cp as *mut c_void); }
                s
            };
            unsafe { crate::parse::xmlFreeDoc(doc); }
            s
        };
        unsafe { xmlFreeParserCtxt(ctxt); }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            !got.contains("CTXT_OVERRIDE_SECRET"),
            "per-call options=0 should override stored DTDLOAD: {got:?}"
        );
    }

    /// And the converse: per-call `options=XML_PARSE_DTDLOAD` opts
    /// IN even when the ctxt has nothing stored.
    #[test]
    fn ctxt_read_memory_per_call_options_enable_dtdload() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-ctxt-enable-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"CTXT_ENABLE_PAYLOAD").unwrap();
        }
        let src_str = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = CString::new(src_str.as_str()).unwrap();
        let len = src_str.len() as c_int;

        let ctxt = unsafe { xmlNewParserCtxt() };
        // No stored options.  Per-call: NOENT|DTDLOAD.
        let doc = unsafe {
            xmlCtxtReadMemory(ctxt, src.as_ptr(), len,
                              ptr::null(), ptr::null(), 2 | 4)
        };
        assert!(!doc.is_null());
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let cp = unsafe { crate::parse::xmlNodeGetContent(root) };
        let got = unsafe { std::ffi::CStr::from_ptr(cp) }
            .to_str().unwrap_or("").to_string();
        unsafe {
            crate::parse::xmlFree(cp as *mut c_void);
            crate::parse::xmlFreeDoc(doc);
            xmlFreeParserCtxt(ctxt);
        }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            got.contains("CTXT_ENABLE_PAYLOAD"),
            "per-call DTDLOAD|NOENT should enable entity load: {got:?}"
        );
    }

    #[test]
    fn create_file_parser_ctxt_then_parse() {
        // Write a temp XML file, build a ctxt over it, parse, verify
        // the doc landed on myDoc.
        let tmp = std::env::temp_dir().join(format!("cfp_{}.xml", std::process::id()));
        std::fs::write(&tmp, b"<r><a>x</a></r>").unwrap();
        let cstr = std::ffi::CString::new(tmp.to_str().unwrap()).unwrap();

        let ctxt = unsafe { xmlCreateFileParserCtxt(cstr.as_ptr()) };
        assert!(!ctxt.is_null());
        let rc = unsafe { xmlParseDocument(ctxt) };
        assert_eq!(rc, 0);

        // Read ctxt->myDoc (offset 16) — the doc the parse produced.
        let mut bytes = [0u8; std::mem::size_of::<usize>()];
        unsafe {
            std::ptr::copy_nonoverlapping(
                (ctxt as *const u8).add(CTXT_MYDOC_OFFSET),
                bytes.as_mut_ptr(),
                bytes.len(),
            );
        }
        let doc_addr = usize::from_ne_bytes(bytes);
        assert!(doc_addr != 0, "myDoc not populated");
        let doc = doc_addr as *mut XmlDoc;
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        assert!(!root.is_null());

        unsafe {
            crate::parse::xmlFreeDoc(doc);
            xmlFreeParserCtxt(ctxt);
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

// Re-export the v_void marker so any external probe can see what we expect
// the ctxt allocation size to be.  (`#[used]` keeps it out of dead-code
// elimination.)
#[doc(hidden)]
#[used]
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static SUPXML_CTXT_SIZE: usize = std::mem::size_of::<XmlParserCtxt>();
// Suppress unused warning on the marker only — `c_void` is otherwise unused.
const _: *mut c_void = ptr::null_mut();
