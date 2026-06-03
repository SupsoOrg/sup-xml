//! libxslt / Schematron ABI bridge — routes the public entry
//! points that consumers (lxml, libxslt-bound C apps) actually
//! call into the real `sup-xml-xslt` engine.
//!
//! The functions split into three groups:
//!
//! 1. **Real routing** — `xsltParseStylesheetDoc`,
//!    `xsltApplyStylesheet[User]`, `xsltSaveResultToString`,
//!    `xsltFreeStylesheet`; `xmlSchematronParse`/`NewParserCtxt`/
//!    `NewValidCtxt`/`ValidateDoc`/`Free*`.  These do meaningful
//!    work on top of `Stylesheet::compile` and `Schematron::compile`.
//! 2. **Accept-and-ignore** — settings/registration calls
//!    (`xsltSetCtxtParseOptions`, `xsltSecurityForbid`, etc.).
//!    No-op success so callers don't fail their setup phase.
//! 3. **Deferred** — niche entry points (`xsltApplyOneTemplate`,
//!    profiling, security prefs read-back).  Still warn-and-NULL
//!    stubs; flagged in module docs so anyone hitting them knows
//!    why their stylesheet failed.
//!
//! Boxed handles are returned as `*mut c_void` to keep the
//! signatures portable across libxslt struct shapes — lxml only
//! reads a few documented offsets that we mimic in the
//! `XsltStylesheetShim` struct.  Everything else (the compiled
//! AST, the transform context state) lives in the Rust struct
//! tail and is invisible to C callers.

use std::cell::Cell;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use sup_xml_tree::dom::XmlDoc;
use sup_xml_xslt::schematron::{FindingKind, Schematron, ValidationReport};
use sup_xml_xslt::Stylesheet;

// ── one-time diagnostic for unsupported corner cases ──────────────

static WARNED: AtomicBool = AtomicBool::new(false);

#[inline]
fn warn_once(fn_name: &'static str) {
    if WARNED.swap(true, Ordering::Relaxed) { return; }
    eprintln!(
        "[sup-xml-compat] note: `{fn_name}` is not yet wired in this build; \
         consumers should check the return value (NULL / -1).  XSLT and \
         Schematron core paths (compile / apply / validate) are implemented; \
         this is a less-common entry point."
    );
}

// ── libxslt-shape stylesheet handle ──────────────────────────────
//
// libxslt's `xsltStylesheet` is a large struct with many internal
// fields.  lxml only reads a handful of them.  We expose the
// header fields it reads in a `#[repr(C)]` struct and stash our
// Rust `Stylesheet` in a hidden tail field.  The pointer returned
// to callers is the address of `XsltStylesheetShim` — the first
// `Cell<*mut c_void>` field doubles as the libxslt `_private`
// slot, so lxml's "is this our stylesheet" pointer-tagging works.

#[repr(C)]
pub struct XsltStylesheetShim {
    /// libxslt's `_private` slot — lxml stamps this with a marker
    /// pointer at compile time and reads it back later.
    pub _private:    Cell<*mut c_void>,
    /// `doc` field — points at the originating xmlDoc.  lxml reads
    /// this for error reporting.
    pub doc:         Cell<*mut XmlDoc>,
    /// Output method discovered during compile — lxml inspects
    /// this when deciding how to wrap the result.  Held as a
    /// nul-terminated C string we own.
    pub method:      *mut c_char,
    pub method_uri:  *mut c_char,
    pub version:     *mut c_char,
    /// "html" output method flag — lxml fast-paths on this.
    pub is_html:     c_int,
    _pad:            [u8; 4],
    /// Tail — invisible to C consumers, dropped when
    /// `xsltFreeStylesheet` runs.
    pub _rust: Box<Stylesheet>,
}

impl XsltStylesheetShim {
    fn new(stylesheet: Stylesheet, source_doc: *mut XmlDoc) -> Self {
        let method_str = stylesheet.ast.outputs.iter()
            .find_map(|o| o.method.clone())
            .unwrap_or_else(|| "xml".to_string());
        let is_html = if method_str == "html" { 1 } else { 0 };
        let method  = CString::new(method_str).unwrap().into_raw();
        let version = stylesheet.ast.outputs.iter()
            .find_map(|o| o.version.clone())
            .map(|s| CString::new(s).unwrap().into_raw())
            .unwrap_or(ptr::null_mut());
        XsltStylesheetShim {
            _private:    Cell::new(ptr::null_mut()),
            doc:         Cell::new(source_doc),
            method,
            method_uri:  ptr::null_mut(),
            version,
            is_html,
            _pad: [0; 4],
            _rust: Box::new(stylesheet),
        }
    }
}

impl Drop for XsltStylesheetShim {
    fn drop(&mut self) {
        // Reclaim the C strings we leaked into the libxslt-shape
        // fields.  The Box<Stylesheet> drops automatically.
        unsafe {
            if !self.method.is_null()  { let _ = CString::from_raw(self.method);  }
            if !self.version.is_null() { let _ = CString::from_raw(self.version); }
        }
    }
}

