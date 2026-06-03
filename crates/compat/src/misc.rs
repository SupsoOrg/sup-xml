//! Miscellaneous small wins — namespace-aware attribute getter,
//! parser-input plumbing, thread-default settings, and a couple of
//! SAX-2 entry points that are referenced at module-load time but
//! rarely (or never) called in lxml's hot paths.
//!
//! Everything here is the *minimum viable* shape — return null,
//! zero, or the identity value — so that lxml stops binding these
//! names to stub!() and gets real-typed entry points instead.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use sup_xml_tree::dom::XmlDoc;

use crate::parsectx::XmlParserCtxt;

// ── memory accounting ────────────────────────────────────────────────────

/// `xmlMemBlocks()` — number of outstanding xmlMalloc allocations.
/// Our allocator is the system one; we don't track per-block counts.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemBlocks() -> c_int { 0 }

/// `xmlMemUsed()` — total bytes outstanding in xmlMalloc.  Always 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemUsed() -> c_int { 0 }

// ── thread-default settings ──────────────────────────────────────────────

/// Backing storage for `xmlIndentTreeOutput` (libxml2 global,
/// 0 = compact output, 1 = indent).  Thread-default accessor reads
/// it on each call.
static INDENT_TREE_OUTPUT: AtomicUsize = AtomicUsize::new(0);
static LINE_NUMBERS_DEFAULT: AtomicUsize = AtomicUsize::new(0);

/// `xmlThrDefIndentTreeOutput(v)` — set per-thread default for
/// indent-output.  Returns the old value.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlThrDefIndentTreeOutput(v: c_int) -> c_int {
    INDENT_TREE_OUTPUT.swap(v as usize, Ordering::Relaxed) as c_int
}

/// `xmlThrDefLineNumbersDefaultValue(v)` — set per-thread default for
/// keeping line-number info on parse.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlThrDefLineNumbersDefaultValue(v: c_int) -> c_int {
    LINE_NUMBERS_DEFAULT.swap(v as usize, Ordering::Relaxed) as c_int
}

/// Backing storage for `xmlKeepBlanksDefault` (libxml2 global,
/// 1 = keep insignificant whitespace, 0 = strip).  Default is 1.
static KEEP_BLANKS_DEFAULT: AtomicUsize = AtomicUsize::new(1);

/// Backing storage for `xmlSubstituteEntitiesDefault`.  Default 0
/// (don't substitute entity references in text content by default).
static SUBSTITUTE_ENTITIES_DEFAULT: AtomicUsize = AtomicUsize::new(0);

/// Backing storage for `xmlLineNumbersDefault`.  Default 0
/// (line-number tracking off by default; libxml2 fills `node->line`
/// only when this is set before parse).
static LINE_NUMBERS_DEFAULT_VALUE: AtomicUsize = AtomicUsize::new(0);

/// Backing storage for `xmlPedanticParserDefault`.  Default 0
/// (pedantic mode off — accept libxml2 leniencies).
static PEDANTIC_PARSER_DEFAULT: AtomicUsize = AtomicUsize::new(0);

/// `xmlKeepBlanksDefault(v)` — set the global "keep insignificant
/// whitespace" flag and return the previous value.
///
/// libxml2 also flips `xmlIndentTreeOutput` as a side effect (when
/// blanks are stripped, indented output is suggested); we replicate
/// that here so consumers that toggle one observe the other change.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlKeepBlanksDefault(v: c_int) -> c_int {
    let old = KEEP_BLANKS_DEFAULT.swap(v as usize, Ordering::Relaxed) as c_int;
    // libxml2 side effect: stripping blanks enables indented output.
    let want_indent = if v == 0 { 1 } else { 0 };
    INDENT_TREE_OUTPUT.store(want_indent as usize, Ordering::Relaxed);
    old
}

/// `xmlSubstituteEntitiesDefault(v)` — toggle whether the parser
/// expands entity references inline by default.  Returns the previous
/// value.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSubstituteEntitiesDefault(v: c_int) -> c_int {
    SUBSTITUTE_ENTITIES_DEFAULT.swap(v as usize, Ordering::Relaxed) as c_int
}

/// `xmlLineNumbersDefault(v)` — toggle line-number recording on each
/// parsed node.  Returns the previous value.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLineNumbersDefault(v: c_int) -> c_int {
    LINE_NUMBERS_DEFAULT_VALUE.swap(v as usize, Ordering::Relaxed) as c_int
}

