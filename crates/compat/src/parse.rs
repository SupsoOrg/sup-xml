//! Tier 1 parsing entry points.
//!
//! Implements the minimum function surface needed for T-PARSE-01:
//!
//!   - [`xmlReadMemory`] — parse from a byte buffer
//!   - [`xmlFreeDoc`] — release a parsed document
//!   - [`xmlDocGetRootElement`] — first element child of a document
//!   - [`xmlNodeGetContent`] — concatenated UTF-8 text under a node
//!   - [`xmlFree`] — release any pointer this crate handed out
//!
//! # Allocator pairing
//!
//! Pointers returned by [`xmlNodeGetContent`] are libc-`malloc`'d and
//! must be released via [`xmlFree`] (which calls libc `free`).
//! **Arena-resident pointers** (e.g. `node->name`, `attr->value` read
//! directly off the struct) live in the document arena and are reclaimed
//! by [`xmlFreeDoc`] — never pass those to [`xmlFree`].  Slice Tier1-D
//! will add address-range detection so the caller doesn't have to know
//! which kind they hold.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use std::sync::Arc;

use sup_xml_core::error::{ErrorCode, ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::entity_resolver::{EntityResolver, ResolveError};
use sup_xml_core::options::ParseOptions;
// parse_bytes_with_dtd is reached via the fully-qualified path
// inside xmlReadMemory so it stays adjacent to the dtd-stash call.
use sup_xml_tree::dom::{Attribute, Node, NodeKind, XmlDoc};

use crate::alloc::{alloc_registered_cstring, take_alloc};
use crate::error::record_last_error;

// ── parsing ────────────────────────────────────────────────────────────────

/// libxml2 `XML_PARSE_*` option bits.  Mirrors `xmlParserOption` from
/// libxml2's `parser.h`.  Only the bits we currently act on are
/// listed; the rest are accepted-and-ignored (consistent with our
/// "tier 1 minimum" stance — but bits that affect safety must be
/// honoured, and bits that change visible tree shape should be too
/// or the cdylib silently diverges from real libxml2).
const XML_PARSE_RECOVER:   c_int = 1 << 0;
const XML_PARSE_NOENT:     c_int = 1 << 1;
const XML_PARSE_DTDLOAD:   c_int = 1 << 2;
const XML_PARSE_DTDVALID:  c_int = 1 << 4;
const XML_PARSE_NOBLANKS:  c_int = 1 << 8;
const XML_PARSE_NOCDATA:   c_int = 1 << 14;

/// Translate a libxml2 `XML_PARSE_*` bitmask onto our
/// [`ParseOptions`].
///
/// The five bits wired here are the ones our parser actually has
/// `ParseOptions` fields for:
///
/// | libxml2 bit          | maps to                                 |
/// |----------------------|-----------------------------------------|
/// | `XML_PARSE_RECOVER`  | `recovery_mode`                         |
/// | `XML_PARSE_NOENT`    | `resolve_entities`                      |
/// | `XML_PARSE_DTDLOAD`  | `load_external_dtd`                     |
/// | `XML_PARSE_DTDVALID` | `validating`                            |
/// | `XML_PARSE_NOBLANKS` | `skip_inter_element_whitespace`         |
/// | `XML_PARSE_NOCDATA`  | `cdata_as_text`                         |
///
/// Other libxml2 bits (`NSCLEAN`, `XINCLUDE`, `HUGE`, …)
/// are silently accepted and ignored; they need either a new
/// `ParseOptions` field or a separate processing phase, and the
/// `options_audit` bench reports them as IGNORED.
///
/// Defaults divergence:
///
///   - `resolve_entities` mirrors `XML_PARSE_NOENT` directly: set when
///     the caller asks for substitution, cleared otherwise.  This
///     matches libxml2's documented default (NOENT-off leaves general
///     entity references in the tree as `XML_ENTITY_REF_NODE`s), which
///     is what `lxml`'s `XMLParser(resolve_entities=False)` relies on
///     to round-trip `&ent;` as an entity node.  Native (non-cdylib)
///     callers keep sup-xml's modern `resolve_entities: true` default
///     because they construct `ParseOptions` directly and never pass
///     through this translator.
///
/// Exposed publicly so the bench suite can audit per-flag translation
/// coverage against libxml2's actual behaviour (see
/// `crates/bench/benches/options_audit.rs`).  Most consumers reach
/// this indirectly via [`xmlReadMemory`] / [`xmlCtxtUseOptions`].
pub fn map_libxml2_options(bitmask: c_int, opts: &mut ParseOptions) {
    if (bitmask & XML_PARSE_RECOVER)  != 0 { opts.recovery_mode                 = true; }
    opts.resolve_entities = (bitmask & XML_PARSE_NOENT) != 0;
    if (bitmask & XML_PARSE_DTDLOAD)  != 0 { opts.load_external_dtd             = true; }
    if (bitmask & XML_PARSE_DTDVALID) != 0 { opts.validating                    = true; }
    if (bitmask & XML_PARSE_NOBLANKS) != 0 { opts.skip_inter_element_whitespace = true; }
    if (bitmask & XML_PARSE_NOCDATA)  != 0 { opts.cdata_as_text                 = true; }
}


/// libxml2 `xmlReadMemory(buffer, size, url, encoding, options)`.
///
/// Parses `size` bytes at `buffer` as an XML 1.0 document.  On success,
/// returns an owning pointer to a libxml2-shape document (release via
/// [`xmlFreeDoc`]).  On error, records the failure in the thread-local
/// last-error slot (inspect via `xmlGetLastError`) and returns NULL.
///
/// The `url` and `encoding` arguments are accepted but currently ignored
/// — sup-xml auto-detects encoding from the XML declaration or BOM, and
/// the URL is informational only.  Tier 1 widens this; for v0.1 the
/// "default options" path is what consumers get.
///
/// The `options` bitmask is the libxml2 `XML_PARSE_*` set; the bits
/// we currently honour are listed on [`map_libxml2_options`].  Most
/// importantly, external-DTD / external-entity loading is **off**
/// unless the caller sets `XML_PARSE_DTDLOAD` — matching libxml2's
/// documented default and preventing untrusted XML
/// referencing something like `<!ENTITY x SYSTEM "/etc/passwd">` as
/// a way to exfiltrate file contents.
///
/// # Safety
///
/// `buffer` must be a valid pointer to at least `size` readable bytes
/// when `size > 0`.  `url` / `encoding` must be NULL or NUL-terminated
/// C strings.
/// libxml2 `xmlReadFile(filename, encoding, options)` — slurp `filename`
/// into memory and parse it.  Returns NULL on I/O or parse failure;
/// the last-error slot is populated on either path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReadFile(
    filename: *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    if filename.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts NUL-terminated.
    let path = match unsafe { std::ffi::CStr::from_ptr(filename) }.to_str() {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => {
            // SAFETY: no context to mirror into on this entry point.
            unsafe { report_input_load_error(ptr::null_mut(), path); }
            return ptr::null_mut();
        }
    };
    unsafe {
        xmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            filename,
            encoding,
            options,
        )
    }
}

/// Record a libxml2-style "failed to load external entity" error for a
/// document input that could not be opened or read.  Mirrors libxml2's
/// `__xmlLoaderErr` — domain [`ErrorDomain::Io`], code
/// [`ErrorCode::IoLoadError`] — so a consumer inspecting the thread's
/// last error, or (when `ctxt` is non-NULL) the context's inline
/// `lastError`, can tell a missing file from a malformed document.
///
/// lxml's `_raiseParseError` keys on `ctxt->lastError.domain ==
/// XML_FROM_IO` to raise `OSError` instead of `XMLSyntaxError`, so the
/// ctxt mirror is what lets `etree.parse("nonexistent.xml")` surface as
/// an I/O error.
///
/// # Safety
///
/// `ctxt` must be NULL or a valid libxml2-layout parser context.
pub(crate) unsafe fn report_input_load_error(
    ctxt: *mut crate::parsectx::XmlParserCtxt,
    path: &str,
) {
    let err = XmlError::new(
        ErrorDomain::Io,
        ErrorLevel::Error,
        format!("failed to load external entity \"{path}\""),
    )
    .with_code(ErrorCode::IoLoadError);
    record_last_error(&err);
    if !ctxt.is_null() {
        // SAFETY: caller asserts `ctxt` is a valid parser context.
        unsafe { crate::parsectx::mirror_last_error_into_ctxt(ctxt); }
    }
}

/// libxml2 `xmlParseFile(filename)` — the legacy entry point that
/// predates `xmlReadFile`'s flags argument.  Equivalent to
/// `xmlReadFile(filename, NULL, 0)`; we delegate so the two share a
/// single path through the parser.  Returns NULL on I/O or parse
/// failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseFile(filename: *const c_char) -> *mut XmlDoc {
    unsafe { xmlReadFile(filename, ptr::null(), 0) }
}

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReadMemory(
    buffer:    *const c_char,
    size:      c_int,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
) -> *mut XmlDoc {
    // Route through the thread-local shared dict so every doc parsed
    // on this thread interns names into the same intern table.  That
    // makes cross-doc graft operations (e.g. lxml's
    // `moveNodeToDocument`) safe by construction — name pointers
    // stay valid as long as the dict is referenced by anyone.
    let dict = crate::dict::thread_dict();
    unsafe { xml_read_memory_with_dict(buffer, size, url, encoding, options, dict) }
}