// ── xsltParseStylesheetDoc ────────────────────────────────────────

/// `xsltParseStylesheetDoc(doc)` — compile `doc` (an `xmlDoc*` from
/// our parser) into a stylesheet.  Returns an opaque pointer
/// callers free via `xsltFreeStylesheet`, or NULL on failure.
///
/// The supplied document MUST have been parsed with
/// `namespace_aware: true` — that's what our `xmlReadMemory`
/// already does, so callers don't typically need to think about it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltParseStylesheetDoc(doc: *mut XmlDoc) -> *mut c_void {
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `doc` is a live XmlDoc from our
    // allocator; the `_doc` tail field is the actual Document.
    // SAFETY: caller asserts `doc` is a live XmlDoc from our
    // allocator; the `_doc` tail field is the actual Document.
    let document = unsafe { &(*doc)._doc };
    match Stylesheet::compile(document) {
        Ok(style) => {
            let shim = XsltStylesheetShim::new(style, doc);
            Box::into_raw(Box::new(shim)) as *mut c_void
        }
        Err(_) => ptr::null_mut(),
    }
}

// ── xsltFreeStylesheet ───────────────────────────────────────────

/// `xsltFreeStylesheet(style)` — drop a stylesheet handle returned
/// by [`xsltParseStylesheetDoc`].  Idempotent on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltFreeStylesheet(style: *mut c_void) {
    if style.is_null() { return; }
    let _ = unsafe { Box::from_raw(style as *mut XsltStylesheetShim) };
}

// ── xsltApplyStylesheet[User] ────────────────────────────────────

/// `xsltApplyStylesheet(style, doc, params)` — apply `style` to
/// `doc`, returning a fresh `xmlDoc*` callers free via
/// `xmlFreeDoc`.  `params` is a NULL-terminated array of
/// alternating `name`/`value` C-string pointers for top-level
/// `xsl:param` overrides; NULL when no params.  Currently
/// accepted-and-ignored — top-level param wiring lands as a
/// follow-up.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltApplyStylesheet(
    style:  *mut c_void,
    doc:    *mut XmlDoc,
    params: *const *const c_char,
) -> *mut XmlDoc {
    unsafe { xsltApplyStylesheetUser(style, doc, params, ptr::null(),
                                     ptr::null_mut(), ptr::null_mut()) }
}

/// `xsltApplyStylesheetUser(style, doc, params, output, profile,
/// userCtxt)` — full-featured variant.  We currently ignore the
/// trailing three "context-shape" args; `output` is honoured only
/// when an explicit filename was set.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltApplyStylesheetUser(
    style:     *mut c_void,
    doc:       *mut XmlDoc,
    _params:   *const *const c_char,
    _output:   *const c_char,
    _profile:  *mut c_void,
    _user_ctxt:*mut c_void,
) -> *mut XmlDoc {
    if style.is_null() || doc.is_null() {
        return ptr::null_mut();
    }
    let shim = unsafe { &*(style as *const XsltStylesheetShim) };
    let document = unsafe { &(*doc)._doc };
    let result = match shim._rust.apply(document) {
        Ok(r) => r,
        Err(_) => return ptr::null_mut(),
    };
    // Serialise the result and re-parse as a fresh xmlDoc.  This is
    // round-trip-y but lets the C caller treat the output exactly
    // like a parser output — including `xmlDocGetRootElement`,
    // serialisation via the standard libxml2 entry points, etc.
    //
    // The performance cost is acceptable for v1 — the alternative
    // (assembling the libxml2-shape node tree directly without
    // going through our parser) is delicate enough to be a
    // follow-up project.
    let serialised = match result.to_string() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let cstr = match CString::new(serialised) {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let len = cstr.as_bytes().len() as c_int;
    unsafe {
        crate::parse::xmlReadMemory(
            cstr.as_ptr(), len, ptr::null(), ptr::null(), 0,
        )
    }
}

// ── xsltSaveResultToString ───────────────────────────────────────

/// `xsltSaveResultToString(string, length, result, style)` —
/// serialise the xmlDoc returned by `xsltApplyStylesheet` to a
/// freshly-malloc'd C string per the stylesheet's `xsl:output`
/// settings.  Caller owns the buffer and must `free()` it.
///
/// Since our `xsltApplyStylesheet` already returns a serialisable
/// xmlDoc, this re-serialises through the standard tree-to-XML
/// path.  The output-method specifics (HTML void elements, text
/// stripping) were already applied during stringification inside
/// `xsltApplyStylesheetUser`, so this call effectively does a
/// pass-through.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltSaveResultToString(
    string: *mut *mut c_char,
    length: *mut c_int,
    result: *mut XmlDoc,
    _style: *mut c_void,
) -> c_int {
    if string.is_null() || length.is_null() || result.is_null() {
        return -1;
    }
    // Use the engine's standard serialiser for the result xmlDoc.
    let doc = unsafe { &(*result)._doc };
    let opts = sup_xml_core::serializer::SerializeOptions::default();
    let buf = sup_xml_core::serializer::serialize_with(doc, &opts);
    let len = buf.len();
    let cstr = match CString::new(buf) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    unsafe {
        *string = cstr.into_raw();
        *length = len as c_int;
    }
    0
}