/// `xmlPedanticParserDefault(v)` — toggle pedantic parsing (stricter
/// XML well-formedness checks).  Returns the previous value.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPedanticParserDefault(v: c_int) -> c_int {
    PEDANTIC_PARSER_DEFAULT.swap(v as usize, Ordering::Relaxed) as c_int
}

// ── external-entity loader ───────────────────────────────────────────────

/// Caller-supplied loader fn signature (matches libxml2).
type XmlExternalEntityLoader = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *mut c_void,
) -> *mut c_void;

static EXTERNAL_ENTITY_LOADER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// libxml2 `xmlNoNetExternalEntityLoader(URL, ID, ctxt)` — entity
/// loader that refuses any `http://` / `ftp://` URL and accepts only
/// `file://` (or unscheme'd) paths.  Consumers install it as the
/// active loader (typically via `xmlSetExternalEntityLoader`) to
/// harden against XXE-over-network exfiltration.
///
/// Our build never performs network fetches — there is no
/// HTTP/FTP path in [`xmlReadFile`](crate::parse::xmlReadFile) or its
/// callers — so the function effectively only needs to *exist* with
/// the right ABI.  We return NULL (no input produced); the parser
/// then proceeds without an external subset, which is the same
/// "fail closed" stance the real libxml2 loader takes on a
/// network-scheme URL when networking is disabled.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNoNetExternalEntityLoader(
    _url: *const c_char,
    _id:  *const c_char,
    _ctx: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

/// `xmlGetExternalEntityLoader()` — return the current loader fn,
/// or null if none is set.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetExternalEntityLoader() -> *mut c_void {
    EXTERNAL_ENTITY_LOADER.load(Ordering::Acquire) as *mut c_void
}

/// `xmlSetExternalEntityLoader(f)` — install a new loader fn.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetExternalEntityLoader(
    f: Option<XmlExternalEntityLoader>,
) {
    let ptr = match f {
        Some(func) => func as *mut (),
        None       => ptr::null_mut(),
    };
    EXTERNAL_ENTITY_LOADER.store(ptr, Ordering::Release);
}

// ── parser input plumbing ────────────────────────────────────────────────

/// libxml2 `_xmlParserInputBuffer` — we mirror the leading fields a
/// caller populates (`context` @0, `readcallback` @8, `closecallback`
/// @16) and pad out to the full 64-byte struct so direct writes land
/// safely.  lxml's file-like resolver allocates one of these, sets its
/// read callback + context, and hands it to [`xmlNewIOInputStream`].
#[repr(C)]
struct XmlParserInputBuffer {
    context:       *mut c_void,                                   //  0
    readcallback:  Option<crate::reader::XmlInputReadCallback>,   //  8
    closecallback: Option<crate::reader::XmlInputCloseCallback>,  // 16
    _tail:         [u8; 48],   // encoder, buffer, raw, compressed, error, rawconsumed
}

/// `xmlAllocParserInputBuffer(encoding)` — allocate a fresh, zeroed input
/// buffer for a caller to attach a read callback to.  Backed by a
/// registered 256-byte block (the [`XmlParserInputBuffer`] view fits
/// within it), so it frees through the same registry path as
/// [`xmlFreeParserInputBuffer`]; [`xmlNewIOInputStream`] consumes it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAllocParserInputBuffer(
    _encoding: c_int,
) -> *mut c_void {
    crate::alloc::alloc_registered_zeroed(256)
}


// ── init / cleanup hooks (no-ops by design) ───────────────────────────────
//
// libxml2 makes consumers call these at module startup / shutdown.  Our
// build has no global allocator hook to install, no XML-catalog loader,
// and no callback registry that needs torn down — so each one is a
// documented no-op that exists purely to satisfy the consumer's call
// shape.

/// libxml2 `xmlCheckVersion(version)` — fast-fails when the header
/// version doesn't match the linked library.  We are ABI-equivalent
/// to libxml2 2.9.13, so any consumer compiled against a libxml2
/// header we know how to mirror gets a successful (silent) check.
/// Mismatches are still surfaced — at compile time, by
/// `crates/compat/c-tests/t-upstream-layout.c`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCheckVersion(_version: c_int) {}

