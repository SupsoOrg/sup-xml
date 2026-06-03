//! libxml2 XSD validator façade — wraps `sup_xml_core::xsd::Schema`.
//!
//! The lxml flow is:
//!
//! ```text
//!   ctxt   = xmlSchemaNewDocParserCtxt(doc) | xmlSchemaNewParserCtxt(file)
//!   xmlSchemaSetParserStructuredErrors(ctxt, callback, user_data)
//!   schema = xmlSchemaParse(ctxt)               // compile
//!   xmlSchemaFreeParserCtxt(ctxt)
//!
//!   vctxt  = xmlSchemaNewValidCtxt(schema)
//!   xmlSchemaSetValidStructuredErrors(vctxt, callback, user_data)
//!   xmlSchemaSetValidOptions(vctxt, options)
//!   ret    = xmlSchemaValidateDoc(vctxt, doc)  // 0=valid, >0=invalid, <0=error
//!   xmlSchemaFreeValidCtxt(vctxt)
//!
//!   xmlSchemaFree(schema)
//! ```
//!
//! All four opaque types (`xmlSchemaParserCtxt`, `xmlSchema`,
//! `xmlSchemaValidCtxt`, `xmlSchemaSAXPlugStruct`) are heap-allocated
//! by us — lxml only ever holds pointers and calls back into our
//! functions.  Field-level access doesn't happen on these structs in
//! the consumer code paths we care about.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::error::{ErrorCode, ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::xsd::{Schema, SchemaResolver, ValidationIssue, ValidationKind};
use sup_xml_tree::dom::XmlDoc;

use crate::error::{xmlError, record_last_error, StructuredErrorFn};

// ── opaque types ──────────────────────────────────────────────────────────

/// Source of schema bytes for a parser context.
enum SchemaSource {
    /// Existing document — we serialize it back to bytes to feed our
    /// compiler.  Cheap relative to the compile itself.
    Doc(*const XmlDoc),
    /// File path — read at compile time.
    File(CString),
    /// In-memory buffer — copied at ctxt construction so the caller's
    /// buffer doesn't need to outlive the ctxt.
    Memory(Vec<u8>),
}

pub struct xmlSchemaParserCtxt {
    source: SchemaSource,
    error_cb: RefCell<Option<(StructuredErrorFn, *mut c_void)>>,
}

pub struct xmlSchema {
    inner: Schema,
}

pub struct xmlSchemaValidCtxt {
    /// Raw pointer — we don't own the Schema; consumer keeps it alive.
    schema: *const xmlSchema,
    options: c_int,
    error_cb: RefCell<Option<(StructuredErrorFn, *mut c_void)>>,
    /// Cached validity of the most recent validation pass on this context
    /// (`Some(true)` valid, `Some(false)` invalid, `None` not yet
    /// validated).  Set by [`xmlSchemaValidateDoc`] and by the parse-finish
    /// validation that [`xmlSchemaSAXPlug`] arranges; read by
    /// [`xmlSchemaIsValid`].
    last_valid: std::cell::Cell<Option<bool>>,
}

// ── parser context ────────────────────────────────────────────────────────

/// `xmlSchemaNewDocParserCtxt(doc)` — parser context bound to an
/// existing document.  We re-serialize it on compile.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaNewDocParserCtxt(
    doc: *const XmlDoc,
) -> *mut xmlSchemaParserCtxt {
    if doc.is_null() {
        return ptr::null_mut();
    }
    Box::into_raw(Box::new(xmlSchemaParserCtxt {
        source:   SchemaSource::Doc(doc),
        error_cb: RefCell::new(None),
    }))
}

/// `xmlSchemaNewParserCtxt(filename)` — parser context bound to a
/// file path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaNewParserCtxt(
    filename: *const c_char,
) -> *mut xmlSchemaParserCtxt {
    if filename.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let path = match unsafe { CStr::from_ptr(filename) }.to_owned() {
        p if p.as_bytes().is_empty() => return ptr::null_mut(),
        p => p,
    };
    Box::into_raw(Box::new(xmlSchemaParserCtxt {
        source:   SchemaSource::File(path),
        error_cb: RefCell::new(None),
    }))
}