/// Read an entire file descriptor into a byte buffer **without** taking
/// ownership of the fd — the caller retains it, matching libxml2's
/// `xmlReadFd` family contract.  Returns `None` on a negative fd or a
/// read error.
pub(crate) fn slurp_fd(fd: c_int) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;
    if fd < 0 {
        return None;
    }
    // ManuallyDrop so dropping the File doesn't close the caller's fd.
    let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(fd) });
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// libxml2 `xmlReadFd(fd, URL, encoding, options)` — parse a document
/// read in full from a file descriptor.  Slurps the fd (without closing
/// it) and routes through [`xmlReadMemory`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlReadFd(
    fd:       c_int,
    url:      *const c_char,
    encoding: *const c_char,
    options:  c_int,
) -> *mut XmlDoc {
    let Some(buf) = slurp_fd(fd) else { return ptr::null_mut() };
    unsafe { xmlReadMemory(buf.as_ptr() as *const c_char, buf.len() as c_int, url, encoding, options) }
}

/// Internal variant of [`xmlReadMemory`] that routes names through
/// `dict` when non-null.  Used by [`crate::parsectx::xmlCtxtReadMemory`]
/// to share the parser context's dict with the resulting document.
///
/// # Safety
///
/// `buffer` / `size` follow [`xmlReadMemory`]'s contract.  `dict`
/// must be NULL or a refcount-managed pointer per
/// [`sup_xml_tree::dict::Dict`].
pub unsafe fn xml_read_memory_with_dict(
    buffer:    *const c_char,
    size:      c_int,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
    dict:      *mut sup_xml_tree::dict::Dict,
) -> *mut XmlDoc {
    unsafe {
        xml_read_memory_with_dict_extras(
            buffer, size, url, encoding, options, dict, CtxtParseExtras::default())
    }
}

/// Per-context parse overrides the context-parse path threads into the
/// engine, sourced from the `xmlCtxtSet*` setters.  Each field is
/// `None` / `default` unless the consumer configured it.
#[derive(Default)]
pub(crate) struct CtxtParseExtras {
    /// External-entity resolver bridged from `xmlCtxtSetResourceLoader`.
    /// Installed as `ParseOptions::external_resolver` (its presence is
    /// the opt-in for external loading).
    pub resolver: Option<Arc<dyn EntityResolver>>,
    /// Entity-expansion byte cap from `xmlCtxtSetMaxAmplification`
    /// (computed as `input_size * maxAmpl`).  Overrides the default
    /// `ParseOptions::max_entity_expansion_bytes` when set.
    pub max_entity_expansion_bytes: Option<u64>,
    /// Drop comment / PI nodes — set when the consumer NULLed the
    /// `ctxt->sax->comment` / `processingInstruction` callbacks
    /// (lxml's `remove_comments` / `remove_pis`).
    pub remove_comments: bool,
    pub remove_pis: bool,
    /// The consumer installed a `getEntity` SAX callback restricting
    /// expansion to internal entities (lxml's `resolve_entities='internal'`
    /// default).  External *general* entities must then not be loaded —
    /// a reference to one is reported undefined, matching libxml2.
    pub restrict_external_entities: bool,
}

/// As [`xml_read_memory_with_dict`], applying any [`CtxtParseExtras`]
/// the context-parse path collected from the `xmlCtxtSet*` setters.
pub(crate) unsafe fn xml_read_memory_with_dict_extras(
    buffer:    *const c_char,
    size:      c_int,
    url:       *const c_char,
    encoding:  *const c_char,
    options:   c_int,
    dict:      *mut sup_xml_tree::dict::Dict,
    extras:    CtxtParseExtras,
) -> *mut XmlDoc {
    if buffer.is_null() || size <= 0 {
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "empty input")
            .with_code(ErrorCode::DocumentEmpty);
        record_last_error(&e);
        return ptr::null_mut();
    }
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(buffer as *const u8, size as usize)
    };
    // If the caller passed a source URL (xmlCtxtReadFile threads the
    // filename through), make it the base for resolving relative
    // SYSTEM literals during external-DTD loading.  Without this, a
    // file at `/path/to/doc.xml` referencing `<!DOCTYPE r SYSTEM
    // "schema.dtd">` would look for `$PWD/schema.dtd`.
    let base_url = if url.is_null() {
        None
    } else {
        // SAFETY: url is non-null per the check; caller asserts
        // NUL-terminated C string.
        unsafe { std::ffi::CStr::from_ptr(url) }.to_str().ok().map(|s| s.to_string())
    };
    let mut opts = ParseOptions {
        namespace_aware: true,
        base_url,
        ..ParseOptions::default()
    };
    map_libxml2_options(options, &mut opts);
    // A registered resource loader (xmlCtxtSetResourceLoader) is the
    // opt-in for external-entity / external-DTD loading: install it as
    // the resolver (presence = the permission, per the security model).
    // Failing that, globally-registered `xmlRegisterInputCallbacks`
    // handlers serve as the resolver — consulted only when the parse
    // flags already permit external loading.
    if extras.resolver.is_some() {
        opts.external_resolver = extras.resolver;
    } else if crate::input_callbacks::has_callbacks() {
        opts.external_resolver = Some(Arc::new(crate::input_callbacks::InputCallbackResolver));
    }
    // lxml's default `resolve_entities='internal'` installs a `getEntity`
    // SAX callback that loads only internal entities; external *general*
    // entities are then left undefined (a reference raises), even though
    // the resolver above is still used for the external DTD subset.  Honour
    // that opt-out so we don't load — and inline — attacker-controlled
    // external entities by default (XXE), matching libxml2.
    if extras.restrict_external_entities {
        opts.resolve_external_entities = false;
    }
    // Entity-expansion cap from xmlCtxtSetMaxAmplification.
    if let Some(cap) = extras.max_entity_expansion_bytes {
        opts.max_entity_expansion_bytes = cap;
    }
    // remove_comments / remove_pis, signalled by NULLed SAX callbacks.
    opts.remove_comments = extras.remove_comments;
    opts.remove_pis = extras.remove_pis;
    // Explicit `encoding` argument (xmlReadMemory / xmlReadFd / the
    // ctxt-parse path, incl. xmlSwitchEncodingName) overrides
    // auto-detection of the input encoding.
    if !encoding.is_null() {
        if let Ok(name) = unsafe { CStr::from_ptr(encoding) }.to_str() {
            if !name.is_empty() {
                opts.forced_encoding = Some(sup_xml_core::encoding::encoding_from_name(name));
            }
        }
    }
    let arena = crate::dict::new_doc_arena();
    let result = if dict.is_null() {
        // SAFETY: thread_dict returns a live, refcount-managed Dict.
        let td = crate::dict::thread_dict();
        unsafe { sup_xml_core::parser::parse_bytes_with_dtd_dict_arena(bytes, &opts, td, arena) }
    } else {
        // SAFETY: caller asserts dict is live with positive refcount.
        unsafe { sup_xml_core::parser::parse_bytes_with_dtd_dict_arena(bytes, &opts, dict, arena) }
    };
    match result {
        Ok((doc, dtd)) => {
            sup_xml_core::dtd::inject_defaults(&doc, &dtd);
            // DTD validation (XML_PARSE_DTDVALID / lxml's
            // `dtd_validation=True`): validate the parsed tree against its
            // DTD — loading the external subset named by the DOCTYPE's
            // SYSTEM identifier — and fail the parse (NULL, with the
            // violations in the error log) so lxml raises XMLSyntaxError.
            if opts.validating {
                if let Some(errors) = validate_against_dtd(
                    &doc, &dtd, opts.base_url.as_deref(), opts.external_resolver.as_ref(),
                ) {
                    for e in &errors {
                        record_last_error(e);
                    }
                    return ptr::null_mut();
                }
            }
            let raw = doc.into_xml_doc();
            plant_doc_url(raw, url);
            // libxml2 convention: doc->encoding is NULL when the
            // source had no `<?xml encoding="…"?>` declaration.
            // Serializers (libxslt's xsltSaveResultToString, etc.)
            // omit the encoding attribute on output in that case.
            // Our Rust-side `Document::encoding` defaults to empty;
            // when the parser didn't see a declaration, we need to
            // NULL out the C-shape pointer at offset 112 so
            // libxml2-ABI consumers see the expected sentinel.
            unsafe {
                let enc_ptr = (raw as *mut u8).add(112) as *mut *const c_char;
                let current = *enc_ptr;
                if !current.is_null() {
                    // Read the first byte; if NUL, the string is
                    // empty → treat as undeclared.
                    if *(current as *const u8) == 0 {
                        *enc_ptr = ptr::null();
                    }
                }
            }
            // Attach the internal-subset record whenever the parser
            // saw a <!DOCTYPE …> — even if its body was empty.  The
            // root name comes from the doctype header, not from the
            // first element decl.  When neither a doctype nor any
            // decls were present, leave intSubset NULL.
            let has_doctype = !dtd.root_name.is_empty();
            if has_doctype || !dtd.is_empty() {
                let dtd_name: &str = if !dtd.root_name.is_empty() {
                    &dtd.root_name
                } else {
                    dtd.elements.keys().next().map(|s| s.as_str()).unwrap_or("")
                };
                let cname    = std::ffi::CString::new(dtd_name).unwrap_or_default();
                let cpublic  = dtd.public_id.as_deref()
                    .and_then(|s| std::ffi::CString::new(s).ok());
                let csystem  = dtd.system_id.as_deref()
                    .and_then(|s| std::ffi::CString::new(s).ok());
                let public_ptr = cpublic.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null());
                let system_ptr = csystem.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null());
                unsafe {
                    let subset = crate::dtd::xmlCreateIntSubset(
                        raw, cname.as_ptr(), public_ptr, system_ptr,
                    );
                    // Materialize the parsed declarations as libxml2-shaped
                    // typed child nodes: lxml's DTD object model reads them,
                    // and the DOCTYPE serializer reconstructs the `[ … ]`
                    // body from them (matching libxml2, which likewise
                    // reconstructs rather than preserving source text).
                    crate::dtddecl::materialize(subset, raw as *mut std::os::raw::c_void, &dtd);
                    // Place the internal-subset node at its true
                    // document position so a comment/PI that preceded
                    // the `<!DOCTYPE>` serializes before it.
                    crate::dtd::splice_int_subset_into_prolog(
                        raw, dtd.internal_subset_prolog_index,
                    );
                }
                crate::dtd::stash_dtd(raw, dtd);
            }
            raw
        }
        Err(e) => {
            record_last_error(&e);
            ptr::null_mut()
        }
    }
}