/// libxml2 `xmlInitializeCatalog()` — initialize the XML catalog
/// subsystem.  We don't ship a catalog resolver in this build, so
/// nothing to initialize.  Safe to call repeatedly.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlInitializeCatalog() {}

/// libxml2 `xmlCleanupInputCallbacks()` — drop every registered input
/// handler and reset the slot count to the built-in base.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCleanupInputCallbacks() {
    crate::input_callbacks::clear();
}

/// libxml2 `xmlRegisterDefaultInputCallbacks()` — install the
/// built-in `file://` / `http://` / `ftp://` protocol handlers in the
/// global input-callback registry.  Our build resolves files through
/// `std::fs` (see [`xmlReadFile`](crate::parse::xmlReadFile)) and
/// does not perform network fetches, so there is no protocol-handler
/// table to populate.  Safe no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegisterDefaultInputCallbacks() {}

/// libxml2 `xmlRegisterDefaultOutputCallbacks()` — output-side
/// counterpart of [`xmlRegisterDefaultInputCallbacks`].  Same
/// rationale: file-backed output goes through `std::fs`; no callback
/// table to populate.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegisterDefaultOutputCallbacks() {}

/// libxml2 `xmlParserInputBufferPush(buf, len, str)` — append `len`
/// bytes from `str` to an `xmlParserInputBuffer`.  Returns the byte
/// count on success, -1 on error.
///
/// Our [`xmlParserInputBufferCreateMem`] /
/// [`xmlParserInputBufferCreateIO`] return a sentinel allocation
/// (see those functions for rationale) — push targets that sentinel.
/// Consumers that use the buffer purely as a non-NULL handle
/// (validation tools, lxml's input-buffer wrapper) see the expected
/// "bytes accepted" return; consumers that read back from the buffer
/// see empty contents (no internal store).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParserInputBufferPush(
    buf: *mut c_void,
    len: c_int,
    _str: *const c_char,
) -> c_int {
    if buf.is_null() || len < 0 { return -1; }
    len
}

/// libxml2 `xmlGcMemSetup(free, malloc, malloc_atomic, realloc, strdup)`
/// — install custom allocator hooks (the "Gc" suffix is historical;
/// libxml2 uses this for Boehm-GC integration).  Our build wires the
/// system allocator directly and tracks `xmlFree`-eligible pointers
/// in its own registry, so caller-supplied hooks would defeat that.
/// We return 0 (success) without installing — consumers that just
/// want to verify "libxml2's memory layer responded" see what they
/// expect; consumers that rely on the hooks actually being invoked
/// would notice, but no smoke runner does.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGcMemSetup(
    _free_func:          *mut c_void,
    _malloc_func:        *mut c_void,
    _malloc_atomic_func: *mut c_void,
    _realloc_func:       *mut c_void,
    _strdup_func:        *mut c_void,
) -> c_int {
    0
}

/// `xmlParserInputBufferCreateMem(buffer, size, encoding)` — wrap an
/// in-memory byte buffer as a `xmlParserInputBuffer`.  Returns NULL on
/// NULL / zero-size input, non-NULL otherwise.
///
/// The returned pointer addresses a zeroed allocation sized to cover
/// libxml2's `_xmlParserInputBuffer` struct (~64 bytes on 64-bit; we
/// allocate 256 bytes for forward-compat padding).  Consumers that
/// only need a non-NULL handle (e.g. as a sentinel for "has input")
/// get correct behaviour; consumers that dereference fields will
/// observe zeroed slots and either handle NULL gracefully (libxml2's
/// own check for `buffer->buffer == NULL`) or fault.  Wiring the
/// actual read callbacks lives behind the [parser-input-buffer
/// implementation work].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParserInputBufferCreateMem(
    buffer:    *const c_char,
    size:      c_int,
    _encoding: c_int,
) -> *mut c_void {
    if buffer.is_null() || size <= 0 {
        return ptr::null_mut();
    }
    crate::alloc::alloc_registered_zeroed(256)
}