/// `xmlSchemaNewMemParserCtxt(buffer, size)` — parser context bound
/// to an in-memory schema document.  We copy the buffer immediately
/// so the caller's pointer needn't outlive the ctxt.  Returns NULL on
/// NULL/empty input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaNewMemParserCtxt(
    buffer: *const c_char,
    size:   c_int,
) -> *mut xmlSchemaParserCtxt {
    if buffer.is_null() || size <= 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `buffer` is readable for `size` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) };
    Box::into_raw(Box::new(xmlSchemaParserCtxt {
        source:   SchemaSource::Memory(bytes.to_vec()),
        error_cb: RefCell::new(None),
    }))
}

/// `xmlSchemaFreeParserCtxt(ctxt)` — reclaim.  NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaFreeParserCtxt(ctxt: *mut xmlSchemaParserCtxt) {
    if ctxt.is_null() { return; }
    // SAFETY: ctxt came from xmlSchemaNew*ParserCtxt.
    unsafe { let _ = Box::from_raw(ctxt); }
}

/// `xmlSchemaSetParserStructuredErrors(ctxt, callback, user_data)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSetParserStructuredErrors(
    ctxt:     *mut xmlSchemaParserCtxt,
    callback: Option<StructuredErrorFn>,
    user_data: *mut c_void,
) {
    if ctxt.is_null() { return; }
    // SAFETY: ctxt non-null.
    let c = unsafe { &*ctxt };
    *c.error_cb.borrow_mut() = callback.map(|cb| (cb, user_data));
}

/// libxml2 `xmlSchemaSetParserErrors(ctxt, err, warn, ctx)` — the
/// legacy non-structured error setter (`err`/`warn` are printf-style
/// `void (*)(void *ctx, const char *msg, ...)` callbacks).
///
/// We accept the callbacks for API parity but do NOT invoke them at
/// error time — our error path routes through the structured
/// machinery only (see [`xmlSchemaSetParserStructuredErrors`]).
/// Consumers that need callback delivery should use the structured
/// variant.  Safe to call repeatedly; NULL ctxt is a no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSetParserErrors(
    _ctxt:      *mut xmlSchemaParserCtxt,
    _err_func:  *mut c_void,
    _warn_func: *mut c_void,
    _ctx:       *mut c_void,
) {}

/// `xmlSchemaParse(ctxt)` — compile.  Returns NULL on failure;
/// errors are reported via the registered structured callback.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaParse(
    ctxt: *mut xmlSchemaParserCtxt,
) -> *mut xmlSchema {
    if ctxt.is_null() { return ptr::null_mut(); }
    // SAFETY: ctxt non-null.
    let c = unsafe { &*ctxt };
    // Compile per source.  A File source uses `compile_file`, which
    // resolves `<xs:import>` / `<xs:include>` relative to the schema's own
    // directory (the in-memory / doc sources have no base directory, so
    // their references would need a caller-supplied resolver).
    let compiled = match &c.source {
        SchemaSource::Doc(doc) => {
            // Serialize via our compat-side serializer (dumps the doc
            // to UTF-8 XML).
            let mut mem: *mut c_char = ptr::null_mut();
            let mut size: c_int = 0;
            unsafe { crate::serialize::xmlDocDumpMemory(*doc as *const XmlDoc, &mut mem, &mut size); }
            if mem.is_null() {
                return ptr::null_mut();
            }
            let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size as usize) };
            let s = std::str::from_utf8(bytes).unwrap_or("").to_string();
            unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
            compile_schema_source(&s)
        }
        SchemaSource::File(path) => {
            let p = match path.to_str() {
                Ok(p) => p,
                Err(_) => return ptr::null_mut(),
            };
            Schema::compile_file(p)
        }
        SchemaSource::Memory(buf) => {
            // Reject buffers that aren't UTF-8 (libxml2 would handle
            // BOM / encoding sniffing — we expect callers to pre-
            // transcode for now).
            match std::str::from_utf8(buf) {
                Ok(s) => compile_schema_source(s),
                Err(_) => {
                    emit_parser_error(ctxt, &XmlError::new(
                        ErrorDomain::Encoding, ErrorLevel::Fatal,
                        "xmlSchemaParse: schema buffer is not valid UTF-8",
                    ));
                    return ptr::null_mut();
                }
            }
        }
    };
    match compiled {
        Ok(schema) => Box::into_raw(Box::new(xmlSchema { inner: schema })),
        Err(e) => {
            emit_parser_error(ctxt, &XmlError::new(
                ErrorDomain::Validation, ErrorLevel::Fatal,
                format!("xmlSchemaParse: {e}"),
            ));
            ptr::null_mut()
        }
    }
}