/// Validate `doc` against its DTD for `XML_PARSE_DTDVALID`.  Loads the
/// external subset named by the DOCTYPE's SYSTEM identifier (resolved
/// against `base`) and merges it with any internal-subset declarations
/// before validating.  Returns `Some(errors)` — in document order — when
/// the parse should fail (a DTD that cannot be loaded, or a content /
/// attribute violation); `None` when the document is valid.
fn validate_against_dtd(
    doc:      &sup_xml_tree::dom::Document,
    dtd:      &sup_xml_core::dtd::Dtd,
    base:     Option<&str>,
    resolver: Option<&Arc<dyn EntityResolver>>,
) -> Option<Vec<XmlError>> {
    let mut combined = dtd.clone();
    if let Some(sys) = dtd.system_id.clone() {
        // Load the external subset the same way the parser does: through
        // the configured resolver (lxml installs one on every parser,
        // including a filesystem-backed default), or a direct filesystem
        // read when none is set.  A DTD that can't be loaded is a fatal
        // validation error — libxml2 reports the SYSTEM identifier
        // verbatim, and lxml's callers filter the error log by it.
        // Resolve a relative SYSTEM identifier against the base URI
        // before loading, exactly as the parser does for external
        // entities — the resolver expects the absolute URI.
        let resolved = sup_xml_core::resolve_uri(&sys, base);
        let bytes: Option<Vec<u8>> = match resolver {
            Some(r) => r.resolve(dtd.public_id.as_deref(), &resolved, base).ok(),
            None => {
                let path = resolved.strip_prefix("file://").unwrap_or(&resolved);
                std::fs::read(path).ok()
            }
        };
        match bytes {
            Some(bytes) => {
                let popts = ParseOptions::default();
                if let Ok(ext) = sup_xml_core::parser::parse_external_subset(&bytes, &popts) {
                    merge_external_dtd(&mut combined, &ext);
                }
            }
            None => {
                return Some(vec![XmlError::new(
                    ErrorDomain::Validation,
                    ErrorLevel::Fatal,
                    format!("failed to load external DTD \"{sys}\""),
                )
                .with_code(ErrorCode::IoLoadError)]);
            }
        }
    }
    match sup_xml_core::dtd::validate(doc, &combined) {
        Ok(()) => None,
        Err(errors) => Some(
            errors
                .iter()
                .map(|e| XmlError::new(ErrorDomain::Validation, ErrorLevel::Error, e.to_string()))
                .collect(),
        ),
    }
}

/// Fold an external subset's declarations into `into` for validation.
/// Internal-subset declarations take precedence (XML 1.0 § 2.8), so an
/// element already declared internally is left untouched; attribute
/// lists append, matching libxml2's merge.
fn merge_external_dtd(into: &mut sup_xml_core::dtd::Dtd, ext: &sup_xml_core::dtd::Dtd) {
    for name in &ext.element_order {
        if !into.elements.contains_key(name) {
            if let Some(decl) = ext.elements.get(name) {
                into.add_element(decl.clone());
            }
        }
    }
    for (elem, atts) in &ext.attlists {
        into.attlists.entry(elem.clone()).or_default().extend(atts.iter().cloned());
    }
}

/// Record `url` in `doc->URL` (offset 136) so consumers walking
/// the libxml2-shape document — `lxml`'s `docinfo.URL`,
/// `root.base`, anything that resolves relative URIs — see the
/// source URL the caller passed to a parse entry point.
///
/// Stored as a leaked CString; `XmlDoc::free` reclaims it.  No-op
/// for NULL `doc` or NULL `url`.
pub(crate) fn plant_doc_url(doc: *mut XmlDoc, url: *const c_char) {
    if doc.is_null() || url.is_null() {
        return;
    }
    // SAFETY: url is non-null per the check; caller asserted NUL-terminated.
    let s = match unsafe { std::ffi::CStr::from_ptr(url).to_str() } {
        Ok(s) => s,
        Err(_) => return,
    };
    let cs = match std::ffi::CString::new(s) {
        Ok(c) => c,
        Err(_) => return,
    };
    // SAFETY: `doc` is a live XmlDoc returned by into_xml_doc.
    // Writing the url field is a raw pointer assignment to a slot
    // designed for `xmlChar*` ownership; the CString::into_raw
    // pointer is reclaimed in `XmlDoc::free`.
    unsafe { (*doc).url = cs.into_raw(); }
}

/// libxml2 `xmlFreeDoc`.  Reclaim a document returned by
/// `xmlReadMemory`.  NULL-safe (no-op).
///
/// After this returns, every pointer derived from the document
/// (element names, attribute values, content strings reachable from
/// the tree) becomes dangling — caller must not retain them.
///
/// # Safety
///
/// `doc` must be NULL or a pointer returned by [`xmlReadMemory`] that
/// has not been freed.  Double-free is undefined behavior.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeDoc(doc: *mut XmlDoc) {
    if !doc.is_null() {
        crate::dtd::forget_dtd(doc);
        crate::idindex::free_doc_id_table(doc);
    }
    unsafe { XmlDoc::free(doc); }
}

// ── tree walking (minimum needed by T-PARSE-01) ─────────────────────────────

/// libxml2 `xmlDocGetRootElement`.  Returns the first child of `doc`
/// whose kind is `XML_ELEMENT_NODE`, or NULL if no element child exists
/// (or `doc` is NULL).
///
/// libxml2 documents may have non-element prelude/epilogue siblings
/// (comments, PIs, the DTD) — this walks past them to the element root.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDocGetRootElement(doc: *const XmlDoc) -> *mut Node<'static> {
    if doc.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: doc is non-null per the check above; lifetime tied to
    // caller's ownership of the XmlDoc allocation.
    let d = unsafe { &*doc };
    let mut cur: *mut Node<'static> = d.children.get();
    while !cur.is_null() {
        // SAFETY: cur is a non-null pointer into the document's arena.
        let n: &Node<'static> = unsafe { &*cur };
        if matches!(n.kind, NodeKind::Element) {
            return cur;
        }
        cur = match n.next_sibling.get() {
            Some(s) => s as *const Node<'_> as *mut Node<'static>,
            None    => ptr::null_mut(),
        };
    }
    ptr::null_mut()
}

/// libxml2 `xmlNodeGetContent`.  Returns a newly-allocated UTF-8
/// NUL-terminated string containing the concatenated text content of
/// `node` and its descendants:
///
/// - Text and CDATA nodes contribute their content verbatim.
/// - Element nodes recurse into their children.
/// - Comments, PIs, and attribute nodes contribute nothing.
///
/// Returns NULL if `node` is NULL.  The returned pointer must be
/// released via [`xmlFree`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeGetContent(node: *const Node<'static>) -> *mut c_char {
    if node.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts node is a valid pointer into a live doc.
    let n: &Node<'static> = unsafe { &*node };
    let mut buf = Vec::<u8>::new();
    if matches!(n.kind, NodeKind::Attribute) {
        // XPath nodesets returned by xmlXPathCompiledEval include
        // attribute nodes via the shared *xmlNode* pointer typing
        // (libxml2 ABI lets callers walk a mixed nodeTab through
        // xmlNode*).  But Node and Attribute diverge after offset 72
        // (Node has `content`/`first_attribute`; Attribute has
        // `atype`/`psvi`).  Re-view as the real type before walking
        // children so we don't accidentally read past the shared
        // window — and so the intent is explicit at the call site,
        // matching libxml2's own xmlNodeGetContent which switches on
        // node->type.
        let attr: &Attribute<'static> = unsafe { &*(node as *const Attribute<'static>) };
        let mut child = attr.children.get();
        while let Some(c) = child {
            collect_content(c, &mut buf);
            child = c.next_sibling.get();
        }
    } else {
        collect_content(n, &mut buf);
    }
    // Registered alloc so xmlFree can distinguish from arena pointers.
    alloc_registered_cstring(&buf)
}