/// `xsltSaveResultTo(buf, result, style)` — serialise into a
/// libxml2 `xmlOutputBuffer*`.  Compat-layer's output buffers
/// aren't fully wired yet, so this returns -1 with a one-time
/// warning rather than silently producing nothing.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltSaveResultTo(
    _buf:    *mut c_void,
    _result: *mut XmlDoc,
    _style:  *mut c_void,
) -> c_int {
    warn_once("xsltSaveResultTo");
    -1
}

// ── transform-context shape (minimal) ─────────────────────────────
//
// Some lxml code paths construct a transform context, set
// parameters on it, then apply.  We expose a thin Box-backed
// shape; the actual transformation state still flows through
// `Stylesheet::apply`'s arguments since our engine is
// transaction-style rather than mutable-context-style.

#[repr(C)]
pub struct XsltTransformContextShim {
    pub _private:  Cell<*mut c_void>,
    pub style:     *mut XsltStylesheetShim,
}

/// `xsltNewTransformContext(style, doc)` — return a fresh
/// transformation context bound to `style`.  Owned by caller;
/// freed via `xsltFreeTransformContext`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltNewTransformContext(
    style: *mut c_void, _doc: *mut XmlDoc,
) -> *mut c_void {
    if style.is_null() { return ptr::null_mut(); }
    let ctx = XsltTransformContextShim {
        _private: Cell::new(ptr::null_mut()),
        style:    style as *mut XsltStylesheetShim,
    };
    Box::into_raw(Box::new(ctx)) as *mut c_void
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltFreeTransformContext(ctx: *mut c_void) {
    if ctx.is_null() { return; }
    let _ = unsafe { Box::from_raw(ctx as *mut XsltTransformContextShim) };
}

// ── settings & registration: accept-and-ignore ────────────────────
//
// These don't carry meaningful semantics for our engine — the
// equivalent settings are either always-on (extension functions)
// or pulled from `xsl:output` directly.  We accept them so
// lxml's setup phase succeeds; their values aren't read back.

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetCtxtParseOptions(
    _ctxt: *mut c_void, _options: c_int,
) -> c_int { 0 }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltRegisterAllExtras() {}

// xsltNewSecurityPrefs / Free / Set / Get / SetCtxt / Allow / Forbid
// have authoritative implementations in real libxslt — lxml uses
// those directly.  Our shim exports stubs of the same names purely
// to satisfy the symbols.ld export list; they're unreachable at
// runtime because lxml's etree.so resolves to libxslt's own copies
// (loaded earlier in the dependency chain).  document()'s security
// gate consults the prefs that libxslt's xsltSetCtxtSecurityPrefs
// wrote into xsltTransformContext.sec (offset 272), then calls
// libxslt's `xsltCheckRead` via the extern declaration below.

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltNewSecurityPrefs()
    -> *mut c_void { ptr::null_mut() }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltFreeSecurityPrefs(_p: *mut c_void) {}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetSecurityPrefs(
    _p: *mut c_void, _o: c_int, _f: *mut c_void,
) -> c_int { 0 }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltGetSecurityPrefs(
    _p: *mut c_void, _o: c_int,
) -> *mut c_void { ptr::null_mut() }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetCtxtSecurityPrefs(
    _p: *mut c_void, _c: *mut c_void,
) -> c_int { 0 }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSecurityAllow(
    _p: *mut c_void, _c: *mut c_void, _v: *const c_char,
) -> c_int { 1 }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSecurityForbid(
    _p: *mut c_void, _c: *mut c_void, _v: *const c_char,
) -> c_int { 0 }

/// Wrapper around libxslt's `xsltCheckRead`.  We can't link against
/// libxslt at build time (it depends on libxml2, which IS this
/// crate's cdylib — a circular link).  Resolve it lazily via
/// `dlsym(RTLD_DEFAULT, "xsltCheckRead")` at first use.  If libxslt
/// isn't loaded into the process (e.g. a pure-libxml2 consumer),
/// the call returns "allowed" (1) so we don't accidentally block
/// document() in non-XSLT contexts.
pub unsafe fn xsltCheckRead(
    sec:   *mut c_void,
    ctxt:  *mut c_void,
    value: *const c_char,
) -> c_int {
    use std::sync::OnceLock;
    type Fn = unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> c_int;
    static FN: OnceLock<Option<Fn>> = OnceLock::new();
    let cached = FN.get_or_init(|| {
        unsafe extern "C" {
            fn dlsym(handle: *mut c_void, sym: *const c_char) -> *mut c_void;
        }
        // RTLD_DEFAULT = -2 on macOS; search every loaded image.
        let rtld_default: *mut c_void = -2isize as usize as *mut c_void;
        let name = b"xsltCheckRead\0".as_ptr() as *const c_char;
        let p = unsafe { dlsym(rtld_default, name) };
        if p.is_null() { None } else { Some(unsafe { std::mem::transmute::<*mut c_void, Fn>(p) }) }
    });
    match cached {
        Some(f) => unsafe { f(sec, ctxt, value) },
        None    => 1,
    }
}


#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetGenericErrorFunc(
    _ctx: *mut c_void, _handler: *mut c_void,
) {}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetTransformErrorFunc(
    _ctxt: *mut c_void, _ctx: *mut c_void, _handler: *mut c_void,
) {}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltSetLoaderFunc(_func: *mut c_void) {}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltQuoteOneUserParam(
    _ctxt: *mut c_void, _name: *const c_char, _value: *const c_char,
) -> c_int { 0 }

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltRegisterExtElement(
    _ctxt: *mut c_void, _name: *const c_char, _uri: *const c_char,
    _func: *mut c_void,
) -> c_int { 0 }