/// Bridges `<xs:import>` / `<xs:include>` resolution to the globally
/// registered external-entity loader (the hook lxml's schema resolvers
/// install).  Used for in-memory / document schema sources, which have no
/// base directory of their own.  Passes a NULL context; lxml's
/// `_local_resolver` then locates the active resolver via its thread-local
/// implied context.
struct LoaderSchemaResolver {
    loader: crate::parse::XmlExternalEntityLoader,
}

impl SchemaResolver for LoaderSchemaResolver {
    fn resolve(
        &self,
        location: &str,
        _target_namespace: Option<&str>,
    ) -> Result<Option<Vec<u8>>, std::io::Error> {
        let url = CString::new(location)
            .map_err(|_| std::io::Error::other("schemaLocation has interior NUL"))?;
        // lxml's `_local_resolver` reads `ctxt->_private` (offset 424) to
        // find its resolver registry, and falls back to a thread-local
        // implied context when that is NULL — so pass a zeroed context
        // blob (NULL `_private`) rather than a NULL pointer (which the
        // loader would dereference and crash on).
        let mut ctxt_blob = [0u8; 1024];
        let input = unsafe {
            (self.loader)(url.as_ptr(), ptr::null(), ctxt_blob.as_mut_ptr() as *mut c_void)
        };
        // None = the loader declined (no match); the caller turns an
        // unresolved import into the appropriate compile error.
        Ok(unsafe { crate::parse::loader_input_take_bytes(input) })
    }
}

/// Compile in-memory/document schema source `src`, routing imports through
/// a consumer-registered loader when one is present (else single-file).
fn compile_schema_source(src: &str) -> Result<Schema, sup_xml_core::xsd::SchemaCompileError> {
    match crate::parse::consumer_external_entity_loader() {
        Some(loader) => Schema::compile_with(src, LoaderSchemaResolver { loader }),
        None => Schema::compile_str(src),
    }
}

fn emit_parser_error(ctxt: *mut xmlSchemaParserCtxt, err: &XmlError) {
    if ctxt.is_null() {
        record_last_error(err);
        return;
    }
    // SAFETY: ctxt non-null.
    let c = unsafe { &*ctxt };
    emit_via_callback(&c.error_cb.borrow(), err, ptr::null_mut());
}

/// `xmlSchemaFree(schema)` — reclaim.  NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaFree(schema: *mut xmlSchema) {
    if schema.is_null() { return; }
    // SAFETY: schema came from xmlSchemaParse.
    unsafe { let _ = Box::from_raw(schema); }
}

// ── validation context ────────────────────────────────────────────────────