fn collect_content(n: &Node<'_>, out: &mut Vec<u8>) {
    match n.kind {
        NodeKind::Text | NodeKind::CData => {
            out.extend_from_slice(n.content().as_bytes());
        }
        // Element subtrees and document / fragment containers concatenate
        // the text of all descendants — the latter matters for libxslt
        // result-tree fragments (an `xsl:variable`/param with a body),
        // whose root node is a document/fragment: without recursing here
        // their string-value would be empty, so `$rtf = 'x'` /
        // `normalize-space($rtf)` over an RTF variable would misbehave.
        NodeKind::Element | NodeKind::Document | NodeKind::DocumentFragment => {
            for c in n.children() {
                collect_content(c, out);
            }
        }
        _ => {}
    }
}

// ── allocator ──────────────────────────────────────────────────────────────

/// libxml2 `xmlFree`.  Release a pointer returned by an allocating
/// libxml2 API.  NULL-safe.  Safe (silent no-op) when called on an
/// arena-resident pointer such as `node->name` or `attr->value` —
/// historical libxml2 contract.
///
/// Discrimination uses the global allocator registry built in
/// [`crate::alloc`]: every pointer we hand out via
/// [`xmlNodeGetContent`] / `xmlGetProp` is registered; xmlFree
/// removes-and-releases only registered addresses.  Anything else
/// is left alone (it's owned by the document arena and will be
/// reclaimed by [`xmlFreeDoc`]).
///
/// # Safety
///
/// `ptr` must be NULL, a pointer returned by an allocating function
/// in this crate, or a pointer into a live document arena.  Pointers
/// from arbitrary other sources (e.g. caller's `malloc`) have
/// undefined behavior because we won't recognize them and won't free
/// them.
/// Internal implementation of libxml2's `xmlFree`.  Exposed to other
/// modules in this crate as a regular Rust function so internal
/// callers don't have to round-trip through the fn-ptr global below.
///
/// The exported C symbol is `xmlFree` (a `static mut` fn-ptr variable
/// further down) so consumers compiled to dereference it as a
/// function-pointer global — libxslt is one — dispatch correctly.
pub unsafe extern "C" fn xml_free_impl(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // Check the binary-safe registry first — these allocations may
    // contain interior NULs (UTF-16 buffers, raw byte dumps) so
    // CString::from_raw would size them off `strlen` and leak.
    if let Some(total) = crate::alloc::take_binary_alloc(ptr as *const u8) {
        // SAFETY: pointer was produced by Box::into_raw of a
        // Box<[u8]> of length `total` in alloc_registered_buffer.
        unsafe {
            let _ = Box::from_raw(std::slice::from_raw_parts_mut(ptr as *mut u8, total));
        }
        return;
    }
    if take_alloc(ptr as *const u8) {
        // SAFETY: take_alloc returned true, which means we'd previously
        // registered this exact pointer from one of the libc-backed
        // registry allocators.  Release it through libc free for
        // allocator symmetry (see `alloc::registry_free`).
        unsafe { crate::alloc::registry_free(ptr); }
    }
    // else: not ours — treat as arena pointer, silent no-op.
}

/// libxml2 `xmlFree` is documented in the public header as a
/// `xmlFreeFunc` variable, not a function:
/// ```c
/// XMLPUBVAR xmlFreeFunc xmlFree;
/// ```
/// Consumers like libxslt dispatch through it with `ldr+blr`; treating
/// the symbol as a function (so a `bl` jumps straight into our code)
/// reads the first instruction as a fn-pointer and crashes.  Expose it
/// as a fn-ptr global initialised to our implementation.
// Camel-cased to mirror libxml2's `xmlFree`; allow the lint
// explicitly now that `#[no_mangle]` is feature-gated (it used to
// suppress the warning implicitly).
#[allow(non_upper_case_globals)]
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static mut xmlFree: unsafe extern "C" fn(*mut c_void) = xml_free_impl;

/// `xmlParserGetDirectory(filename)` — return the parent directory of
/// `filename` as a freshly heap-allocated string (caller `xmlFree`s).
/// NULL on NULL input.  Used by parsers to set the base URL for
/// resolving relative entity references.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParserGetDirectory(filename: *const c_char) -> *mut c_char {
    if filename.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated readable.
    let s = match unsafe { CStr::from_ptr(filename) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let path = std::path::Path::new(s);
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        // No parent (just a bare filename) → libxml2 returns "./"
        _ => return crate::alloc::alloc_registered_cstring(b"./"),
    };
    let dir_str = dir.to_string_lossy();
    crate::alloc::alloc_registered_cstring(dir_str.as_bytes())
}

/// libxml2 `xmlParserInput` — slimmed-down layout matching the
/// public-header byte offsets through `consumed`.  Real libxml2's
/// struct has additional fields after `consumed` (free callback,
/// encoding, version, standalone, id) but libxslt 2.13+ only
/// reads / writes the prefix; we leave the tail unallocated.
///
/// `[u8; …]` filler past `consumed` keeps callers that read
/// further offsets inside our allocation rather than off the end.
#[repr(C)]
struct XmlParserInput {
    /// `buf` — would normally be an `xmlParserInputBuffer*`.
    /// We don't need a buffer object since our parser reads
    /// from `base..end` directly.  Always NULL.
    pub buf:        *mut std::os::raw::c_void,                //   0
    /// Filename (the URL the input was loaded from), owned C
    /// string.  Freed in [`xmlFreeInputStream`].
    pub filename:   *mut c_char,                              //   8
    pub directory:  *mut c_char,                              //  16
    pub base:       *const u8,                                //  24
    pub cur:        *const u8,                                //  32
    pub end:        *const u8,                                //  40
    pub length:     c_int,                                    //  48
    pub line:       c_int,                                    //  52
    pub col:        c_int,                                    //  56
    _pad_col:       c_int,                                    //  60
    pub consumed:   u64,                                      //  64
    /// Padding to ~104 bytes (libxml2's full struct).  Any
    /// libxslt read beyond `consumed` lands here rather than
    /// off-buffer; zero is a safe default for the remaining
    /// fields (free callback, encoding, version, standalone, id).
    _tail:          [u8; 64],                                 //  72..136
    /// Tail bytes we own and must reclaim: the heap-allocated
    /// byte vector backing `base/cur/end` and the filename
    /// CString.  Sit OUTSIDE the libxml2-shape ABI window.
    pub _bytes_box: *mut Vec<u8>,
}

/// `xmlLoadExternalEntity(url, id, ctx)` — load `url` as an XML
/// parser input.  Real libxslt calls this to fetch `xsl:import`
/// and `xsl:include` referenced files, then feeds the returned
/// input through [`xmlCtxtParseDocument`].
///
/// Implementation: open the file at `url` (filesystem only —
/// network URLs are skipped, matching libxslt's default XXE
/// stance), read the bytes, wrap in an [`XmlParserInput`].
/// Returns NULL on read failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLoadExternalEntity(
    url: *const c_char,
    _id: *const c_char,
    _ctx: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    if url.is_null() { return ptr::null_mut(); }
    let url_str = match unsafe { std::ffi::CStr::from_ptr(url) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    // Strip an optional `file://` prefix; treat everything else
    // as a local path (network resolution is intentionally
    // off-by-default).
    let path = url_str.strip_prefix("file://").unwrap_or(url_str);
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return ptr::null_mut(),
    };
    let bytes_box = Box::into_raw(Box::new(bytes));
    let (base, end) = unsafe {
        let v = &*bytes_box;
        let base = v.as_ptr();
        let end  = base.add(v.len());
        (base, end)
    };
    let filename = std::ffi::CString::new(url_str).unwrap().into_raw();
    let input = XmlParserInput {
        buf:        ptr::null_mut(),
        filename,
        directory:  ptr::null_mut(),
        base, cur: base, end,
        length: unsafe { (*bytes_box).len() } as c_int,
        line:  1, col: 1, _pad_col: 0,
        consumed: 0,
        _tail: [0; 64],
        _bytes_box: bytes_box,
    };
    Box::into_raw(Box::new(input)) as *mut std::os::raw::c_void
}

/// `xmlCtxtParseDocument(ctx, input)` — parse `input`'s byte range
/// through the engine, return the resulting xmlDoc.  Matches the
/// libxml2 v2.13+ entry point that libxslt's `xsltDocDefaultLoader`
/// uses for `xsl:import` / `xsl:include` resolution.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtParseDocument(
    _ctx:  *mut crate::parsectx::XmlParserCtxt,
    input: *mut std::os::raw::c_void,
) -> *mut XmlDoc {
    if input.is_null() { return ptr::null_mut(); }
    let inp = unsafe { &*(input as *const XmlParserInput) };
    if inp.base.is_null() || inp.end.is_null() { return ptr::null_mut(); }
    let len = unsafe { inp.end.offset_from(inp.base) } as usize;
    let url_cstr = if inp.filename.is_null() { ptr::null() } else { inp.filename };
    unsafe {
        xmlReadMemory(inp.base as *const c_char, len as c_int, url_cstr, ptr::null(), 0)
    }
}

