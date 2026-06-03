//! Push-parser API — `xmlCreatePushParserCtxt` + `xmlParseChunk`.
//!
//! lxml's `etree.XMLPullParser`, and any consumer that feeds bytes
//! incrementally (network streams, large files chunked from disk),
//! lands here.  Our v0.1 implementation buffers all chunks until
//! `terminate=1` then parses the accumulated bytes — not truly
//! streaming, but matches the API surface so consumers work.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::options::ParseOptions;
// parse_bytes_with_dtd is reached via the fully-qualified path
// inside xmlParseChunk so it stays adjacent to the DTD stash call.
use sup_xml_tree::dom::{Node, XmlDoc};

use crate::parsectx::XmlParserCtxt;

/// Reuse [`XmlParserCtxt`] for the push parser context — it's a
/// 752-byte zero-init buffer that consumer code reads scalar fields
/// from.  We also stash the byte-buffer accumulator + final doc
/// pointer in a side allocation, keyed by the ctxt's address.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// Per-context push-state: accumulator, terminated flag, last
    /// returned doc pointer (so xmlCtxtReadMemory-style reads work
    /// after termination).
    static PUSH_STATES: RefCell<HashMap<usize, PushState>>
        = RefCell::new(HashMap::new());
}

thread_local! {
    /// Key of the push context currently being finalized, or 0.
    ///
    /// Finalizing drives the consumer's SAX callbacks, which may re-enter
    /// the ABI — including `xmlFreeParserCtxt` on *another* context (lxml
    /// frees a temporary parser inside a target callback) or on *this*
    /// one.  To keep those re-entrant calls from borrowing `PUSH_STATES`
    /// while `xmlParseChunk` holds it, the state being finalized is
    /// removed from the map for the duration of the call; this records
    /// which key is "checked out" so [`forget_push_state`] can recognise
    /// a free of the in-flight context and signal it (by clearing this)
    /// rather than touching the map.
    static FINALIZING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct PushState {
    buf:       Vec<u8>,
    finished:  bool,
    /// HTML push parser (`htmlCreatePushParserCtxt`) vs XML
    /// (`xmlCreatePushParserCtxt`).  Selects the HTML5 parser on
    /// terminate and drives SAX1 (not SAX2) callback replay, since
    /// libxml2's HTML parser is SAX1-only (no namespaces).
    is_html:   bool,
    /// Doc pointer set on xmlParseChunk(terminate=1).  Caller may
    /// reach it via the ctxt's `myDoc` field (offset 16) — we plant
    /// it there directly.
    doc:       *mut XmlDoc,
    /// Forced input encoding from `xmlCtxtResetPush`'s `encoding`
    /// argument (lxml's `iterparse(..., encoding=…)` / feed parser).
    /// Overrides the document's own `<?xml encoding?>` declaration.
    forced_encoding: Option<String>,
    /// Base URI from the push-context `filename` argument, used to
    /// resolve a relative external-DTD SYSTEM id when
    /// `attribute_defaults` / DTD loading is on (iterparse from a file).
    base_url: Option<String>,
    /// Number of SAX events already fired to an event consumer
    /// (`XMLPullParser` / `iterparse` / a target parser).  Each `feed`
    /// grows the persistent tree and fires only the events past this
    /// watermark; the close fires the remainder.  See [`incremental_feed`].
    events_fired: usize,
    /// Whether `startDocument` has been fired yet (so the first
    /// incremental batch fires it and later ones don't).
    started_doc:  bool,
    /// The single persistent document grown across feeds for an event
    /// consumer (NULL until the first incremental batch).  Event elements
    /// reference its nodes, so it must be ONE stable tree — `iterparse`
    /// compares an event's element to the finished tree by identity.
    inc_doc:      *mut XmlDoc,
    /// Stack of currently-open element nodes in [`inc_doc`], carried
    /// across feeds so a start tag in one chunk and its children in the
    /// next attach to the same parent.
    open_stack:   Vec<*mut Node<'static>>,
}

/// `xmlCreatePushParserCtxt(sax, userData, chunk, size, filename)`.
///
/// We ignore `sax`/`userData` — our internal parser doesn't dispatch
/// through SAX callbacks.  `chunk`/`size` is the optional initial
/// buffer; the rest comes via `xmlParseChunk`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreatePushParserCtxt(
    _sax:       *mut c_void,
    _user_data: *mut c_void,
    chunk:      *const c_char,
    size:       c_int,
    filename:   *const c_char,
) -> *mut XmlParserCtxt {
    let ctxt = unsafe { crate::parsectx::xmlNewParserCtxt() };
    if ctxt.is_null() {
        return ptr::null_mut();
    }
    // lxml passes the source path/URL here for a file-backed
    // `iterparse`; it is the base for resolving a relative external DTD.
    let base_url = if filename.is_null() {
        None
    } else {
        unsafe { std::ffi::CStr::from_ptr(filename) }.to_str().ok().filter(|s| !s.is_empty()).map(str::to_string)
    };
    let mut state = PushState {
        buf:      Vec::new(),
        finished: false,
        is_html:  false,
        doc:      ptr::null_mut(),
        forced_encoding: None,
        base_url,
        events_fired: 0,
        started_doc:  false,
        inc_doc:      ptr::null_mut(),
        open_stack:   Vec::new(),
    };
    if !chunk.is_null() && size > 0 {
        // SAFETY: caller asserts `chunk` is readable for `size` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(chunk as *const u8, size as usize) };
        state.buf.extend_from_slice(bytes);
    }
    PUSH_STATES.with(|m| {
        m.borrow_mut().insert(ctxt as usize, state);
    });
    // Start with wellFormed=1.  lxml's feed-parser polls this slot
    // after every chunk; an unconditional 0 would make any mid-stream
    // poll look like a malformed parse.  We flip it back to 0 only if
    // the final terminate=1 parse errors out.
    unsafe { crate::parsectx::set_well_formed(ctxt, true); }
    ctxt
}