/// `xmlSchemaNewValidCtxt(schema)` — validation context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaNewValidCtxt(
    schema: *mut xmlSchema,
) -> *mut xmlSchemaValidCtxt {
    if schema.is_null() { return ptr::null_mut(); }
    Box::into_raw(Box::new(xmlSchemaValidCtxt {
        schema:   schema as *const _,
        options:  0,
        error_cb: RefCell::new(None),
        last_valid:    std::cell::Cell::new(None),
    }))
}

/// `xmlSchemaFreeValidCtxt(ctxt)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaFreeValidCtxt(ctxt: *mut xmlSchemaValidCtxt) {
    if ctxt.is_null() { return; }
    unsafe { let _ = Box::from_raw(ctxt); }
}

/// `xmlSchemaSetValidOptions(ctxt, options)`.  Accepted; honored
/// where applicable (currently a no-op — our validator's option set
/// is fixed for v0.1).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSetValidOptions(
    ctxt:    *mut xmlSchemaValidCtxt,
    options: c_int,
) -> c_int {
    if ctxt.is_null() { return -1; }
    unsafe { (*ctxt).options = options; }
    0
}

/// `xmlSchemaValidCtxtGetOptions(ctxt)` — read back the validation
/// options previously set via [`xmlSchemaSetValidOptions`].  Returns
/// `0` when `ctxt` is NULL or no options were set.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaValidCtxtGetOptions(
    ctxt: *mut xmlSchemaValidCtxt,
) -> c_int {
    if ctxt.is_null() { return 0; }
    unsafe { (*ctxt).options }
}

/// `xmlSchemaSetValidStructuredErrors(ctxt, callback, user_data)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSetValidStructuredErrors(
    ctxt:      *mut xmlSchemaValidCtxt,
    callback:  Option<StructuredErrorFn>,
    user_data: *mut c_void,
) {
    if ctxt.is_null() { return; }
    let c = unsafe { &*ctxt };
    *c.error_cb.borrow_mut() = callback.map(|cb| (cb, user_data));
}

/// libxml2 `xmlSchemaSetValidErrors(ctxt, err, warn, ctx)` — legacy
/// non-structured error setter, validator side.  Same rationale as
/// [`xmlSchemaSetParserErrors`]: accepted for API parity, not
/// invoked at error time — use the structured variant if you need
/// callback delivery.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSetValidErrors(
    _ctxt:      *mut xmlSchemaValidCtxt,
    _err_func:  *mut c_void,
    _warn_func: *mut c_void,
    _ctx:       *mut c_void,
) {}

/// `xmlSchemaValidateDoc(ctxt, doc)` — validate `doc` against the
/// schema bound to `ctxt`.  Returns 0 (valid), >0 (invalid), <0
/// (error).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaValidateDoc(
    ctxt: *mut xmlSchemaValidCtxt,
    doc:  *mut XmlDoc,
) -> c_int {
    if ctxt.is_null() || doc.is_null() {
        return -1;
    }
    let c = unsafe { &*ctxt };
    unsafe { run_validation(c, doc) }
}