/// `xmlFreeInputStream(input)` — reclaim an input allocated by
/// [`xmlLoadExternalEntity`].  Safe on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeInputStream(input: *mut std::os::raw::c_void) {
    if input.is_null() { return; }
    let inp = unsafe { Box::from_raw(input as *mut XmlParserInput) };
    if !inp.filename.is_null() {
        let _ = unsafe { std::ffi::CString::from_raw(inp.filename) };
    }
    if !inp._bytes_box.is_null() {
        let _ = unsafe { Box::from_raw(inp._bytes_box) };
    }
}

/// Build a heap [`XmlParserInput`] owning `bytes`, tagged with the
/// optional source `url`.  Returned as an opaque `*mut c_void` (the
/// `xmlParserInput*` ABI shape).  Consumed by [`xmlCtxtParseDocument`]
/// and reclaimed by [`xmlFreeInputStream`] — the caller owns it (this
/// shim's `xmlCtxtParseDocument` does not take ownership; see its docs).
pub(crate) fn new_input_from_bytes(bytes: Vec<u8>, url: *const c_char) -> *mut c_void {
    let bytes_box = Box::into_raw(Box::new(bytes));
    let (base, end, len) = unsafe {
        let v = &*bytes_box;
        let base = v.as_ptr();
        (base, base.add(v.len()), v.len())
    };
    let filename = if url.is_null() {
        ptr::null_mut()
    } else {
        match unsafe { std::ffi::CStr::from_ptr(url) }.to_str() {
            Ok(s)  => std::ffi::CString::new(s).map_or(ptr::null_mut(), |c| c.into_raw()),
            Err(_) => ptr::null_mut(),
        }
    };
    let input = XmlParserInput {
        buf: ptr::null_mut(),
        filename,
        directory: ptr::null_mut(),
        base, cur: base, end,
        length: len as c_int,
        line: 1, col: 1, _pad_col: 0,
        consumed: 0,
        _tail: [0; 64],
        _bytes_box: bytes_box,
    };
    Box::into_raw(Box::new(input)) as *mut c_void
}

/// An empty [`XmlParserInput`] (all spans NULL).  libxml2's
/// `xmlNewInputStream` hands one back for the caller to populate — lxml's
/// pre-2.14 resolver path does exactly this, writing `base`/`cur`/`end`/
/// `length`/`filename` directly.  Owns no bytes (`_bytes_box` NULL).
pub(crate) fn new_empty_input() -> *mut c_void {
    let input = XmlParserInput {
        buf: ptr::null_mut(),
        filename: ptr::null_mut(),
        directory: ptr::null_mut(),
        base: ptr::null(), cur: ptr::null(), end: ptr::null(),
        length: 0,
        line: 1, col: 1, _pad_col: 0,
        consumed: 0,
        _tail: [0; 64],
        _bytes_box: ptr::null_mut(),
    };
    Box::into_raw(Box::new(input)) as *mut c_void
}

/// Copy the bytes a *consumer-populated* [`XmlParserInput`] spans (one
/// produced by `xmlNewInputStream` then filled in by an external-entity
/// loader), then reclaim the input struct.  Unlike [`take_input_bytes`],
/// this frees `filename` via the registered allocator (`xmlStrdup`'s) and
/// never touches `base` — which points at memory the loader owns (e.g.
/// lxml's Python bytes), not ours.
pub(crate) unsafe fn loader_input_take_bytes(input: *mut c_void) -> Option<Vec<u8>> {
    if input.is_null() {
        return None;
    }
    // SAFETY: the loader built this via xmlNewInputStream (our Box).
    let boxed = unsafe { Box::from_raw(input as *mut XmlParserInput) };
    let bytes = if boxed.base.is_null() {
        None
    } else {
        let len = if !boxed.end.is_null() && boxed.end >= boxed.base {
            unsafe { boxed.end.offset_from(boxed.base) as usize }
        } else {
            boxed.length.max(0) as usize
        };
        Some(unsafe { std::slice::from_raw_parts(boxed.base, len) }.to_vec())
    };
    if !boxed.filename.is_null() {
        // Loader set this via xmlStrdup → registered allocator; free to match.
        unsafe { xml_free_impl(boxed.filename as *mut c_void); }
    }
    // `base` is the loader's memory; `_bytes_box` is NULL for this path.
    bytes
}

/// libxml2 `xmlNewInputFromMemory(url, mem, size, flags)` — wrap an
/// in-memory buffer as a parser input for [`xmlCtxtParseDocument`].
/// The bytes are copied, so the caller's buffer need not outlive the
/// input.  Free with [`xmlFreeInputStream`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromMemory(
    url:    *const c_char,
    mem:    *const c_void,
    size:   usize,
    _flags: c_int,
) -> *mut c_void {
    if mem.is_null() {
        return ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size) }.to_vec();
    new_input_from_bytes(bytes, url)
}

/// libxml2 `xmlNewInputFromString(url, str, flags)` — wrap a
/// NUL-terminated string as a parser input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromString(
    url:    *const c_char,
    string: *const c_char,
    _flags: c_int,
) -> *mut c_void {
    if string.is_null() {
        return ptr::null_mut();
    }
    let bytes = unsafe { std::ffi::CStr::from_ptr(string) }.to_bytes().to_vec();
    new_input_from_bytes(bytes, url)
}

/// libxml2 `xmlNewInputFromFd(url, fd, flags)` — read the entire file
/// descriptor (without closing it) and wrap it as a parser input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromFd(
    url:    *const c_char,
    fd:     c_int,
    _flags: c_int,
) -> *mut c_void {
    let Some(bytes) = slurp_fd(fd) else { return ptr::null_mut() };
    new_input_from_bytes(bytes, url)
}

/// libxml2 `xmlNewInputFromIO(url, ioRead, ioClose, ioCtxt, flags)` —
/// drive the caller's read callback to EOF, then wrap the collected
/// bytes.  `ioClose` (when non-NULL) is invoked once reading finishes.
/// Returns NULL if `ioRead` is NULL or the callback reports an error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromIO(
    url:      *const c_char,
    io_read:  Option<crate::reader::XmlInputReadCallback>,
    io_close: Option<crate::reader::XmlInputCloseCallback>,
    io_ctxt:  *mut c_void,
    _flags:   c_int,
) -> *mut c_void {
    let Some(read) = io_read else { return ptr::null_mut() };
    let mut bytes = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        let n = unsafe { read(io_ctxt, scratch.as_mut_ptr() as *mut c_char, scratch.len() as c_int) };
        if n <= 0 {
            if n < 0 {
                if let Some(close) = io_close { unsafe { close(io_ctxt); } }
                return ptr::null_mut();
            }
            break;
        }
        let n = (n as usize).min(scratch.len());
        bytes.extend_from_slice(&scratch[..n]);
    }
    if let Some(close) = io_close { unsafe { close(io_ctxt); } }
    new_input_from_bytes(bytes, url)
}

/// libxml2 `xmlNewInputFromUrl(url, flags, out)` — load a (file) URL
/// and write the resulting input through `*out`.  Returns `XML_ERR_OK`
/// (0) on success, non-zero otherwise.  Network URLs are not loaded
/// (XXE-safe default), matching [`xmlLoadExternalEntity`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewInputFromUrl(
    url:    *const c_char,
    _flags: c_int,
    out:    *mut *mut c_void,
) -> c_int {
    if out.is_null() {
        return 1;
    }
    unsafe { *out = ptr::null_mut(); }
    if url.is_null() {
        return 1;
    }
    let url_str = match unsafe { std::ffi::CStr::from_ptr(url) }.to_str() {
        Ok(s)  => s,
        Err(_) => return 1,
    };
    let path = url_str.strip_prefix("file://").unwrap_or(url_str);
    let bytes = match std::fs::read(path) {
        Ok(b)  => b,
        Err(_) => return 1,
    };
    unsafe { *out = new_input_from_bytes(bytes, url); }
    0
}

/// libxml2 `xmlResourceLoader` callback —
/// `xmlParserErrors (*)(void *ctxt, const char *url, const char *publicId,
/// xmlResourceType type, xmlParserInputFlags flags, xmlParserInput **out)`.
pub(crate) type XmlResourceLoader = unsafe extern "C" fn(
    ctxt:      *mut c_void,
    url:       *const c_char,
    public_id: *const c_char,
    typ:       c_int,
    flags:     c_int,
    out:       *mut *mut c_void,
) -> c_int;

/// Copy out the bytes an [`XmlParserInput`] (as produced by the
/// `xmlNewInputFrom*` family) spans, then free the input.  Returns
/// `None` for a NULL/empty/malformed input.
pub(crate) unsafe fn take_input_bytes(input: *mut c_void) -> Option<Vec<u8>> {
    if input.is_null() {
        return None;
    }
    let (base, end) = {
        let inp = unsafe { &*(input as *const XmlParserInput) };
        (inp.base, inp.end)
    };
    if base.is_null() || end.is_null() || end < base {
        unsafe { xmlFreeInputStream(input); }
        return None;
    }
    let len = unsafe { end.offset_from(base) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(base, len) }.to_vec();
    unsafe { xmlFreeInputStream(input); }
    Some(bytes)
}