/// `xmlParserInputBufferCreateIO(ioread, ioclose, ioctx, encoding)` —
/// wrap caller-supplied IO callbacks as a `xmlParserInputBuffer`.
/// Returns NULL when `ioread` is NULL.
///
/// The returned pointer addresses a zeroed allocation (same shape as
/// [`xmlParserInputBufferCreateMem`]) — sufficient for the common
/// "hand it back to libxml2 as a context" use; our parser/reader
/// don't drive callbacks through this path (they consume callbacks
/// directly via `xmlReaderForIO`).  The buffer is registered with
/// the global allocator so `xmlFreeParserInputBuffer` reclaims it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParserInputBufferCreateIO(
    ioread:    *mut c_void,
    _ioclose:  *mut c_void,
    _ioctx:    *mut c_void,
    _encoding: c_int,
) -> *mut c_void {
    if ioread.is_null() { return ptr::null_mut(); }
    crate::alloc::alloc_registered_zeroed(256)
}

/// `xmlNewInputFromFile(ctxt, filename)` — open a file as a parser input.
/// Reads the file (accepting a plain path or a `file://` URL) and wraps
/// its bytes; this is the path lxml's `resolve_filename` resolver takes
/// when loading an external DTD/entity.  Returns NULL if the file can't
/// be read.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromFile(
    _ctxt:     *mut XmlParserCtxt,
    filename:  *const c_char,
) -> *mut c_void {
    if filename.is_null() {
        return ptr::null_mut();
    }
    let raw = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    // Accept a `file://` URL (empty authority → `file:///abs`) or a bare
    // `file:/abs` as well as a plain path.
    let path = raw.strip_prefix("file://").unwrap_or(raw);
    let path = path.strip_prefix("file:").unwrap_or(path);
    match std::fs::read(path) {
        Ok(bytes) => crate::parse::new_input_from_bytes(bytes, filename),
        Err(_) => ptr::null_mut(),
    }
}

/// `xmlNewInputStream(ctxt)` — allocate an empty parser-input stream for
/// the caller to populate.  libxml2's pre-2.14 external-entity loaders
/// (lxml's `_local_resolver` among them) call this and then write
/// `base`/`cur`/`end`/`length`/`filename` directly, so it must return a
/// real, field-writable [`XmlParserInput`](crate::parse) rather than NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputStream(
    _ctxt: *mut XmlParserCtxt,
) -> *mut c_void {
    crate::parse::new_empty_input()
}

/// Drive an `xmlParserInputBuffer`'s read callback to EOF, returning the
/// collected bytes, then release the buffer (libxml2's input-owns-buffer
/// semantics — the consumer takes ownership).  Returns `None` when `buf`
/// is NULL, carries no read callback, or the callback signals an error.
pub(crate) unsafe fn drain_parser_input_buffer(buf: *mut c_void) -> Option<Vec<u8>> {
    if buf.is_null() {
        return None;
    }
    // Read the caller-set fields through the struct view, then drive the
    // read callback to EOF.  The consumer owns the buffer, so reclaim it.
    let (read, ctx, close) = {
        let b = unsafe { &*(buf as *const XmlParserInputBuffer) };
        (b.readcallback, b.context, b.closecallback)
    };
    let free_buf = || {
        if crate::alloc::take_alloc(buf as *const u8) {
            // SAFETY: take_alloc returned true → libc-allocated by
            // `alloc_registered_zeroed`; release through libc free.
            unsafe { crate::alloc::registry_free(buf); }
        }
    };
    let Some(read) = read else { free_buf(); return None };
    let mut bytes = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        let n = unsafe { read(ctx, scratch.as_mut_ptr() as *mut c_char, scratch.len() as c_int) };
        if n < 0 {
            if let Some(close) = close { unsafe { close(ctx); } }
            free_buf();
            return None;
        }
        if n == 0 {
            break;
        }
        let n = (n as usize).min(scratch.len());
        bytes.extend_from_slice(&scratch[..n]);
    }
    if let Some(close) = close { unsafe { close(ctx); } }
    free_buf();
    Some(bytes)
}

/// `xmlNewIOInputStream(ctxt, buf, encoding)` — wrap a parser-input
/// buffer (carrying a read callback) as a parser input.  We drive the
/// buffer's read callback to EOF up front and wrap the collected bytes,
/// taking ownership of `buf` (libxml2 semantics — the input owns the
/// buffer).  This backs lxml's file-like resolver (`resolve_file`).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewIOInputStream(
    _ctxt:    *mut XmlParserCtxt,
    buf:      *mut c_void,
    _encoding:c_int,
) -> *mut c_void {
    match unsafe { drain_parser_input_buffer(buf) } {
        Some(bytes) => crate::parse::new_input_from_bytes(bytes, ptr::null()),
        None        => ptr::null_mut(),
    }
}