/// Validate `doc` against `c`'s schema, emit each issue via the registered
/// structured error callback, cache the verdict on `c.last_valid`, and
/// return the libxml2 code (0 valid, >0 invalid, <0 error).
unsafe fn run_validation(c: &xmlSchemaValidCtxt, doc: *mut XmlDoc) -> c_int {
    if doc.is_null() || c.schema.is_null() {
        return -1;
    }
    let schema = unsafe { &(*c.schema).inner };
    // XML_SCHEMA_VAL_VC_I_CREATE (1<<0): augment the instance with
    // schema-defined attribute defaults.  This must mutate the live tree,
    // so validate the document in place rather than a serialized copy.
    const XML_SCHEMA_VAL_VC_I_CREATE: c_int = 1;
    let result = if c.options & XML_SCHEMA_VAL_VC_I_CREATE != 0 {
        let opts = sup_xml_core::xsd::ValidationOptions {
            apply_attribute_defaults: true,
            ..Default::default()
        };
        schema.validate_doc_opts(unsafe { &(*doc)._doc }, opts)
    } else {
        // Serialize doc → bytes → validate (keeps source line numbers in
        // diagnostics, which a direct DOM walk can't supply).
        let mut mem: *mut c_char = ptr::null_mut();
        let mut size: c_int = 0;
        unsafe { crate::serialize::xmlDocDumpMemory(doc, &mut mem, &mut size); }
        if mem.is_null() {
            return -1;
        }
        let bytes = unsafe { std::slice::from_raw_parts(mem as *const u8, size as usize) };
        let r = schema.validate_bytes(bytes);
        unsafe { crate::parse::xml_free_impl(mem as *mut c_void); }
        r
    };
    match result {
        Ok(()) => {
            c.last_valid.set(Some(true));
            0
        }
        Err(verr) => {
            for issue in &verr.issues {
                // The native validator owns its own (clearer) wording;
                // this ABI shim translates to libxml2's phrasing because
                // its consumers (lxml's objectify docs, error-log
                // scrapers) assert on the exact strings.  libxml2 also
                // keeps the node path in the error struct's node/line
                // fields, not baked into the message — so no path prefix.
                let msg = libxml2_validation_message(issue);
                let mut e = XmlError::new(ErrorDomain::SchemasValidate, ErrorLevel::Error, msg)
                    .with_code(schema_error_code(issue.kind));
                e.line = issue.line;
                // Carry the offending node so a consumer derives the
                // locator from it — lxml's `_LogEntry.path` is
                // `xmlGetNodePath(error.node)`, not a string on the error.
                // The validator ran over a serialized copy, so map its
                // instance path back onto the live `doc`.
                let node = unsafe { resolve_instance_path(doc, &issue.path) };
                emit_via_callback(&c.error_cb.borrow(), &e, node as *mut c_void);
            }
            c.last_valid.set(Some(false));
            // libxml2 reports invalid-doc as a positive return value
            // (typically 1 — number of errors).
            verr.issues.len() as c_int
        }
    }
}

/// `xmlSchemaIsValid(ctxt)` — whether the most recent validation on this
/// context found the instance valid.  When the validator was plugged into
/// a parser (via [`xmlSchemaSAXPlug`]) but no explicit validation has run
/// yet, validate the parser's result document now — this is how the
/// schema-validating `XMLParser(schema=…)` path reports invalidity.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaIsValid(
    ctxt: *mut xmlSchemaValidCtxt,
) -> c_int {
    if ctxt.is_null() {
        return 0;
    }
    let c = unsafe { &*ctxt };
    // The validation ran at parse-finish (see xmlSchemaSAXPlug); report its
    // cached verdict.  `None` means no validation happened — nothing to
    // reject.
    match c.last_valid.get() {
        Some(v) => c_int::from(v),
        None    => 1,
    }
}

/// Validate `doc` against the schema validator pointed to by the opaque
/// `vctxt`, caching the verdict.  Invoked by the parser at parse-finish for
/// a validator plugged onto the parse context via [`xmlSchemaSAXPlug`].
pub(crate) unsafe fn validate_plugged(vctxt: *mut c_void, doc: *mut XmlDoc) {
    if vctxt.is_null() || doc.is_null() {
        return;
    }
    let c = unsafe { &*(vctxt as *const xmlSchemaValidCtxt) };
    unsafe { run_validation(c, doc); }
}