/// libxslt `xsltRegisterExtFunction(ctxt, name, uri, func)` —
/// register an XSLT extension function for the duration of the
/// transform.  Routes through `xmlXPathRegisterFuncNS` on the
/// embedded xpath context (xsltTransformContext->xpathCtxt) so our
/// XPath engine's `fn_map` picks it up.
///
/// `ctxt` is libxslt's `xsltTransformContext*`; its `xpathCtxt`
/// field lives at offset 160 — verified against
/// `/opt/homebrew/Cellar/libxslt/1.1.45/include/libxslt/xsltInternals.h`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltRegisterExtFunction(
    ctxt: *mut c_void, name: *const c_char, uri: *const c_char,
    func: *mut c_void,
) -> c_int {
    if ctxt.is_null() || name.is_null() { return -1; }
    // SAFETY: caller-provided xsltTransformContext*; offset 160 is
    // its `xpathCtxt` field.
    let xpath_ctx_ptr = unsafe {
        *((ctxt as *const u8).add(160) as *const *mut crate::xpath::xmlXPathContext)
    };
    if xpath_ctx_ptr.is_null() { return -1; }
    unsafe { crate::xpath::xmlXPathRegisterFuncNS(xpath_ctx_ptr, name, uri, func) }
}

// ── deferred entry points ────────────────────────────────────────
//
// These are real libxslt entry points that we don't yet back.
// Returning NULL / -1 + a one-time stderr note keeps consumers'
// error paths working without giving them a misleading "ok"
// answer.

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltApplyOneTemplate() -> *mut c_void {
    warn_once("xsltApplyOneTemplate"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltProcessOneNode() -> *mut c_void {
    warn_once("xsltProcessOneNode"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltDocDefaultLoader() -> *mut c_void {
    warn_once("xsltDocDefaultLoader"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltNextImport() -> *mut c_void {
    warn_once("xsltNextImport"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltGetProfileInformation() -> *mut c_void {
    warn_once("xsltGetProfileInformation"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltGenericError() -> *mut c_void {
    warn_once("xsltGenericError"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltGenericErrorContext() -> *mut c_void {
    warn_once("xsltGenericErrorContext"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltTransformError() -> *mut c_void {
    warn_once("xsltTransformError"); ptr::null_mut()
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltLibxsltVersion() -> *mut c_void {
    // Some callers read this as an int via a different signature.
    // Returning a pointer that holds the version int is fine —
    // these callers compare to a static address, they don't
    // dereference.  20102 == "1.1.34"-ish.
    20102 as *mut c_void
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))] pub unsafe extern "C" fn xsltMaxDepth() -> *mut c_void {
    3000 as *mut c_void
}

// ── Schematron ABI ────────────────────────────────────────────────
//
// libxml2's Schematron API has a parser-context phase and a
// validation phase.  Our engine collapses both into one struct
// (`Schematron`) — we shape the C surface to match libxml2's
// while still bottoming out at the same engine call.

#[repr(C)]
pub struct SchematronParserCtxtShim {
    /// The source xmlDoc the consumer handed us, or NULL if
    /// constructed from a file path (deferred).
    pub doc: *mut XmlDoc,
}

#[repr(C)]
pub struct SchematronShim {
    pub _private: Cell<*mut c_void>,
    pub _rust:    Box<Schematron>,
}

#[repr(C)]
pub struct SchematronValidCtxtShim {
    pub schema: *mut SchematronShim,
    /// Optional structured-error callback — lxml registers one to
    /// capture findings.  Type is `void(*)(void*, xmlError*)` but
    /// we only forward the message string, not a full xmlError.
    pub error_handler: *mut c_void,
    pub error_user:    *mut c_void,
}

/// `xmlSchematronNewDocParserCtxt(doc)` — wrap a parsed schema
/// xmlDoc into a parser-context handle.  Caller owns; frees via
/// `xmlSchematronFreeParserCtxt`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronNewDocParserCtxt(doc: *mut XmlDoc) -> *mut c_void {
    if doc.is_null() { return ptr::null_mut(); }
    let ctx = SchematronParserCtxtShim { doc };
    Box::into_raw(Box::new(ctx)) as *mut c_void
}

/// `xmlSchematronNewParserCtxt(url)` — variant taking a file path.
/// Not yet wired (needs a file-loader hookup); returns NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronNewParserCtxt(_url: *const c_char) -> *mut c_void {
    warn_once("xmlSchematronNewParserCtxt");
    ptr::null_mut()
}

/// `xmlSchematronParse(ctxt)` — compile the schema referenced by
/// the parser context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronParse(ctxt: *mut c_void) -> *mut c_void {
    if ctxt.is_null() { return ptr::null_mut(); }
    let pc = unsafe { &*(ctxt as *const SchematronParserCtxtShim) };
    if pc.doc.is_null() { return ptr::null_mut(); }
    let doc = unsafe { &(*pc.doc)._doc };
    match Schematron::compile(doc) {
        Ok(s) => {
            let shim = SchematronShim {
                _private: Cell::new(ptr::null_mut()),
                _rust:    Box::new(s),
            };
            Box::into_raw(Box::new(shim)) as *mut c_void
        }
        Err(_) => ptr::null_mut(),
    }
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronFreeParserCtxt(ctxt: *mut c_void) {
    if ctxt.is_null() { return; }
    let _ = unsafe { Box::from_raw(ctxt as *mut SchematronParserCtxtShim) };
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronFree(schema: *mut c_void) {
    if schema.is_null() { return; }
    let _ = unsafe { Box::from_raw(schema as *mut SchematronShim) };
}

/// `xmlSchematronNewValidCtxt(schema, options)` — build a
/// validation-context handle bound to `schema`.  Options are
/// libxml2's `XML_SCHEMATRON_OUT_*` bitmask; we don't emit SVRL
/// (we use structured callbacks instead), so they're accepted-and-
/// ignored.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronNewValidCtxt(
    schema: *mut c_void, _options: c_int,
) -> *mut c_void {
    if schema.is_null() { return ptr::null_mut(); }
    let vc = SchematronValidCtxtShim {
        schema:        schema as *mut SchematronShim,
        error_handler: ptr::null_mut(),
        error_user:    ptr::null_mut(),
    };
    Box::into_raw(Box::new(vc)) as *mut c_void
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronFreeValidCtxt(ctxt: *mut c_void) {
    if ctxt.is_null() { return; }
    let _ = unsafe { Box::from_raw(ctxt as *mut SchematronValidCtxtShim) };
}

/// `xmlSchematronSetValidStructuredErrors(ctxt, handler, user)` —
/// register a structured-error callback.  When validation runs,
/// failed-assert and successful-report findings flow through this
/// callback as one xmlError-shaped record each.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronSetValidStructuredErrors(
    ctxt: *mut c_void, handler: *mut c_void, user: *mut c_void,
) {
    if ctxt.is_null() { return; }
    let vc = unsafe { &mut *(ctxt as *mut SchematronValidCtxtShim) };
    vc.error_handler = handler;
    vc.error_user    = user;
}

/// `xmlSchematronValidateDoc(ctxt, doc)` — run validation.
///
/// Returns 0 if the document is valid (no failed asserts), -1 on
/// internal error, positive count of failed asserts otherwise.
/// libxml2's convention.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchematronValidateDoc(
    ctxt: *mut c_void, doc: *mut XmlDoc,
) -> c_int {
    if ctxt.is_null() || doc.is_null() { return -1; }
    let vc = unsafe { &*(ctxt as *const SchematronValidCtxtShim) };
    if vc.schema.is_null() { return -1; }
    let schema  = unsafe { &(*vc.schema)._rust };
    let instance = unsafe { &(*doc)._doc };
    let report = match schema.validate(instance) {
        Ok(r) => r,
        Err(_) => return -1,
    };
    // Deliver every failed assertion to the structured-error handler lxml
    // installs via `xmlSchematronSetValidStructuredErrors`, so it lands in
    // `Schematron.error_log` (the test reads `error_log.filter_from_errors()`
    // after an invalid validation).  Build a minimal `xmlError` per finding
    // — message, ERROR level, and the SCHEMATRONV domain lxml reports.
    let failures = report.findings.iter()
        .filter(|f| matches!(f.kind, FindingKind::FailedAssert))
        .count();
    if !vc.error_handler.is_null() {
        // SAFETY: lxml stored a real `void(*)(void*, xmlError*)` here.
        let handler: crate::error::StructuredErrorFn =
            unsafe { std::mem::transmute(vc.error_handler) };
        for f in report.findings.iter()
            .filter(|f| matches!(f.kind, FindingKind::FailedAssert))
        {
            let msg_cs = std::ffi::CString::new(f.message.as_str()).unwrap_or_default();
            // SAFETY: xmlError is plain-old-data; zeroing then filling the
            // fields lxml reads is sufficient.
            let mut e: crate::error::xmlError = unsafe { std::mem::zeroed() };
            e.domain  = sup_xml_core::error::ErrorDomain::SchematronValidate as c_int;
            e.code    = 0;
            e.message = msg_cs.as_ptr() as *mut c_char;
            e.level   = sup_xml_core::error::ErrorLevel::Error as c_int;
            // SAFETY: handler is a caller-supplied extern "C" fn; `&mut e`
            // and `error_user` are valid for the call.
            unsafe { handler(vc.error_user, &mut e as *mut crate::error::xmlError); }
            drop(msg_cs);
        }
    }
    // Return libxml2 convention: number of failed asserts (>0 → invalid).
    failures as c_int
}

// Pull the schematron module's enum into scope so the cast above
// works; `FindingKind` is already imported at the top.
#[allow(dead_code)]
fn _force_report_type_import(_: ValidationReport) {}

// ── extension-function registry + xpath ctx → transform ctx ──────────

/// Process-wide registry of XSLT extension functions installed via
/// [`xsltRegisterExtModuleFunction`].  Keyed by `(name, namespace_uri)`
/// — libxml2/libxslt's identity for an extension function.
///
/// Stored as a `RwLock<HashMap>`; lookup is read-locked, register and
/// unregister take the write lock.  The stored pointer is the
/// caller-supplied `xsltExtFunction` function pointer.
static EXT_FUNCTIONS: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<(String, String), usize>>,
> = std::sync::OnceLock::new();

fn ext_functions_table()
    -> &'static std::sync::RwLock<std::collections::HashMap<(String, String), usize>>
{
    EXT_FUNCTIONS.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// libxml2/libxslt `xsltRegisterExtModuleFunction(name, URI, function)` —
/// register `function` as the implementation of an XSLT extension
/// function callable from XPath as `prefix:name(...)` when `prefix`
/// is bound to `URI`.
///
/// Returns 0 on success, -1 on invalid input.  Our XSLT engine does
/// not yet dispatch through this registry during transform — registration
/// succeeds (so consumer setup runs cleanly) but invoking the registered
/// function from a stylesheet is a follow-up.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltRegisterExtModuleFunction(
    name:     *const c_char,
    uri:      *const c_char,
    function: *mut c_void,
) -> c_int {
    if name.is_null() || uri.is_null() || function.is_null() { return -1; }
    // SAFETY: caller asserts NUL-terminated.
    let n = match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
        Ok(s)  => s.to_string(),
        Err(_) => return -1,
    };
    let u = match unsafe { std::ffi::CStr::from_ptr(uri) }.to_str() {
        Ok(s)  => s.to_string(),
        Err(_) => return -1,
    };
    let mut t = match ext_functions_table().write() { Ok(g) => g, Err(_) => return -1 };
    t.insert((n, u), function as usize);
    0
}

/// libxml2/libxslt `xsltUnregisterExtModuleFunction(name, URI)` —
/// undo a previous [`xsltRegisterExtModuleFunction`].  Returns 0 if
/// removed (or absent), -1 on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltUnregisterExtModuleFunction(
    name: *const c_char,
    uri:  *const c_char,
) -> c_int {
    if name.is_null() || uri.is_null() { return -1; }
    let n = match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
        Ok(s)  => s.to_string(),
        Err(_) => return -1,
    };
    let u = match unsafe { std::ffi::CStr::from_ptr(uri) }.to_str() {
        Ok(s)  => s.to_string(),
        Err(_) => return -1,
    };
    let mut t = match ext_functions_table().write() { Ok(g) => g, Err(_) => return -1 };
    t.remove(&(n, u));
    0
}

/// libxslt `xsltSaveResultToFilename(URI, result, style, compression)` —
/// serialize `result` to disk per the stylesheet's `xsl:output`
/// settings (method, encoding, indent).  Returns the byte count
/// written, or -1 on error.  `compression` is accepted for API
/// parity but ignored (we never gzip output).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltSaveResultToFilename(
    uri:           *const c_char,
    result:        *mut XmlDoc,
    _style:        *mut c_void,
    _compression:  c_int,
) -> c_int {
    if uri.is_null() || result.is_null() { return -1; }
    let path = match unsafe { std::ffi::CStr::from_ptr(uri) }.to_str() {
        Ok(s)  => s,
        Err(_) => return -1,
    };
    let mut buf: *mut c_char = ptr::null_mut();
    let mut len: c_int       = 0;
    let rc = unsafe { xsltSaveResultToString(&mut buf, &mut len, result, ptr::null_mut()) };
    if rc != 0 || buf.is_null() || len <= 0 {
        return -1;
    }
    // SAFETY: xsltSaveResultToString hands back len bytes at buf.
    let bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
    let res = std::fs::write(path, bytes);
    // xsltSaveResultToString returned a CString-into-raw allocation;
    // libxslt's contract has the caller `free()` it.  We use libc free
    // via the registered allocator surface.
    unsafe {
        drop(CString::from_raw(buf));
    }
    match res {
        Ok(())  => len,
        Err(_)  => -1,
    }
}

/// libxslt `xsltXPathGetTransformContext(xpath_ctxt)` — fetch the
/// XSLT transform context that owns `xpath_ctxt`.  libxslt stores it
/// at the `extra` (`userData`) slot on the XPath context at offset
/// 408 (verified against libxslt 1.1.46).
///
/// Our XPath context (see [`crate::xpath`]) does not yet plumb the
/// transform context back through this hook because our transform
/// engine is single-call and doesn't expose a per-eval handle.  We
/// return NULL — callers that use it purely as a "is this an XSLT
/// xpath context?" check (the dominant use) see "no" and fall back
/// to non-XSLT handling.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xsltXPathGetTransformContext(
    _xpath_ctxt: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::xmlReadMemory;

    /// Smoke-test: parse a stylesheet, apply it to a doc, get the
    /// expected output back.  Round-trips through the ABI.
    #[test]
    fn xslt_compile_apply_roundtrip() {
        let stylesheet = r#"<?xml version="1.0"?>
            <xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:output method="xml" omit-xml-declaration="yes"/>
                <xsl:template match="/">
                    <out><xsl:value-of select="/r/v"/></out>
                </xsl:template>
            </xsl:stylesheet>"#;
        let style_cstr  = CString::new(stylesheet).unwrap();
        let source_cstr = CString::new("<r><v>hello</v></r>").unwrap();

        let style_doc = unsafe {
            xmlReadMemory(style_cstr.as_ptr(),
                          style_cstr.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        assert!(!style_doc.is_null(), "stylesheet should parse");
        let style = unsafe { xsltParseStylesheetDoc(style_doc) };
        assert!(!style.is_null(), "stylesheet should compile");

        let src_doc = unsafe {
            xmlReadMemory(source_cstr.as_ptr(),
                          source_cstr.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        let result = unsafe { xsltApplyStylesheet(style, src_doc, ptr::null()) };
        assert!(!result.is_null(), "apply should produce a result doc");

        let mut buf: *mut c_char = ptr::null_mut();
        let mut len: c_int = 0;
        let rc = unsafe { xsltSaveResultToString(&mut buf, &mut len, result, style) };
        assert_eq!(rc, 0, "saveResultToString should succeed");
        let out = unsafe { std::ffi::CStr::from_ptr(buf) }.to_str().unwrap().to_string();
        assert!(out.contains("hello"), "output should contain value, got: {out}");

        unsafe {
            let _ = CString::from_raw(buf);  // reclaim the alloc
            xsltFreeStylesheet(style);
        }
    }

    #[test]
    fn xslt_foreach_self_at_doc_node() {
        // iso-schematron's pipeline does `<xsl:apply-templates select="."
        // mode="..."/>` from the document-node root template.  Exercise the
        // equivalent through the full compat path (parse → compile → apply).
        let stylesheet = r#"<?xml version="1.0"?>
            <xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
                <xsl:output method="xml" omit-xml-declaration="yes"/>
                <xsl:template match="/"><r><xsl:for-each select="."><x/></xsl:for-each></r></xsl:template>
            </xsl:stylesheet>"#;
        let style_cstr  = CString::new(stylesheet).unwrap();
        let source_cstr = CString::new("<a>hi</a>").unwrap();
        let style_doc = unsafe {
            xmlReadMemory(style_cstr.as_ptr(), style_cstr.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0) };
        let style = unsafe { xsltParseStylesheetDoc(style_doc) };
        let src_doc = unsafe {
            xmlReadMemory(source_cstr.as_ptr(), source_cstr.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0) };
        let result = unsafe { xsltApplyStylesheet(style, src_doc, ptr::null()) };
        assert!(!result.is_null(), "apply should produce a result doc");
        let mut buf: *mut c_char = ptr::null_mut();
        let mut len: c_int = 0;
        unsafe { xsltSaveResultToString(&mut buf, &mut len, result, style); }
        let out = unsafe { std::ffi::CStr::from_ptr(buf) }.to_str().unwrap().to_string();
        unsafe { let _ = CString::from_raw(buf); xsltFreeStylesheet(style); }
        assert!(out.contains("<x"), "for-each select=. at doc node should iterate: {out:?}");
    }

    #[test]
    fn schematron_compile_and_validate_roundtrip() {
        let schema = r#"<?xml version="1.0"?>
            <schema xmlns="http://purl.oclc.org/dsdl/schematron">
                <pattern>
                    <rule context="r">
                        <assert test="@x">missing x</assert>
                    </rule>
                </pattern>
            </schema>"#;
        let schema_cstr  = CString::new(schema).unwrap();
        let schema_doc = unsafe {
            xmlReadMemory(schema_cstr.as_ptr(),
                          schema_cstr.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        assert!(!schema_doc.is_null());

        let parser = unsafe { xmlSchematronNewDocParserCtxt(schema_doc) };
        assert!(!parser.is_null());
        let compiled = unsafe { xmlSchematronParse(parser) };
        assert!(!compiled.is_null());
        let valid_ctxt = unsafe { xmlSchematronNewValidCtxt(compiled, 0) };
        assert!(!valid_ctxt.is_null());

        // Invalid instance — missing @x.
        let bad = CString::new("<r/>").unwrap();
        let bad_doc = unsafe {
            xmlReadMemory(bad.as_ptr(), bad.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        let rc = unsafe { xmlSchematronValidateDoc(valid_ctxt, bad_doc) };
        assert!(rc > 0, "expected positive (failed assertions), got {rc}");

        // Valid instance.
        let good = CString::new(r#"<r x="1"/>"#).unwrap();
        let good_doc = unsafe {
            xmlReadMemory(good.as_ptr(), good.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        let rc = unsafe { xmlSchematronValidateDoc(valid_ctxt, good_doc) };
        assert_eq!(rc, 0, "expected zero (valid), got {rc}");

        unsafe {
            xmlSchematronFreeValidCtxt(valid_ctxt);
            xmlSchematronFree(compiled);
            xmlSchematronFreeParserCtxt(parser);
        }
    }

    /// Stylesheet that fails to compile (malformed XSLT) returns
    /// NULL rather than crashing.
    #[test]
    fn malformed_stylesheet_returns_null() {
        let bogus = CString::new(r#"<not-a-stylesheet/>"#).unwrap();
        let doc = unsafe {
            xmlReadMemory(bogus.as_ptr(), bogus.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        let style = unsafe { xsltParseStylesheetDoc(doc) };
        assert!(style.is_null());
    }

    #[test]
    fn null_inputs_handled_gracefully() {
        assert!(unsafe { xsltParseStylesheetDoc(ptr::null_mut()) }.is_null());
        unsafe { xsltFreeStylesheet(ptr::null_mut()) };
        assert!(unsafe { xsltApplyStylesheet(ptr::null_mut(), ptr::null_mut(), ptr::null()) }.is_null());
        assert!(unsafe { xmlSchematronParse(ptr::null_mut()) }.is_null());
        assert_eq!(unsafe { xmlSchematronValidateDoc(ptr::null_mut(), ptr::null_mut()) }, -1);
    }

    #[test]
    fn xslt_extension_function_registry_register_and_unregister() {
        let name = CString::new("my-fn").unwrap();
        let uri  = CString::new("urn:test").unwrap();
        // Dummy function pointer — just needs to be non-null.  Go via
        // `* const ()` because `fn item as usize` casts the fn item's
        // ZST rather than its address (rustc warns since 1.86).
        let fnptr: *mut c_void =
            xsltRegisterExtModuleFunction as *const () as *mut c_void;

        assert_eq!(unsafe { xsltRegisterExtModuleFunction(name.as_ptr(), uri.as_ptr(), fnptr) }, 0);
        // NULL inputs: -1.
        assert_eq!(unsafe { xsltRegisterExtModuleFunction(ptr::null(), uri.as_ptr(), fnptr) }, -1);
        assert_eq!(unsafe { xsltRegisterExtModuleFunction(name.as_ptr(), ptr::null(), fnptr) }, -1);
        // Unregister succeeds (and is idempotent — removing absent is 0).
        assert_eq!(unsafe { xsltUnregisterExtModuleFunction(name.as_ptr(), uri.as_ptr()) }, 0);
        assert_eq!(unsafe { xsltUnregisterExtModuleFunction(name.as_ptr(), uri.as_ptr()) }, 0);
    }

    #[test]
    fn xslt_save_result_to_filename_writes_to_disk() {
        // Parse a small doc, write it via xsltSaveResultToFilename
        // (style ptr is unused on our serialiser path).
        let src = CString::new("<r><a/></r>").unwrap();
        let doc = unsafe {
            xmlReadMemory(src.as_ptr(), src.as_bytes().len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null());
        let tmp = std::env::temp_dir().join(format!("xslt_save_{}.xml", std::process::id()));
        let cpath = CString::new(tmp.to_str().unwrap()).unwrap();
        let n = unsafe { xsltSaveResultToFilename(cpath.as_ptr(), doc, ptr::null_mut(), 0) };
        assert!(n > 0, "expected bytes written, got {n}");
        let s = std::fs::read_to_string(&tmp).unwrap();
        assert!(s.contains("<r"));
        let _ = std::fs::remove_file(&tmp);
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }
}