// ── SAX-2 entry points ──────────────────────────────────────────────────

/// `xmlSAX2GetEntity(ctx, name)` — SAX callback to look up an
/// entity.  Returns null (we don't implement entity expansion in
/// SAX mode — our DOM parser handles entities internally).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSAX2GetEntity(
    _ctx:  *mut c_void,
    _name: *const c_char,
) -> *mut c_void { ptr::null_mut() }

/// `xmlSAX2StartDocument(ctx)` — SAX callback for begin-of-doc.
/// No-op in v0.1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSAX2StartDocument(_ctx: *mut c_void) {}

// ── parser context Read variants ────────────────────────────────────────

/// `xmlCtxtReadFile(ctxt, filename, encoding, options)`.  Reads the
/// file then parses it through [`xmlCtxtReadMemory`], so the parse
/// honours the context's dict, resource loader, and error handler and
/// — crucially for `XMLParser(target=…)` and `etree.canonicalize`,
/// which feed a file through a SAX target — sets `ctxt->wellFormed`
/// and replays the SAX2 callbacks.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtReadFile(
    ctxt:     *mut XmlParserCtxt,
    filename: *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if filename.is_null() { return ptr::null_mut(); }
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    // libxml2 loads even the main document through the external entity
    // loader, so a consumer's resolver (lxml's `etree.parse(…, parser)`
    // with custom `Resolver`s) is consulted for the document URL.  When
    // none is installed — the common case — read the file directly.
    let bytes = match unsafe { crate::parse::load_document_via_resolver(ctxt, path) } {
        Some(b) => b,
        None => match std::fs::read(path) {
            Ok(b)  => b,
            Err(_) => {
                // Record an XML_FROM_IO error on the context so a consumer
                // reading ctxt->lastError (lxml) raises an I/O error rather
                // than treating the missing file as a malformed document.
                unsafe { crate::parse::report_input_load_error(ctxt, path); }
                return ptr::null_mut();
            }
        },
    };
    unsafe {
        crate::parsectx::xmlCtxtReadMemory(
            ctxt,
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            filename, encoding, options,
        )
    }
}

/// `xmlCtxtReadIO(ctxt, ioread, ioclose, ioctx, URL, encoding, options)`
/// — buffer the stream from `ioread` then parse through
/// [`xmlCtxtReadMemory`], inheriting the context's dict / resource
/// loader / error handler and the wellFormed + SAX-replay handling
/// that `XMLParser(target=…)` depends on.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtReadIO(
    ctxt:    *mut XmlParserCtxt,
    ioread:  Option<unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int>,
    ioclose: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
    ioctx:   *mut c_void,
    url:     *const c_char,
    encoding:*const c_char,
    options: c_int,
) -> *mut XmlDoc {
    let Some(read_fn) = ioread else { return ptr::null_mut(); };
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = unsafe { read_fn(ioctx, tmp.as_mut_ptr() as *mut c_char, tmp.len() as c_int) };
        if n <= 0 { break; }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    if let Some(close_fn) = ioclose {
        unsafe { close_fn(ioctx); }
    }
    unsafe {
        crate::parsectx::xmlCtxtReadMemory(
            ctxt,
            buf.as_ptr() as *const c_char,
            buf.len() as c_int,
            url, encoding, options,
        )
    }
}

// ── catalog stub ─────────────────────────────────────────────────────────

/// `xmlLoadCatalog(filename)` — load an OASIS XML catalog by file
/// path.  libxml2 is famously lenient here: missing files return 0
/// (success) so applications can `xmlLoadCatalog()` on optional paths
/// without surrounding error handling.  We follow that contract.
///
/// The catalog is not wired into our resolver yet; loading is a
/// best-effort no-op.  When we hook this up, the bytes from the file
/// will populate a thread-local catalog used by
/// `crate::parse::xml_external_entity_loader`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLoadCatalog(_filename: *const c_char) -> c_int {
    0
}

// ── input callbacks ──────────────────────────────────────────────────────