/// Variant of [`validate_plugged`] that validates source `bytes` rather
/// than a built document — used by the incremental (`iterparse`) push
/// parser, whose persistent tree does not round-trip through
/// `xmlDocDumpMemory`.  The original input is already well-formed, so
/// validating it directly gives the schema verdict (`xmlSchemaIsValid`)
/// and the located diagnostics lxml needs to reject an invalid document,
/// without serializing the incremental tree.
pub(crate) unsafe fn validate_plugged_bytes(vctxt: *mut c_void, bytes: &[u8]) {
    if vctxt.is_null() {
        return;
    }
    let c = unsafe { &*(vctxt as *const xmlSchemaValidCtxt) };
    if c.schema.is_null() {
        return;
    }
    let schema = unsafe { &(*c.schema).inner };
    match schema.validate_bytes(bytes) {
        Ok(()) => {
            c.last_valid.set(Some(true));
        }
        Err(verr) => {
            for issue in &verr.issues {
                let msg = libxml2_validation_message(issue);
                let mut e = XmlError::new(ErrorDomain::SchemasValidate, ErrorLevel::Error, msg)
                    .with_code(schema_error_code(issue.kind));
                e.line = issue.line;
                emit_via_callback(&c.error_cb.borrow(), &e, ptr::null_mut());
            }
            c.last_valid.set(Some(false));
        }
    }
}

// ── SAX integration ────────────────────────────────────────────────────────

/// `xmlSchemaSAXPlug(ctxt, sax, user_data_ptr)` — install the validator as
/// a SAX filter on a parsing pipeline.  Rather than run the validator over
/// the SAX event stream, we link it onto the parse context (the `sax`
/// out-pointer is `&parserCtxt->sax`, and `sax` is the first field, so it
/// *is* the parse-context pointer); the parser then validates its result
/// document at finish time, before libxml2's `myDoc` is cleared.  Returns a
/// non-NULL handle (carrying the parse context) so the caller proceeds and
/// later calls [`xmlSchemaSAXUnplug`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSAXPlug(
    ctxt:            *mut xmlSchemaValidCtxt,
    sax:             *mut *mut c_void,
    _user_data_ptr:  *mut *mut c_void,
) -> *mut c_void {
    if ctxt.is_null() || sax.is_null() {
        return ptr::null_mut();
    }
    let c = unsafe { &*ctxt };
    c.last_valid.set(None);
    let pctxt = sax as *mut crate::parsectx::XmlParserCtxt;
    unsafe { crate::parsectx::set_ctxt_schema_validator(pctxt, ctxt as *mut c_void); }
    Box::into_raw(Box::new(pctxt)) as *mut c_void
}

/// `xmlSchemaSAXUnplug(plug)` — counterpart to [`xmlSchemaSAXPlug`];
/// detaches the validator from the parse context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSchemaSAXUnplug(plug: *mut c_void) -> c_int {
    if plug.is_null() {
        return 0;
    }
    let pctxt = unsafe { *Box::from_raw(plug as *mut *mut crate::parsectx::XmlParserCtxt) };
    if !pctxt.is_null() {
        unsafe { crate::parsectx::set_ctxt_schema_validator(pctxt, ptr::null_mut()); }
    }
    0
}

// ── callback helper ───────────────────────────────────────────────────────

/// Map a validation issue category to the libxml2 schema-validation error
/// code (`XML_SCHEMAV_*`) consumers classify on (e.g. lxml's
/// `error_log.filter_types`).  Kinds without a dedicated mapping keep the
/// generic internal-error code.
/// Translate a native validation issue's message into libxml2's
/// wording for ABI consumers.  The native validator keeps its own
/// (often clearer) phrasing; only this compat shim matches libxml2,
/// whose exact strings lxml & friends assert on.  Kinds without a
/// known libxml2 phrasing fall through to the native message.
fn libxml2_validation_message(issue: &ValidationIssue) -> String {
    match issue.kind {
        // libxml2 `xmlschemas.c` cvc-complex-type 2.4:
        //   "Element '<local>': This element is not expected. Expected is ( a b )."
        // Native form is "unexpected element <local>".
        ValidationKind::UnexpectedElement => {
            let local = match issue.message.split_once('<').and_then(|(_, r)| r.split_once('>')) {
                Some((l, _)) => l.to_string(),
                None => return issue.message.clone(),
            };
            let mut s = format!("Element '{local}': This element is not expected.");
            if !issue.expected.is_empty() {
                s.push_str(&format!(" Expected is ( {} ).", issue.expected.join(" ")));
            }
            s
        }
        // libxml2 `xmlschemas.c` cvc-datatype-valid 1.2.1:
        //   "Element '<local>': '<value>' is not a valid value of the
        //    atomic type '<type>'."
        ValidationKind::TypeMismatch
            if issue.value.is_some() && issue.type_name.is_some() =>
        {
            let local = path_local(&issue.path);
            format!(
                "Element '{local}': '{}' is not a valid value of the atomic type '{}'.",
                issue.value.as_deref().unwrap_or(""),
                issue.type_name.as_deref().unwrap_or(""),
            )
        }
        _ => issue.message.clone(),
    }
}