/// Mark a push-parser context as HTML so `terminate=1` runs the
/// HTML5 parser and replays SAX1 (not SAX2) callbacks.  Called by
/// `htmlCreatePushParserCtxt`.
pub(crate) fn mark_html(ctxt: *mut XmlParserCtxt) {
    PUSH_STATES.with(|m| {
        if let Some(s) = m.borrow_mut().get_mut(&(ctxt as usize)) {
            s.is_html = true;
        }
    });
}

/// Build [`ParseOptions`] from the ctxt's stored `XML_PARSE_*`
/// bitmask (set via `xmlCtxtUseOptions`).  Safe defaults
/// (`load_external_dtd: false`) — the bitmask only widens behaviour.
///
/// # Safety
///
/// `ctxt` must point at a live [`XmlParserCtxt`].
unsafe fn build_parse_options(ctxt: *const XmlParserCtxt) -> ParseOptions {
    let mut opts = ParseOptions {
        namespace_aware: true,
        ..ParseOptions::default()
    };
    let stored = unsafe { crate::parsectx::read_ctxt_options(ctxt) };
    crate::parse::map_libxml2_options(stored, &mut opts);
    // remove_comments / remove_pis: lxml NULLs the SAX comment /
    // processingInstruction callbacks (iterparse honours these too).
    let (rc, rp) = unsafe { crate::parsectx::read_ctxt_sax_remove_flags(ctxt) };
    opts.remove_comments = rc;
    opts.remove_pis = rp;
    opts
}

/// Apply the push state's forced encoding (from `xmlCtxtResetPush`) so
/// it overrides the document's `<?xml encoding?>` declaration — what
/// lxml's `iterparse(..., encoding=…)` relies on.
fn apply_forced_encoding(opts: &mut ParseOptions, state: &PushState) {
    if let Some(name) = state.forced_encoding.as_deref() {
        opts.forced_encoding = Some(sup_xml_core::encoding::encoding_from_name(name));
    }
}