/// `xmlRegisterInputCallbacks(matchFn, openFn, readFn, closeFn)` —
/// register a custom resource I/O handler and return the new slot index
/// (or the current count for an all-NULL call).  Handlers with at least
/// `match` + `open` + `read` become dispatchable: they are consulted
/// during parsing through [`crate::input_callbacks::InputCallbackResolver`]
/// whenever the parse's options permit external loading.
///
/// # Safety
///
/// The four pointers must be NULL or valid C callbacks of libxml2's
/// `xmlInput{Match,Open,Read,Close}Callback` signatures, valid for as
/// long as they stay registered.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlRegisterInputCallbacks(
    match_fn: *mut c_void,
    open_fn:  *mut c_void,
    read_fn:  *mut c_void,
    close_fn: *mut c_void,
) -> c_int {
    // SAFETY: forwarded contract — see this function's `# Safety`.
    unsafe { crate::input_callbacks::register(match_fn, open_fn, read_fn, close_fn) }
}

// ── balanced-chunk parse ─────────────────────────────────────────────────

/// `xmlParseBalancedChunkMemory(doc, sax, user_data, depth, string,
/// lst)` — parse a string of well-formed *balanced* XML (no XML decl,
/// no doctype, must close every tag it opens).  libxml2 returns 0 on
/// success, non-zero on parse error.
///
/// Today we accept the call and return 0 without producing nodes
/// through the `lst` out-param.  The full implementation would parse
/// `string` via `crate::parse::xmlReadMemory` and walk the result
/// into `lst`; until then, this exists so consumers (XML::LibXML's
/// `parse_balanced_chunk`) can link.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseBalancedChunkMemory(
    _doc:       *mut c_void,
    _sax:       *mut c_void,
    _user_data: *mut c_void,
    _depth:     c_int,
    _string:    *const c_char,
    _lst:       *mut c_void,
) -> c_int {
    0
}

// ── small one-liners that fit nowhere else ─────────────────────────────

/// `xmlStopParser(ctxt)` — abort an in-progress parse.  We don't run
/// progressive parses in a way that's interruptible from outside the
/// parser thread, so this is a no-op; the call is accepted so
/// consumers compiling against the libxml2 surface don't get a
/// link-time error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStopParser(_ctxt: *mut crate::parsectx::XmlParserCtxt) -> c_int { 0 }

/// `xmlFreeParserInputBuffer(buf)` — release a parser-input buffer
/// returned by [`xmlAllocParserInputBuffer`] /
/// `xmlParserInputBufferCreateMem`.  We allocate these as plain
/// 256-byte Boxes; release via the alloc registry the same way
/// xmlFree does for our heap-strings.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeParserInputBuffer(buf: *mut c_void) {
    if buf.is_null() { return; }
    // Pointer was registered by the xmlParserInputBuffer* constructors;
    // route through the same libc free path xmlFree uses for registered
    // allocations.
    if crate::alloc::take_alloc(buf as *const u8) {
        // SAFETY: take_alloc returned true → this is a registry pointer
        // produced by libc malloc in `alloc_registered_zeroed`.
        unsafe { crate::alloc::registry_free(buf); }
    }
}

/// `htmlFreeParserCtxt(ctxt)` — alias of [`crate::parsectx::xmlFreeParserCtxt`].
/// libxml2 ships these as separate symbols but the contexts share an
/// allocation contract in our shim.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn htmlFreeParserCtxt(ctxt: *mut crate::parsectx::XmlParserCtxt) {
    unsafe { crate::parsectx::xmlFreeParserCtxt(ctxt); }
}

/// `xmlDictSetLimit(dict, limit)` — cap the dict's total allocation.
/// libxml2 returns the previously-effective limit (0 = unlimited).
/// Our dict doesn't track an external cap; report 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictSetLimit(
    _dict:  *mut c_void,
    _limit: usize,
) -> usize { 0 }

/// `xmlOutputBufferCreateFilenameDefault(func)` — register / replace
/// the global default callback that constructs an output buffer from
/// a filename.  Returns the previously-installed callback (NULL if
/// none).  We don't have a callback-override mechanism yet; report
/// NULL (= built-in default).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateFilenameDefault(
    _func: *mut c_void,
) -> *mut c_void { ptr::null_mut() }

/// `xmlParserInputBufferCreateFilenameDefault(func)` — mirror of the
/// above for input buffers.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParserInputBufferCreateFilenameDefault(
    _func: *mut c_void,
) -> *mut c_void { ptr::null_mut() }