/// Local name of the deepest element in an issue's XPath-ish `path`
/// (e.g. `/invoice/item[3]` → `item`), for libxml2's "Element '…':"
/// prefix.
fn path_local(path: &str) -> &str {
    path.rsplit('/').next().map(|s| s.split('[').next().unwrap_or(s)).unwrap_or(path)
}

fn schema_error_code(kind: ValidationKind) -> ErrorCode {
    match kind {
        ValidationKind::UnexpectedElement
        | ValidationKind::MissingRequiredElement
        | ValidationKind::SubstitutionMismatch => ErrorCode::SchemavElementContent,
        ValidationKind::UnexpectedAttribute      => ErrorCode::SchemavCvcComplexType322,
        ValidationKind::MissingRequiredAttribute  => ErrorCode::SchemavCvcComplexType4,
        ValidationKind::TypeMismatch              => ErrorCode::SchemavCvcDatatypeValid121,
        ValidationKind::FacetViolation            => ErrorCode::SchemavCvcFacetValid,
        _ => ErrorCode::InternalError,
    }
}

/// Resolve a validator instance path (`/a/b[2]`) to the live node it names
/// in `doc`, matching element children by local name and 1-based same-name
/// position.  The validator runs over a serialized copy of the instance, so
/// its issue paths must be mapped back onto the caller's tree to attach the
/// node a consumer needs (lxml's `xmlGetNodePath(error.node)`).  NULL when
/// the path is empty or doesn't resolve.
unsafe fn resolve_instance_path(
    doc: *mut XmlDoc, path: &str,
) -> *mut sup_xml_tree::dom::Node<'static> {
    use sup_xml_tree::dom::{Node, NodeKind};
    fn step(s: &str) -> (&str, usize) {
        match s.find('[') {
            Some(b) => (&s[..b], s[b + 1..].trim_end_matches(']').parse().unwrap_or(1)),
            None    => (s, 1),
        }
    }
    if doc.is_null() || !path.starts_with('/') {
        return ptr::null_mut();
    }
    let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
    if root.is_null() {
        return ptr::null_mut();
    }
    let mut cur: &Node<'static> = unsafe { &*root };
    let mut steps = path[1..].split('/');
    // The first step names the root element itself.
    match steps.next() {
        Some(first) if cur.local_name() == step(first).0 => {}
        _ => return ptr::null_mut(),
    }
    for s in steps {
        let (name, idx) = step(s);
        let mut count = 0usize;
        let mut next: Option<&Node<'static>> = None;
        let mut child = cur.first_child.get();
        while let Some(c) = child {
            if matches!(c.kind, NodeKind::Element) && c.local_name() == name {
                count += 1;
                if count == idx {
                    next = Some(c);
                    break;
                }
            }
            child = c.next_sibling.get();
        }
        match next {
            Some(c) => cur = c,
            None    => return ptr::null_mut(),
        }
    }
    cur as *const Node<'static> as *mut Node<'static>
}