/// Plant the parsed document on `state.doc` and on `ctxt->myDoc`,
/// stash the DTD sidecar if one was produced, and replay SAX
/// callbacks for any consumer that installed handlers.
///
/// # Safety
///
/// `ctxt` must point at a live [`XmlParserCtxt`]; `xml_doc` must
/// be a freshly-built doc whose ownership we're transferring into
/// the ctxt's `myDoc` field and `state.doc`.
unsafe fn install_parsed_doc(
    ctxt:    *mut XmlParserCtxt,
    state:   &mut PushState,
    xml_doc: *mut XmlDoc,
    dtd:     sup_xml_core::dtd::Dtd,
) {
    state.doc = xml_doc;
    if !dtd.is_empty() {
        let dtd_name = dtd.elements.keys().next().map(|s| s.as_str()).unwrap_or("");
        let cname = std::ffi::CString::new(dtd_name).unwrap_or_default();
        unsafe {
            let _ = crate::dtd::xmlCreateIntSubset(
                xml_doc, cname.as_ptr(), ptr::null(), ptr::null(),
            );
        }
        crate::dtd::stash_dtd(xml_doc, dtd);
    }
    unsafe {
        crate::parsectx::write_my_doc(ctxt, xml_doc);
        crate::parsectx::set_well_formed(ctxt, true);
        // Drive any SAX callbacks the consumer installed on
        // ctxt->sax — lxml's iterparse / target parser depends on
        // start/end/characters firing in document order.  Our push
        // parser is buffer-then-parse, so we synthesise the calls
        // here from the already-built tree.  XML consumers get SAX2
        // (startElementNs); HTML consumers get SAX1 (startElement),
        // matching libxml2's namespace-free HTML parser.  See
        // `saxreplay` for the walk + no-op-stub setup rationale.
        crate::saxreplay::replay(ctxt, xml_doc, state.is_html);
    }
}

/// Terminate-branch body of [`xmlParseChunk`]: parse the
/// accumulated buffer and update both the side-channel state and
/// the ctxt's visible fields.  Extracted to keep the FFI entry
/// point readable.
///
/// # Safety
///
/// `ctxt` must point at a live [`XmlParserCtxt`].  Caller holds
/// the exclusive borrow on `state` (it came from a thread-local
/// map and the closure doesn't re-enter).
unsafe fn finalize_push_parse(ctxt: *mut XmlParserCtxt, state: &mut PushState) {
    // Route through the thread-local shared dict so push-parsed docs
    // join the same intern table as docs from other paths, and a fresh
    // per-document arena (kept alive per-thread).  See
    // `dict.rs::new_doc_arena`.
    let dict  = crate::dict::thread_dict();
    let arena = crate::dict::new_doc_arena();
    if state.is_html {
        unsafe { finalize_push_parse_html(ctxt, state, dict, arena); }
        return;
    }
    let mut opts = unsafe { build_parse_options(ctxt) };
    apply_forced_encoding(&mut opts, state);
    if opts.base_url.is_none() {
        opts.base_url = state.base_url.clone();
    }
    // SAFETY: thread_dict returns a live, refcount-managed Dict.
    let result = unsafe {
        sup_xml_core::parser::parse_bytes_with_dtd_dict_arena(&state.buf, &opts, dict, arena)
    };
    match result {
        Ok((doc, dtd)) => {
            sup_xml_core::dtd::inject_defaults(&doc, &dtd);
            let xml_doc = doc.into_xml_doc();
            unsafe { install_parsed_doc(ctxt, state, xml_doc, dtd); }
        }
        Err(e) => {
            crate::error::record_last_error(&e);
            // Parse failed — flip wellFormed back to 0 so lxml's
            // `not pctxt.wellFormed` check fires.
            unsafe { crate::parsectx::set_well_formed(ctxt, false); }
        }
    }
}


/// Ensure the persistent incremental document exists, then build the
/// buffer's complete prefix into it (only the nodes past the watermark).
/// Returns `(events, clean_eof, stop_error)` — the error is the reason the
/// build stopped short of EOF (truncated tail or malformed markup), which
/// the close records so lxml's error log raises.
///
/// # Safety
/// `ctxt`/`state` are live.
unsafe fn build_into_persistent(
    ctxt:  *mut XmlParserCtxt,
    state: &mut PushState,
) -> (usize, bool, Option<sup_xml_core::error::XmlError>) {
    if state.inc_doc.is_null() {
        state.inc_doc = crate::pushincr::new_incremental_doc();
        if state.inc_doc.is_null() {
            return (state.events_fired, false, None);
        }
    }
    let mut opts = unsafe { build_parse_options(ctxt) };
    apply_forced_encoding(&mut opts, state);
    if opts.base_url.is_none() {
        opts.base_url = state.base_url.clone();
    }
    // `open_stack` is moved out so the reader can borrow `state.buf`
    // immutably while we mutate the stack; `inc_doc` is a raw pointer.
    let mut open = std::mem::take(&mut state.open_stack);
    let result = unsafe {
        crate::pushincr::build_prefix(state.inc_doc, &mut open, state.events_fired, &state.buf, &opts)
    };
    state.open_stack = open;
    result
}