/// Bridges a libxml2 `xmlResourceLoader` C callback to our
/// [`EntityResolver`] trait, so the parse engine can drive a
/// consumer-supplied loader for external DTDs / entities.
pub(crate) struct CResourceLoaderResolver {
    pub loader: XmlResourceLoader,
    /// The consumer's context, stored as `usize` so the resolver is
    /// `Send`/`Sync`; only ever handed back to the consumer's callback.
    pub vctxt:  usize,
}

// SAFETY: a parse runs on one thread; the loader + context are the
// consumer's, handed straight back to their callback.  We never
// dereference `vctxt` ourselves.
unsafe impl Send for CResourceLoaderResolver {}
unsafe impl Sync for CResourceLoaderResolver {}

impl std::fmt::Debug for CResourceLoaderResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CResourceLoaderResolver")
    }
}

impl EntityResolver for CResourceLoaderResolver {
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let url = CString::new(system_id)
            .map_err(|_| ResolveError::Io("system id has interior NUL".into()))?;
        let pid = public_id.and_then(|s| CString::new(s).ok());
        let pid_ptr = pid.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        let mut out: *mut c_void = ptr::null_mut();
        // The resolve() contract doesn't carry libxml2's resource-type
        // discriminant, so we pass XML_RESOURCE_UNKNOWN (0) / flags 0.
        let rc = unsafe {
            (self.loader)(self.vctxt as *mut c_void, url.as_ptr(), pid_ptr, 0, 0, &mut out)
        };
        if rc != 0 {
            if !out.is_null() {
                unsafe { xmlFreeInputStream(out); }
            }
            return Err(ResolveError::Io(format!("resource loader returned {rc}")));
        }
        unsafe { take_input_bytes(out) }
            .ok_or_else(|| ResolveError::Io("resource loader produced no input".into()))
    }
}

/// libxml2 `xmlExternalEntityLoader` callback —
/// `xmlParserInput* (*)(const char *url, const char *id, xmlParserCtxt *ctxt)`.
pub(crate) type XmlExternalEntityLoader = unsafe extern "C" fn(
    url:  *const c_char,
    id:   *const c_char,
    ctxt: *mut c_void,
) -> *mut c_void;

/// Bridges the *global* external-entity loader (installed via
/// `xmlSetExternalEntityLoader`) to our [`EntityResolver`].  This is the
/// hook lxml uses: it registers `_local_resolver` as the loader and stows
/// its `Resolver` registry on `ctxt->_private`, so the loader must be
/// called with the parsing context to locate the consumer's resolvers.
pub(crate) struct CExternalEntityLoaderResolver {
    pub loader: XmlExternalEntityLoader,
    /// The parse context to hand the loader (lxml reads its `_private`).
    /// Stored as `usize` for `Send`/`Sync`; only passed back, never read.
    pub ctxt:   usize,
}

// SAFETY: a parse runs on one thread; loader + ctxt are the consumer's,
// handed straight back to their callback.  We never dereference `ctxt`.
unsafe impl Send for CExternalEntityLoaderResolver {}
unsafe impl Sync for CExternalEntityLoaderResolver {}

impl std::fmt::Debug for CExternalEntityLoaderResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CExternalEntityLoaderResolver")
    }
}

impl EntityResolver for CExternalEntityLoaderResolver {
    fn resolve(
        &self,
        public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let url = CString::new(system_id)
            .map_err(|_| ResolveError::Io("system id has interior NUL".into()))?;
        let pid = public_id.and_then(|s| CString::new(s).ok());
        let pid_ptr = pid.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        // libxml2's loader signature is (url, publicId, ctxt).
        let input = unsafe {
            (self.loader)(url.as_ptr(), pid_ptr, self.ctxt as *mut c_void)
        };
        if let Some(bytes) = unsafe { loader_input_take_bytes(input) } {
            return Ok(bytes);
        }
        // The loader declined (e.g. lxml's resolver found no match and its
        // captured default loader is absent).  Fall back to libxml2's
        // default behaviour: load a `file://` / local-path system id from
        // disk.  The parser only reaches this resolver when its options
        // already permit external loading.
        let path = system_id
            .strip_prefix("file://")
            .map(|r| r.strip_prefix("localhost").unwrap_or(r))
            .or_else(|| system_id.strip_prefix("file:"))
            .unwrap_or(system_id);
        std::fs::read(path)
            .map_err(|e| ResolveError::Io(format!("external entity {system_id:?}: {e}")))
    }
}

/// The currently-installed external-entity loader, if a consumer set one
/// that isn't our own fail-closed default (`xmlNoNetExternalEntityLoader`)
/// — i.e. a real resolver bridge worth driving for external DTDs/entities.
pub(crate) fn consumer_external_entity_loader() -> Option<XmlExternalEntityLoader> {
    let raw = unsafe { crate::misc::xmlGetExternalEntityLoader() };
    if raw.is_null() {
        return None;
    }
    let default = crate::misc::xmlNoNetExternalEntityLoader as *const () as *mut c_void;
    if raw == default {
        return None;
    }
    // SAFETY: stored by xmlSetExternalEntityLoader as a real fn pointer.
    Some(unsafe { std::mem::transmute::<*mut c_void, XmlExternalEntityLoader>(raw) })
}