fn emit_via_callback(
    cb: &Option<(StructuredErrorFn, *mut c_void)>,
    err: &XmlError,
    node: *mut c_void,
) {
    let Some((cb_fn, user_data)) = cb else {
        record_last_error(err);
        return;
    };
    // Build a transient xmlError struct on the stack matching
    // libxml2's layout, then call into the callback.
    let msg_cs = CString::new(err.message.clone()).unwrap_or_default();
    // Zero-init then set the public fields.  `_pad_*` fields are
    // private to the `error` module; zero is the right default.
    let mut e: xmlError = unsafe { std::mem::zeroed() };
    e.domain  = err.domain as c_int;
    e.code    = err.code as c_int;
    e.message = msg_cs.as_ptr() as *mut c_char;
    e.level   = err.level as c_int;
    e.line    = err.line.unwrap_or(0) as c_int;
    e.int2    = err.column.unwrap_or(0) as c_int;
    e.node    = node;
    // SAFETY: cb_fn is a caller-supplied extern "C" function pointer;
    // &mut e and *user_data are valid for the call's duration.
    unsafe { cb_fn(*user_data, &mut e as *mut xmlError); }
    // msg_cs dropped at end of scope; the callback has consumed
    // (typically copied) the message by now.
    drop(msg_cs);
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
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

    const SCHEMA_SRC: &[u8] = br#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema">
  <xs:element name="r">
    <xs:complexType>
      <xs:sequence>
        <xs:element name="name" type="xs:string"/>
        <xs:element name="age" type="xs:int"/>
      </xs:sequence>
    </xs:complexType>
  </xs:element>
</xs:schema>"#;

    #[test]
    fn compile_then_validate_valid_doc() {
        let schema_doc = parse(SCHEMA_SRC);
        let pctxt = unsafe { xmlSchemaNewDocParserCtxt(schema_doc) };
        assert!(!pctxt.is_null());
        let schema = unsafe { xmlSchemaParse(pctxt) };
        assert!(!schema.is_null(), "schema compile failed");
        unsafe { xmlSchemaFreeParserCtxt(pctxt); }

        let doc = parse(b"<r><name>alice</name><age>30</age></r>");
        let vctxt = unsafe { xmlSchemaNewValidCtxt(schema) };
        let ret = unsafe { xmlSchemaValidateDoc(vctxt, doc) };
        assert_eq!(ret, 0, "valid doc should pass: ret={ret}");

        unsafe {
            xmlSchemaFreeValidCtxt(vctxt);
            xmlSchemaFree(schema);
            xmlFreeDoc(doc);
            xmlFreeDoc(schema_doc);
        }
    }

    #[test]
    fn validate_invalid_doc_returns_positive() {
        let schema_doc = parse(SCHEMA_SRC);
        let pctxt = unsafe { xmlSchemaNewDocParserCtxt(schema_doc) };
        let schema = unsafe { xmlSchemaParse(pctxt) };
        unsafe { xmlSchemaFreeParserCtxt(pctxt); }

        // Wrong element order; missing required field.
        let bad = parse(b"<r><age>thirty</age></r>");
        let vctxt = unsafe { xmlSchemaNewValidCtxt(schema) };
        let ret = unsafe { xmlSchemaValidateDoc(vctxt, bad) };
        assert!(ret > 0, "invalid doc should report errors: ret={ret}");

        unsafe {
            xmlSchemaFreeValidCtxt(vctxt);
            xmlSchemaFree(schema);
            xmlFreeDoc(bad);
            xmlFreeDoc(schema_doc);
        }
    }

    #[test]
    fn null_safety() {
        unsafe {
            xmlSchemaFree(ptr::null_mut());
            xmlSchemaFreeValidCtxt(ptr::null_mut());
            xmlSchemaFreeParserCtxt(ptr::null_mut());
            assert!(xmlSchemaParse(ptr::null_mut()).is_null());
            assert_eq!(xmlSchemaValidateDoc(ptr::null_mut(), ptr::null_mut()), -1);
            assert_eq!(xmlSchemaIsValid(ptr::null_mut()), 0);
        }
    }
}