/// Fire the SAX events that became complete since the previous `feed()`.
/// Grows the persistent tree with the newly-complete nodes and replays
/// only the events past `state.events_fired`, advancing the watermark.
/// Does not finalize — the buffer keeps accumulating until `terminate=1`.
///
/// # Safety
/// `ctxt` must be a live [`XmlParserCtxt`] with event handlers installed.
unsafe fn incremental_feed(ctxt: *mut XmlParserCtxt, state: &mut PushState) {
    // The incremental driver is SAX2-only; HTML push parsers buffer to
    // close and replay SAX1 there.
    if state.is_html { return; }
    let (n, _eof, err) = unsafe { build_into_persistent(ctxt, state) };
    if !state.inc_doc.is_null() && n > state.events_fired {
        // lxml's SAX handlers read `ctxt->myDoc` (the comment / PI handlers
        // attach relative to it).  Plant the persistent doc, fire the newly
        // complete events.
        unsafe { crate::parsectx::write_my_doc(ctxt, state.inc_doc); }
        let count = n - state.events_fired;
        let fire_start_doc = !state.started_doc;
        let fired = unsafe {
            crate::saxreplay::replay_range(ctxt, state.inc_doc, state.events_fired, count, fire_start_doc, false)
        };
        state.started_doc = true;
        state.events_fired = state.events_fired.max(fired);
    }
    // A *genuine* well-formedness violation can't be repaired by more
    // bytes, so report it now — lxml's iterparse raises on the next read
    // rather than silently finishing on the events already delivered.  A
    // merely-truncated tail (the default) is expected mid-feed and only
    // becomes an error at close.
    if let Some(e) = &err {
        if is_genuine_error(e) {
            unsafe { crate::parsectx::set_well_formed(ctxt, false); }
            crate::error::record_last_error(e);
        }
    }
}

/// Whether a reader error is a definite well-formedness violation (more
/// input cannot repair it) rather than a "ran off the end of the buffer"
/// truncation (the token might complete in the next `feed`).
///
/// Conservative by construction: only codes that can *never* be a
/// mid-token truncation count as genuine, so a valid document fed in
/// small chunks is never mistaken for a broken one.  Anything else (a
/// half-read tag, an unbalanced tree at the buffer edge, an undecodable
/// trailing byte) is treated as "wait for more" mid-feed and surfaces at
/// close if it's still unresolved.
fn is_genuine_error(e: &sup_xml_core::error::XmlError) -> bool {
    use sup_xml_core::error::ErrorCode::*;
    matches!(
        e.code,
        TagNameMismatch
            | ExtraContent
            | InvalidChar
            | MisplacedCdataEnd
            | AttributeRedefined
            | NsErrUndefinedNamespace
            | NsErrQname
            | UndeclaredEntity
            | InvalidHexCharRef
            | InvalidDecCharRef
    )
}