/// Load a document's bytes through the consumer's external-entity loader
/// (lxml's `Resolver`s), if one is installed — matching libxml2, which
/// loads even the main document through the entity loader.  Returns
/// `None` when no consumer loader is set, so the caller reads directly.
/// A loader that declines falls back to a filesystem read internally
/// (see [`CExternalEntityLoaderResolver::resolve`]), so a `Some` result
/// already accounts for the resolver having had its chance.
///
/// # Safety
/// `ctxt` must be NULL or a live parser context — it is handed back to
/// the loader (which reads its `_private`), never dereferenced here.
pub(crate) unsafe fn load_document_via_resolver(
    ctxt: *mut crate::parsectx::XmlParserCtxt,
    path: &str,
) -> Option<Vec<u8>> {
    let loader = consumer_external_entity_loader()?;
    let resolver = CExternalEntityLoaderResolver { loader, ctxt: ctxt as usize };
    resolver.resolve(None, path, None).ok()
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The external-entity-loader path: `xmlNewInputStream` yields an
    /// empty input, a loader fills its `base`/`cur`/`end`/`length` (as
    /// lxml's pre-2.14 `_local_resolver` does), and `loader_input_take_bytes`
    /// copies the spanned bytes back out.
    #[test]
    fn loader_input_roundtrip() {
        let input = new_empty_input();
        const DATA: &[u8] = b"<!ELEMENT doc ANY><!ENTITY e \"X\">";
        unsafe {
            let inp = &mut *(input as *mut XmlParserInput);
            inp.base = DATA.as_ptr();
            inp.cur = inp.base;
            inp.end = inp.base.add(DATA.len());
            inp.length = DATA.len() as c_int;
        }
        let bytes = unsafe { loader_input_take_bytes(input) };
        assert_eq!(bytes.as_deref(), Some(DATA));
    }

    /// Round-trip: parse a tiny doc, walk to the root element, read
    /// its content, free.
    #[test]
    fn parse_walk_free_roundtrip() {
        let src = b"<r>hello</r>";
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null(), "expected successful parse");

        let root = unsafe { xmlDocGetRootElement(doc) };
        assert!(!root.is_null(), "expected a root element");

        // Read root->name directly via the struct field; verify "r".
        let root_ref = unsafe { &*root };
        assert_eq!(root_ref.name(), "r");

        // xmlNodeGetContent returns malloc'd UTF-8.
        let content_ptr = unsafe { xmlNodeGetContent(root) };
        assert!(!content_ptr.is_null());
        // SAFETY: content_ptr is a NUL-terminated C string we just allocated.
        let got = unsafe { std::ffi::CStr::from_ptr(content_ptr) }.to_str().unwrap();
        assert_eq!(got, "hello");

        unsafe {
            xmlFree(content_ptr as *mut c_void);
            xmlFreeDoc(doc);
        }
    }

    /// `xmlReadFd` parses from a file descriptor (slurp + xmlReadMemory)
    /// without closing the caller's fd.
    #[test]
    fn read_fd_parses_and_leaves_fd_open() {
        use std::io::Read;
        use std::os::unix::io::AsRawFd;

        let path = std::env::temp_dir()
            .join(format!("supxml_readfd_{}.xml", std::process::id()));
        std::fs::write(&path, b"<r>hi</r>").unwrap();
        let f = std::fs::File::open(&path).unwrap();
        let fd = f.as_raw_fd();

        let doc = unsafe { xmlReadFd(fd, ptr::null(), ptr::null(), 0) };
        assert!(!doc.is_null(), "xmlReadFd should parse the fd's contents");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert_eq!(unsafe { &*root }.name(), "r");
        unsafe { xmlFreeDoc(doc); }

        // The fd must NOT have been closed by xmlReadFd — we can still
        // read from it (after seeking back to the start).
        let mut f2 = f;
        use std::io::Seek;
        f2.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f2.read_to_string(&mut s).unwrap();
        assert_eq!(s, "<r>hi</r>", "fd should remain open and readable");

        drop(f2);
        std::fs::remove_file(&path).ok();
    }

    /// Negative fd → NULL, no panic.
    #[test]
    fn read_fd_negative_returns_null() {
        let doc = unsafe { xmlReadFd(-1, ptr::null(), ptr::null(), 0) };
        assert!(doc.is_null());
    }

    // ── input family → xmlCtxtParseDocument → free ─────────────────────

    /// Drive an `xmlNewInputFrom*` result through `xmlCtxtParseDocument`,
    /// assert the root element name, then reclaim input + doc.
    unsafe fn parse_input_expect_root(input: *mut c_void, want: &str) {
        assert!(!input.is_null(), "input constructor returned NULL");
        let doc = unsafe { xmlCtxtParseDocument(ptr::null_mut(), input) };
        assert!(!doc.is_null(), "xmlCtxtParseDocument returned NULL");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert_eq!(unsafe { &*root }.name(), want);
        unsafe {
            xmlFreeInputStream(input);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn input_from_memory() {
        let src = b"<r/>";
        let input = unsafe {
            xmlNewInputFromMemory(ptr::null(), src.as_ptr() as *const c_void, src.len(), 0)
        };
        unsafe { parse_input_expect_root(input, "r"); }
    }

    #[test]
    fn input_from_string() {
        let src = b"<doc/>\0";
        let input = unsafe { xmlNewInputFromString(ptr::null(), src.as_ptr() as *const c_char, 0) };
        unsafe { parse_input_expect_root(input, "doc"); }
    }

    #[test]
    fn input_from_fd() {
        use std::os::unix::io::AsRawFd;
        let path = std::env::temp_dir().join(format!("supxml_inputfd_{}.xml", std::process::id()));
        std::fs::write(&path, b"<fd/>").unwrap();
        let f = std::fs::File::open(&path).unwrap();
        let input = unsafe { xmlNewInputFromFd(ptr::null(), f.as_raw_fd(), 0) };
        unsafe { parse_input_expect_root(input, "fd"); }
        drop(f);
        std::fs::remove_file(&path).ok();
    }

    struct IoState { data: &'static [u8], pos: usize }
    unsafe extern "C" fn io_read_cb(ctx: *mut c_void, buf: *mut c_char, len: c_int) -> c_int {
        let st = unsafe { &mut *(ctx as *mut IoState) };
        let rest = &st.data[st.pos..];
        let n = rest.len().min(len as usize);
        unsafe { std::ptr::copy_nonoverlapping(rest.as_ptr(), buf as *mut u8, n); }
        st.pos += n;
        n as c_int
    }

    #[test]
    fn input_from_io() {
        let mut st = IoState { data: b"<io/>", pos: 0 };
        let input = unsafe {
            xmlNewInputFromIO(ptr::null(), Some(io_read_cb), None,
                              &mut st as *mut _ as *mut c_void, 0)
        };
        unsafe { parse_input_expect_root(input, "io"); }
    }

    #[test]
    fn input_from_url() {
        let path = std::env::temp_dir().join(format!("supxml_inputurl_{}.xml", std::process::id()));
        std::fs::write(&path, b"<url/>").unwrap();
        let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let mut out: *mut c_void = ptr::null_mut();
        let rc = unsafe { xmlNewInputFromUrl(cpath.as_ptr(), 0, &mut out) };
        assert_eq!(rc, 0, "xmlNewInputFromUrl should succeed on a readable file");
        unsafe { parse_input_expect_root(out, "url"); }
        std::fs::remove_file(&path).ok();

        // Missing file → non-zero status, out stays NULL.
        let bad = std::ffi::CString::new("/no/such/supxml/file.xml").unwrap();
        let mut out2: *mut c_void = ptr::null_mut();
        assert_ne!(unsafe { xmlNewInputFromUrl(bad.as_ptr(), 0, &mut out2) }, 0);
        assert!(out2.is_null());
    }

    // ── xmlCtxtSetResourceLoader → external-entity resolution ──────────

    struct LoaderState { called: bool, last_url: String }

    unsafe extern "C" fn res_loader(
        ctx: *mut c_void, url: *const c_char, _pid: *const c_char,
        _typ: c_int, _flags: c_int, out: *mut *mut c_void,
    ) -> c_int {
        let st = unsafe { &mut *(ctx as *mut LoaderState) };
        st.called = true;
        st.last_url = unsafe { CStr::from_ptr(url) }.to_str().unwrap_or("").to_string();
        // Hand back the resolved bytes as a parser input.
        let body = b"RESOLVED";
        let input = unsafe {
            xmlNewInputFromMemory(ptr::null(), body.as_ptr() as *const c_void, body.len(), 0)
        };
        unsafe { *out = input; }
        0
    }

    #[test]
    fn resource_loader_resolves_external_entity() {
        let mut st = LoaderState { called: false, last_url: String::new() };
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        assert!(!ctxt.is_null());
        unsafe {
            crate::parsectx::xmlCtxtSetResourceLoader(
                ctxt, Some(res_loader), &mut st as *mut _ as *mut c_void);
        }

        let src = b"<!DOCTYPE doc [ <!ENTITY ext SYSTEM \"ext.txt\"> ]><doc>&ext;</doc>";
        // XML_PARSE_NOENT: request entity substitution so the loader is
        // driven and `&ext;` expands into the tree (libxml2's NOENT-off
        // default leaves it as an entity-reference node).
        let doc = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt, src.as_ptr() as *const c_char, src.len() as c_int,
                ptr::null(), ptr::null(), super::XML_PARSE_NOENT)
        };
        assert!(!doc.is_null(), "parse should succeed");
        assert!(st.called, "resource loader should be invoked for the external entity");
        assert_eq!(st.last_url, "ext.txt");

        // The external entity should expand to the bytes the loader returned.
        let root = unsafe { xmlDocGetRootElement(doc) };
        let content = unsafe { xmlNodeGetContent(root) };
        let got = unsafe { CStr::from_ptr(content) }.to_str().unwrap();
        assert_eq!(got, "RESOLVED");

        unsafe {
            xmlFree(content as *mut c_void);
            xmlFreeDoc(doc);
            crate::parsectx::xmlFreeParserCtxt(ctxt);
        }
    }

    /// No loader registered → external entity is not loaded via a callback
    /// (default XXE-safe behaviour); parse still succeeds.
    #[test]
    fn no_resource_loader_does_not_invoke_callback() {
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        let src = b"<doc>plain</doc>";
        let doc = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt, src.as_ptr() as *const c_char, src.len() as c_int,
                ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null());
        unsafe { xmlFreeDoc(doc); crate::parsectx::xmlFreeParserCtxt(ctxt); }
    }

    // ── xmlCtxtSetErrorHandler → per-context error delivery ────────────

    struct ErrState { called: bool, msg: String }

    unsafe extern "C" fn err_handler(data: *mut c_void, err: *const crate::error::xmlError) {
        let st = unsafe { &mut *(data as *mut ErrState) };
        st.called = true;
        if !err.is_null() {
            let m = unsafe { (*err).message };
            if !m.is_null() {
                st.msg = unsafe { CStr::from_ptr(m) }.to_str().unwrap_or("").to_string();
            }
        }
    }

    #[test]
    fn error_handler_receives_parse_error() {
        let mut st = ErrState { called: false, msg: String::new() };
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        unsafe {
            crate::parsectx::xmlCtxtSetErrorHandler(
                ctxt, Some(err_handler), &mut st as *mut _ as *mut c_void);
        }
        // Mismatched tags → parse fails → handler delivers the error.
        let src = b"<a></b>";
        let doc = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt, src.as_ptr() as *const c_char, src.len() as c_int,
                ptr::null(), ptr::null(), 0)
        };
        assert!(doc.is_null(), "malformed input should fail to parse");
        assert!(st.called, "error handler should have been invoked");
        assert!(!st.msg.is_empty(), "handler should receive a non-empty error message");
        unsafe { crate::parsectx::xmlFreeParserCtxt(ctxt); }
    }

    // ── xmlCtxtSetMaxAmplification → entity-expansion cap ───────────────

    #[test]
    fn max_amplification_caps_entity_expansion() {
        // b expands to 100 × 100 = 10,000 chars from a ~440-byte input.
        let a = "x".repeat(100);
        let b_refs = "&a;".repeat(100);
        let src = format!("<!DOCTYPE r [ <!ENTITY a \"{a}\"><!ENTITY b \"{b_refs}\"> ]><r>&b;</r>");
        let bytes = src.as_bytes();

        // maxAmpl = 1 → cap ≈ input size; the 10,000-byte expansion blows
        // past it, so the parse is rejected (it would PASS under the
        // default 1 MB cap — proving the override is honored).  NOENT
        // requests the substitution that drives the expansion.
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        unsafe { crate::parsectx::xmlCtxtSetMaxAmplification(ctxt, 1); }
        let doc = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt, bytes.as_ptr() as *const c_char, bytes.len() as c_int,
                ptr::null(), ptr::null(), super::XML_PARSE_NOENT)
        };
        assert!(doc.is_null(), "tiny maxAmplification should reject the expansion");
        unsafe { crate::parsectx::xmlFreeParserCtxt(ctxt); }

        // Generous maxAmpl → cap >> expansion → parse succeeds.
        let ctxt2 = unsafe { crate::parsectx::xmlNewParserCtxt() };
        unsafe { crate::parsectx::xmlCtxtSetMaxAmplification(ctxt2, 1000); }
        let doc2 = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt2, bytes.as_ptr() as *const c_char, bytes.len() as c_int,
                ptr::null(), ptr::null(), super::XML_PARSE_NOENT)
        };
        assert!(!doc2.is_null(), "generous maxAmplification should allow the expansion");
        unsafe { xmlFreeDoc(doc2); crate::parsectx::xmlFreeParserCtxt(ctxt2); }
    }

    // ── explicit encoding / xmlSwitchEncodingName ──────────────────────

    /// Raw Latin-1 `<r>café</r>` (0xE9 = é) — invalid UTF-8 with no
    /// declaration, so it only parses when an explicit encoding is given.
    fn latin1_cafe() -> Vec<u8> {
        let mut b = b"<r>caf".to_vec();
        b.push(0xE9);
        b.extend_from_slice(b"</r>");
        b
    }

    #[test]
    fn explicit_encoding_argument_is_honored() {
        let bytes = latin1_cafe();

        // encoding = NULL → auto-detect assumes UTF-8 → 0xE9 is invalid → fail.
        let auto = unsafe {
            xmlReadMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int,
                          ptr::null(), ptr::null(), 0)
        };
        assert!(auto.is_null(), "raw Latin-1 without an encoding hint must fail UTF-8 validation");

        // encoding = "ISO-8859-1" → transcoded → "café".
        let enc = CString::new("ISO-8859-1").unwrap();
        let doc = unsafe {
            xmlReadMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int,
                          ptr::null(), enc.as_ptr(), 0)
        };
        assert!(!doc.is_null(), "explicit ISO-8859-1 should parse");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let content = unsafe { xmlNodeGetContent(root) };
        assert_eq!(unsafe { CStr::from_ptr(content) }.to_str().unwrap(), "café");
        unsafe { xmlFree(content as *mut c_void); xmlFreeDoc(doc); }
    }

    #[test]
    fn switch_encoding_name_applies_to_ctxt_parse() {
        let bytes = latin1_cafe();
        let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
        let enc = CString::new("latin1").unwrap();
        assert_eq!(unsafe { crate::parsectx::xmlSwitchEncodingName(ctxt, enc.as_ptr()) }, 0);

        // No explicit arg encoding → the ctxt's switch-encoding is used.
        let doc = unsafe {
            crate::parsectx::xmlCtxtReadMemory(
                ctxt, bytes.as_ptr() as *const c_char, bytes.len() as c_int,
                ptr::null(), ptr::null(), 0)
        };
        assert!(!doc.is_null(), "xmlSwitchEncodingName should make the ctxt parse Latin-1");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let content = unsafe { xmlNodeGetContent(root) };
        assert_eq!(unsafe { CStr::from_ptr(content) }.to_str().unwrap(), "café");
        unsafe {
            xmlFree(content as *mut c_void);
            xmlFreeDoc(doc);
            crate::parsectx::xmlFreeParserCtxt(ctxt);
        }
    }

    /// Malformed input → NULL doc, last-error populated.
    #[test]
    fn malformed_returns_null_with_error() {
        // Reset any leftover last-error from prior tests.
        crate::error::xmlResetLastError();

        let src = b"<unclosed>";
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(doc.is_null(), "expected NULL doc on malformed input");

        // last-error should be present.
        let last = crate::error::xmlGetLastError();
        assert!(!last.is_null(), "expected last-error to be populated");
    }

    /// Empty / NULL buffer is rejected cleanly (no panic, no UB).
    #[test]
    fn empty_input_rejected() {
        crate::error::xmlResetLastError();
        let doc = unsafe {
            xmlReadMemory(ptr::null(), 0, ptr::null(), ptr::null(), 0)
        };
        assert!(doc.is_null());
        let last = crate::error::xmlGetLastError();
        assert!(!last.is_null());
    }

    /// xmlDocGetRootElement on NULL returns NULL.
    #[test]
    fn root_of_null_is_null() {
        let r = unsafe { xmlDocGetRootElement(ptr::null()) };
        assert!(r.is_null());
    }

    /// xmlFreeDoc + xmlFree on NULL are safe no-ops.
    #[test]
    fn frees_on_null_are_noops() {
        unsafe {
            xmlFreeDoc(ptr::null_mut());
            xmlFree(ptr::null_mut());
        }
    }

    #[test]
    fn parser_get_directory_returns_parent() {
        let p = std::ffi::CString::new("/tmp/foo/bar.xml").unwrap();
        let dir = unsafe { xmlParserGetDirectory(p.as_ptr()) };
        let s = unsafe { CStr::from_ptr(dir) }.to_str().unwrap();
        assert_eq!(s, "/tmp/foo");
        unsafe { xml_free_impl(dir as *mut std::os::raw::c_void); }
        // Bare filename → "./"
        let p2 = std::ffi::CString::new("file.xml").unwrap();
        let dir2 = unsafe { xmlParserGetDirectory(p2.as_ptr()) };
        let s2 = unsafe { CStr::from_ptr(dir2) }.to_str().unwrap();
        assert_eq!(s2, "./");
        unsafe { xml_free_impl(dir2 as *mut std::os::raw::c_void); }
    }

    #[test]
    fn load_external_entity_returns_null_on_missing_file() {
        // Used to assert blanket NULL for XXE prevention; the
        // function now actually loads, so we just verify a
        // non-existent path produces NULL (the failure mode for
        // any caller that expected XXE-safety should now come
        // from the surrounding parser context's policy, not from
        // this function refusing across the board).
        let p = std::ffi::CString::new("/no/such/file/anywhere").unwrap();
        assert!(unsafe {
            xmlLoadExternalEntity(p.as_ptr(), ptr::null(), ptr::null_mut())
        }.is_null());
    }

    #[test]
    fn ctxt_parse_document_null_input_returns_null() {
        assert!(unsafe { xmlCtxtParseDocument(ptr::null_mut(), ptr::null_mut()) }.is_null());
    }

    #[test]
    fn load_then_parse_roundtrip() {
        // Round-trip: load a real file via xmlLoadExternalEntity,
        // then feed the input to xmlCtxtParseDocument.
        // Mirrors libxslt's xsltDocDefaultLoader flow.
        let tmp = std::env::temp_dir().join("sup-xml-load-test.xml");
        std::fs::write(&tmp, "<r>hello</r>").unwrap();
        let p = std::ffi::CString::new(tmp.to_str().unwrap()).unwrap();
        let input = unsafe { xmlLoadExternalEntity(p.as_ptr(), ptr::null(), ptr::null_mut()) };
        assert!(!input.is_null(), "should load the file");
        let doc = unsafe { xmlCtxtParseDocument(ptr::null_mut(), input) };
        assert!(!doc.is_null(), "should parse the loaded bytes");
        unsafe { xmlFreeInputStream(input); }
        unsafe { xmlFreeDoc(doc); }
        let _ = std::fs::remove_file(&tmp);
    }

    /// Security regression: with `options = 0`, `xmlReadMemory` must
    /// NOT load external entities.  libxml2's documented default is
    /// `XML_PARSE_DTDLOAD` off; loading requires the caller to opt
    /// in via the options bitmask.  If the shim hardcodes DTD
    /// loading on, a SYSTEM entity reference will exfiltrate the
    /// pointed-at file (classic XXE).
    #[test]
    fn xml_read_memory_default_options_does_not_load_external_entity() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"TOPSECRET").unwrap();
        }
        let src = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = src.into_bytes();
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        // Two acceptable outcomes, both XXE-safe:
        //  - parse fails (NULL doc) because the entity ref can't
        //    be resolved — strict mode rejects the reference.
        //  - parse succeeds but the entity expands to nothing /
        //    the original text — file contents must not appear.
        let got = if doc.is_null() {
            String::new()
        } else {
            let root = unsafe { xmlDocGetRootElement(doc) };
            let content_ptr = unsafe { xmlNodeGetContent(root) };
            let s = unsafe { CStr::from_ptr(content_ptr) }.to_str().unwrap_or("").to_string();
            unsafe {
                xmlFree(content_ptr as *mut c_void);
                xmlFreeDoc(doc);
            }
            s
        };
        let _ = std::fs::remove_file(&tmp);
        assert!(
            !got.contains("TOPSECRET"),
            "XXE: options=0 leaked file contents into entity expansion: {got:?}"
        );
    }

    /// Opt-in counterpart: with `XML_PARSE_DTDLOAD | XML_PARSE_NOENT`
    /// the caller has explicitly requested external-DTD loading and
    /// entity substitution — the file contents *should* expand.
    /// Locks in that the bitmask is actually parsed (not just
    /// silently defaulted off).
    #[test]
    fn xml_read_memory_dtdload_noent_loads_external_entity() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-optin-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"OPTIN_PAYLOAD").unwrap();
        }
        let src = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = src.into_bytes();
        // XML_PARSE_NOENT = 2, XML_PARSE_DTDLOAD = 4
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                2 | 4,
            )
        };
        assert!(!doc.is_null(), "expected successful parse");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let content_ptr = unsafe { xmlNodeGetContent(root) };
        let got = unsafe { CStr::from_ptr(content_ptr) }.to_str().unwrap_or("").to_string();
        unsafe {
            xmlFree(content_ptr as *mut c_void);
            xmlFreeDoc(doc);
        }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            got.contains("OPTIN_PAYLOAD"),
            "opt-in DTDLOAD|NOENT failed to expand external entity: {got:?}"
        );
    }
}