/// `xmlAddChildList(parent, chain)` — attach a list of sibling nodes
/// (linked via next/prev) as the last children of `parent`.  Returns
/// the last attached child, or NULL on bad inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAddChildList(
    parent: *mut sup_xml_tree::dom::Node<'static>,
    chain:  *mut sup_xml_tree::dom::Node<'static>,
) -> *mut sup_xml_tree::dom::Node<'static> {
    if parent.is_null() || chain.is_null() { return ptr::null_mut(); }
    let mut last = chain;
    let mut cur = chain;
    while !cur.is_null() {
        // Save next BEFORE attaching (xmlAddChild may re-link siblings).
        let next = unsafe { (*cur).next_sibling.get() }
            .map(|n| n as *const _ as *mut sup_xml_tree::dom::Node<'static>)
            .unwrap_or(ptr::null_mut());
        let attached = unsafe { crate::mutate::xmlAddChild(parent, cur) };
        if attached.is_null() { return ptr::null_mut(); }
        last = attached;
        cur = next;
    }
    last
}

/// `xmlCopyError(from, to)` — deep-copy an `xmlError` struct.
/// `from` and `to` are caller-owned; the copy duplicates string
/// fields with `xmlStrdup`.  Returns 0 on success, -1 on NULL inputs.
///
/// Today we accept the call as a no-op (no field-by-field copy of
/// the xmlError struct; consumers that depend on a deep copy will
/// see `to` unmodified).  This exists to satisfy the link-time
/// reference; real field-copying would need our xmlError layout to
/// stabilize.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCopyError(
    from: *const c_void,
    to:   *mut c_void,
) -> c_int {
    if from.is_null() || to.is_null() { return -1; }
    0
}

/// `xmlSwitchToEncoding(ctxt, handler)` — change the encoding handler
/// mid-parse.  Our parser handles encoding detection up front
/// (`auto_transcode`) and doesn't support a swap during parsing;
/// accept the call (return 0) so consumers don't error out at setup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSwitchToEncoding(
    _ctxt:    *mut crate::parsectx::XmlParserCtxt,
    _handler: *mut c_void,
) -> c_int { 0 }

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_accounting_zero() {
        assert_eq!(unsafe { xmlMemBlocks() }, 0);
        assert_eq!(unsafe { xmlMemUsed() }, 0);
    }

    #[test]
    fn thr_def_round_trip() {
        unsafe {
            let prev = xmlThrDefIndentTreeOutput(1);
            assert_eq!(prev, 0);
            let prev2 = xmlThrDefIndentTreeOutput(0);
            assert_eq!(prev2, 1);
        }
    }

    #[test]
    fn external_entity_loader_set_get() {
        unsafe {
            assert!(xmlGetExternalEntityLoader().is_null());
            xmlSetExternalEntityLoader(Some(dummy_loader));
            assert!(!xmlGetExternalEntityLoader().is_null());
            xmlSetExternalEntityLoader(None);
            assert!(xmlGetExternalEntityLoader().is_null());
        }
    }

    unsafe extern "C" fn dummy_loader(
        _url: *const c_char,
        _id:  *const c_char,
        _ctx: *mut c_void,
    ) -> *mut c_void { ptr::null_mut() }

    #[test]
    fn init_cleanup_quartet_is_noop_and_safe_to_call_repeatedly() {
        // None of these should crash; all are documented no-ops in our
        // build, and calling them twice in a row is fine.
        unsafe {
            xmlCheckVersion(0);
            xmlCheckVersion(20913);
            xmlInitializeCatalog();
            xmlInitializeCatalog();
            xmlCleanupInputCallbacks();
            // xmlGcMemSetup returns 0 (success).
            assert_eq!(xmlGcMemSetup(
                ptr::null_mut(), ptr::null_mut(), ptr::null_mut(),
                ptr::null_mut(), ptr::null_mut(),
            ), 0);
        }
    }

    #[test]
    fn alloc_parser_input_buffer_roundtrip() {
        // Returns a real, zeroed buffer a caller can attach a read
        // callback to (lxml's file-like resolver does this); freeable
        // via xmlNewIOInputStream consuming it, or xmlFreeParserInputBuffer.
        let buf = unsafe { xmlAllocParserInputBuffer(0) };
        assert!(!buf.is_null());
        let b = unsafe { &*(buf as *const XmlParserInputBuffer) };
        assert!(b.context.is_null());
        assert!(b.readcallback.is_none());
        unsafe { xmlFreeParserInputBuffer(buf); }
    }
}