/// Terminate-branch finalize for an event consumer.  Completes the
/// persistent tree, installs it as `myDoc`, fires any events not yet
/// delivered by [`incremental_feed`] (plus `startDocument` if no feed
/// produced events, and `endDocument`), and sets `wellFormed`.
///
/// # Safety
/// `ctxt` must be a live [`XmlParserCtxt`] with event handlers installed.
unsafe fn finalize_push_parse_incremental(ctxt: *mut XmlParserCtxt, state: &mut PushState) {
    if state.is_html {
        unsafe { finalize_push_parse(ctxt, state); }
        return;
    }
    let (n, clean_eof, stop_err) = unsafe { build_into_persistent(ctxt, state) };
    if state.inc_doc.is_null() {
        unsafe { crate::parsectx::set_well_formed(ctxt, false); }
        return;
    }
    if clean_eof {
        state.doc = state.inc_doc;
        unsafe {
            crate::parsectx::write_my_doc(ctxt, state.inc_doc);
            crate::parsectx::set_well_formed(ctxt, true);
        }
        // Run a schema validator plugged onto this context via
        // `xmlSchemaSAXPlug` (lxml's `iterparse(schema=…)`).  The non-event
        // Read path validates at finish; the incremental path must too, so
        // `xmlSchemaIsValid` reports the verdict and lxml's
        // `_handleParseResult` rejects an invalid document.
        let validator = unsafe { crate::parsectx::read_ctxt_schema_validator(ctxt) };
        if !validator.is_null() {
            // Validate the original source bytes, not the incremental tree
            // (which doesn't survive serialize→reparse).
            unsafe { crate::xsd::validate_plugged_bytes(validator, &state.buf); }
        }
        // Fire the events not yet delivered, plus startDocument (if no
        // feed produced events) and endDocument.
        let count = n.saturating_sub(state.events_fired);
        let fire_start_doc = !state.started_doc;
        let fired = unsafe {
            crate::saxreplay::replay_range(ctxt, state.inc_doc, state.events_fired, count, fire_start_doc, true)
        };
        state.started_doc = true;
        state.events_fired = state.events_fired.max(fired);
    } else {
        // A document that didn't tokenize to a clean EOF (truncated /
        // mismatched tags) is not well-formed.  Leave `myDoc` NULL, do NOT
        // fire `endDocument` (which would look like a clean finish), and
        // record the stop reason, mirroring the non-incremental finalize
        // so lxml's iterparse raises XMLSyntaxError (an empty error log
        // would surface no error).
        unsafe { crate::parsectx::set_well_formed(ctxt, false); }
        if let Some(e) = &stop_err {
            crate::error::record_last_error(e);
        } else {
            crate::error::record_last_error(&sup_xml_core::error::XmlError::new(
                sup_xml_core::error::ErrorDomain::Parser,
                sup_xml_core::error::ErrorLevel::Fatal,
                "premature end of document",
            ));
        }
    }
}

/// HTML branch of [`finalize_push_parse`]: run the HTML5 parser over
/// the accumulated buffer, plant the parsed `<!DOCTYPE>` as the
/// internal subset (so `docinfo.doctype` and the SAX1 `internalSubset`
/// callback both see it), then install + replay.
///
/// Deliberate divergence (see `lib.rs` "Behavioral divergences"): HTML is
/// buffered to the terminating chunk and SAX is replayed from the finished
/// tree, rather than fired incrementally during `feed()` like libxml2's
/// streaming parser.  html5ever's tree builder does adoption-agency /
/// reconstruction, so a prefix parse isn't stable against the full
/// document; the final tree is identical, only mid-`feed()` event timing
/// differs (`test_html_iterparse_*`, `test_html_parser_target_exceptions`).
///
/// # Safety
/// `ctxt` must point at a live [`XmlParserCtxt`]; `dict` is a live
/// refcount-managed dict and `arena` a shared parse arena.
unsafe fn finalize_push_parse_html(
    ctxt:  *mut XmlParserCtxt,
    state: &mut PushState,
    dict:  *mut sup_xml_tree::dict::Dict,
    arena: std::sync::Arc<bumpalo::Bump>,
) {
    let opts = sup_xml_core::html::HtmlParseOptions::default();
    // SAFETY: dict/arena are live; buffer is owned by `state`.
    let result = unsafe {
        sup_xml_core::html::parse_html_bytes_opts_with_dict_arena(&state.buf, &opts, dict, arena)
    };
    match result {
        Ok(doc) => {
            let dt = crate::html::doctype_parts(&doc);
            let xml_doc = doc.into_xml_doc();
            if let Some((name, pid, sid)) = dt {
                unsafe {
                    crate::dtd::plant_int_subset(xml_doc, &name, pid.as_deref(), sid.as_deref());
                }
            }
            state.doc = xml_doc;
            unsafe {
                crate::parsectx::write_my_doc(ctxt, xml_doc);
                crate::parsectx::set_well_formed(ctxt, true);
                crate::saxreplay::replay(ctxt, xml_doc, true);
            }
        }
        Err(e) => {
            crate::error::record_last_error(&e);
            unsafe { crate::parsectx::set_well_formed(ctxt, false); }
        }
    }
}

/// `xmlParseChunk(ctxt, chunk, size, terminate)` — feed bytes.  When
/// `terminate != 0`, parses the accumulated buffer and stashes the
/// resulting doc on the context.  Returns 0 on success, non-zero
/// on parse error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseChunk(
    ctxt:      *mut XmlParserCtxt,
    chunk:     *const c_char,
    size:      c_int,
    terminate: c_int,
) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    let key = ctxt as usize;

    // Phase 1 — append the chunk and decide what to do, under a short
    // borrow that never spans a consumer callback.  `has_event_handlers`
    // distinguishes an event consumer (XMLPullParser / iterparse / a
    // target parser), which gets incrementally-fired SAX events, from a
    // plain feed parser that only wants the finished document.
    let has_events = unsafe { crate::saxreplay::has_event_handlers(ctxt) };
    enum Act { None, Finalize, IncrementalFeed }
    let act = PUSH_STATES.with(|m| {
        let mut map = m.borrow_mut();
        let Some(state) = map.get_mut(&key) else { return Act::None; };
        if !chunk.is_null() && size > 0 {
            // SAFETY: caller asserts `chunk` is readable for `size` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(chunk as *const u8, size as usize) };
            state.buf.extend_from_slice(bytes);
        }
        if terminate != 0 && !state.finished {
            Act::Finalize
        } else if terminate == 0 && !state.finished && has_events {
            // A target/iterparse consumer wants events during feed().
            Act::IncrementalFeed
        } else {
            Act::None
        }
    });
    if matches!(act, Act::None) {
        return 0;
    }

    // Phase 2 — finalize OUTSIDE the borrow.  Finalizing replays SAX
    // callbacks into the consumer, which can re-enter the ABI (e.g.
    // lxml's target parser frees a context when a callback raises).
    // Check the state out of the map so those re-entrant calls can take
    // their own `PUSH_STATES` borrow; `FINALIZING` lets `forget_push_state`
    // recognise a free of this very context.
    let Some(mut state) = PUSH_STATES.with(|m| m.borrow_mut().remove(&key)) else {
        return 0;
    };
    FINALIZING.with(|f| f.set(key));
    match act {
        Act::Finalize => {
            state.finished = true;
            // SAFETY: ctxt non-null (checked above).  Event consumers fire
            // any not-yet-delivered events relative to the incremental
            // watermark; plain feed parsers just build the document.
            if has_events {
                unsafe { finalize_push_parse_incremental(ctxt, &mut state); }
            } else {
                unsafe { finalize_push_parse(ctxt, &mut state); }
            }
        }
        Act::IncrementalFeed => {
            // Fire events that completed since the last feed; keep
            // buffering (don't mark finished) until terminate.
            // SAFETY: as above.
            unsafe { incremental_feed(ctxt, &mut state); }
        }
        Act::None => unreachable!(),
    }
    // Re-insert unless this context was freed mid-finalize (in which case
    // `forget_push_state` cleared `FINALIZING` and the entry must stay gone).
    let alive = FINALIZING.with(|f| {
        let v = f.get();
        f.set(0);
        v == key
    });
    if alive {
        PUSH_STATES.with(|m| { m.borrow_mut().insert(key, state); });
    }
    0
}

/// `xmlCtxtResetPush(ctxt, chunk, size, filename, encoding)` — reset
/// a push parser context to receive a new doc.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtResetPush(
    ctxt:     *mut XmlParserCtxt,
    chunk:    *const c_char,
    size:     c_int,
    _filename:*const c_char,
    encoding: *const c_char,
) -> c_int {
    if ctxt.is_null() {
        return -1;
    }
    // A non-NULL `encoding` forces the input encoding (lxml's
    // `iterparse(..., encoding=…)` passes it here), overriding the
    // document's own `<?xml encoding?>` declaration.
    let forced = if encoding.is_null() {
        None
    } else {
        unsafe { std::ffi::CStr::from_ptr(encoding) }.to_str().ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    PUSH_STATES.with(|m| {
        let mut map = m.borrow_mut();
        let entry = map.entry(ctxt as usize).or_insert_with(|| PushState {
            buf:      Vec::new(),
            finished: false,
            is_html:  false,
            doc:      ptr::null_mut(),
            forced_encoding: None,
            base_url: None,
            events_fired: 0,
            started_doc:  false,
            inc_doc:      ptr::null_mut(),
            open_stack:   Vec::new(),
        });
        if !_filename.is_null() {
            entry.base_url = unsafe { std::ffi::CStr::from_ptr(_filename) }
                .to_str().ok().filter(|s| !s.is_empty()).map(str::to_string);
        }
        entry.buf.clear();
        entry.finished = false;
        entry.doc = ptr::null_mut();
        entry.forced_encoding = forced;
        entry.events_fired = 0;
        entry.started_doc = false;
        // A reset starts a fresh document; the prior incremental tree (its
        // events already fired and its proxies retained by lxml) is left
        // to the thread arena keep-alive.
        entry.inc_doc = ptr::null_mut();
        entry.open_stack.clear();
        if !chunk.is_null() && size > 0 {
            let bytes = unsafe { std::slice::from_raw_parts(chunk as *const u8, size as usize) };
            entry.buf.extend_from_slice(bytes);
        }
    });
    // Reset the parser state for the fresh document, as libxml2's
    // xmlCtxtResetPush does.  Restore wellFormed=1 — a context reused
    // (lxml pools feed-parser contexts) after a prior parse that flipped
    // it to 0 would otherwise look malformed before the new parse runs,
    // surfacing as a spurious "no detail available" XMLSyntaxError.
    unsafe { crate::parsectx::set_well_formed(ctxt, true); }
    crate::error::xmlResetLastError();
    0
}

/// Internal — when [`crate::parsectx::xmlFreeParserCtxt`] runs, it
/// should also evict the push state for this ctxt.  Called from
/// there.  Hidden from the public ABI.
#[doc(hidden)]
pub fn forget_push_state(ctxt: *mut XmlParserCtxt) {
    let key = ctxt as usize;
    // If this context is mid-finalize its state is checked out of the
    // map (see `xmlParseChunk`); signal the free by clearing `FINALIZING`
    // so it isn't re-inserted, and leave the map untouched — borrowing it
    // here would collide with the in-flight `xmlParseChunk` borrow.
    let in_flight = FINALIZING.with(|f| {
        if f.get() == key { f.set(0); true } else { false }
    });
    if in_flight {
        return;
    }
    PUSH_STATES.with(|m| {
        m.borrow_mut().remove(&key);
    });
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsectx::xmlFreeParserCtxt;
    use crate::parse::xmlFreeDoc;

    #[test]
    fn push_parser_round_trip() {
        let ctxt = unsafe {
            xmlCreatePushParserCtxt(ptr::null_mut(), ptr::null_mut(),
                                     ptr::null(), 0, ptr::null())
        };
        assert!(!ctxt.is_null());

        // Feed in chunks.
        let p1 = b"<root><a";
        let p2 = b" id='1'/></root>";
        assert_eq!(unsafe { xmlParseChunk(ctxt, p1.as_ptr() as *const _, p1.len() as c_int, 0) }, 0);
        assert_eq!(unsafe { xmlParseChunk(ctxt, p2.as_ptr() as *const _, p2.len() as c_int, 1) }, 0);

        // Pull the doc out of ctxt->myDoc (offset 16).
        let doc_ptr: *mut XmlDoc = unsafe {
            let p = (ctxt as *mut u8).add(16);
            let bytes = std::slice::from_raw_parts(p, 8);
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            usize::from_ne_bytes(arr) as *mut XmlDoc
        };
        assert!(!doc_ptr.is_null());

        unsafe {
            xmlFreeDoc(doc_ptr);
            forget_push_state(ctxt);
            xmlFreeParserCtxt(ctxt);
        }
    }

    #[test]
    fn null_safety() {
        assert_eq!(unsafe {
            xmlParseChunk(ptr::null_mut(), ptr::null(), 0, 1)
        }, -1);
    }

    /// Opt-in counterpart: `xmlCtxtUseOptions(ctxt, XML_PARSE_DTDLOAD)`
    /// must wire through to the push parser so that the SYSTEM
    /// entity gets loaded.  This is the canonical libxml2 API for
    /// the push-parser surface — without it, callers have no way
    /// to opt into DTD loading for a chunked parse.
    #[test]
    fn push_parser_with_dtdload_option_loads_entity() {
        use std::io::Write;
        use std::ffi::CStr;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-push-optin-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"PUSH_OPTIN_PAYLOAD").unwrap();
        }
        let src = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = src.into_bytes();

        let ctxt = unsafe {
            xmlCreatePushParserCtxt(ptr::null_mut(), ptr::null_mut(),
                                     ptr::null(), 0, ptr::null())
        };
        assert!(!ctxt.is_null());
        // XML_PARSE_DTDLOAD = 4, XML_PARSE_NOENT = 2.
        let rc = unsafe { crate::parsectx::xmlCtxtUseOptions(ctxt, 2 | 4) };
        assert_eq!(rc, 0);
        let pc = unsafe { xmlParseChunk(ctxt, src.as_ptr() as *const _, src.len() as c_int, 1) };
        assert_eq!(pc, 0, "push parse with DTDLOAD should succeed");

        let doc_ptr: *mut XmlDoc = unsafe {
            let p = (ctxt as *mut u8).add(16);
            let bytes = std::slice::from_raw_parts(p, 8);
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            usize::from_ne_bytes(arr) as *mut XmlDoc
        };
        assert!(!doc_ptr.is_null(), "push parse with DTDLOAD should produce a doc");
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc_ptr) };
        let cp = unsafe { crate::parse::xmlNodeGetContent(root) };
        let got = unsafe { CStr::from_ptr(cp) }.to_str().unwrap_or("").to_string();
        unsafe {
            crate::parse::xmlFree(cp as *mut c_void);
            crate::parse::xmlFreeDoc(doc_ptr);
            forget_push_state(ctxt);
            crate::parsectx::xmlFreeParserCtxt(ctxt);
        }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            got.contains("PUSH_OPTIN_PAYLOAD"),
            "xmlCtxtUseOptions(DTDLOAD) failed to wire through to push parser: {got:?}"
        );
    }

    /// Security regression: the push parser must not load external
    /// entities by default.  Same XXE risk as `xmlReadMemory` — a
    /// caller who feeds untrusted XML through `xmlParseChunk`
    /// without any options-handshake should not see file contents
    /// substituted into entity references.
    #[test]
    fn push_parser_default_does_not_load_external_entity() {
        use std::io::Write;
        use std::ffi::CStr;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup-xml-xxe-push-{}.txt", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(b"PUSHSECRET").unwrap();
        }
        let src = format!(
            "<!DOCTYPE r [<!ENTITY x SYSTEM \"{}\">]><r>&x;</r>",
            tmp.display()
        );
        let src = src.into_bytes();

        let ctxt = unsafe {
            xmlCreatePushParserCtxt(ptr::null_mut(), ptr::null_mut(),
                                     ptr::null(), 0, ptr::null())
        };
        assert!(!ctxt.is_null());
        let _ = unsafe { xmlParseChunk(ctxt, src.as_ptr() as *const _, src.len() as c_int, 1) };
        let doc_ptr: *mut XmlDoc = unsafe {
            let p = (ctxt as *mut u8).add(16);
            let bytes = std::slice::from_raw_parts(p, 8);
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            usize::from_ne_bytes(arr) as *mut XmlDoc
        };
        // Same dual-outcome contract as the xmlReadMemory test:
        // NULL doc (strict reject) or doc with no leak are both
        // XXE-safe; doc with file contents is the bug.
        let got = if doc_ptr.is_null() {
            String::new()
        } else {
            let root = unsafe { crate::parse::xmlDocGetRootElement(doc_ptr) };
            if root.is_null() {
                String::new()
            } else {
                let cp = unsafe { crate::parse::xmlNodeGetContent(root) };
                let s = if cp.is_null() {
                    String::new()
                } else {
                    let s = unsafe { CStr::from_ptr(cp) }.to_str().unwrap_or("").to_string();
                    unsafe { crate::parse::xmlFree(cp as *mut c_void); }
                    s
                };
                s
            }
        };
        unsafe {
            if !doc_ptr.is_null() { crate::parse::xmlFreeDoc(doc_ptr); }
            forget_push_state(ctxt);
            crate::parsectx::xmlFreeParserCtxt(ctxt);
        }
        let _ = std::fs::remove_file(&tmp);
        assert!(
            !got.contains("PUSHSECRET"),
            "push-parser XXE: leaked file contents into entity expansion: {got:?}"
        );
    }
}
