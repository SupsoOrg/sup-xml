//! After-the-fact SAX-callback replay for the push parser.
//!
//! lxml's `iterparse(BytesIO(...))` collects start/end events by
//! installing handlers on `ctxt->sax->startElementNs` and
//! `ctxt->sax->endElementNs` and relying on libxml2 to invoke them
//! during chunked parsing.  Our push parser buffers all chunks until
//! `terminate=1` then parses in one shot — there's no natural
//! per-event hook.
//!
//! This module's [`replay`] walks the freshly-built tree in document
//! order and synthesises the same callbacks lxml would have seen from
//! a real streaming parse.  Pre-visit fires `startElementNs`, the
//! children are walked (text/CDATA → `characters`, comment →
//! `comment`), then post-visit fires `endElementNs`.  Before each
//! callback we plant the current node at `ctxt->node` (offset 80)
//! so lxml's `_elementFactory(context._doc, c_ctxt.node)` wraps the
//! right element when it builds its Python-side `Element`.
//!
//! # Why the no-op stubs in [`crate::parsectx`]
//!
//! lxml saves the pre-existing handler as `_origSaxStart` and then
//! installs its own wrapper.  Inside the wrapper it calls
//! `_origSaxStart(...)` to let the underlying parser do its
//! bookkeeping.  If `_origSaxStart` is NULL (the default if the SAX
//! block is zero-initialised), the call segfaults.  We install
//! [`noop_start_ns`] / [`noop_end_ns`] / [`noop_chars`] /
//! [`noop_comment`] etc. so lxml's captured pointer is a safe no-op.

use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_tree::dom::{Node, NodeKind, XmlDoc};

use crate::parsectx::XmlParserCtxt;

// ── SAX layout constants — match libxml2's _xmlSAXHandler ────────────────
//
// Each is the offset of a function pointer in the 256-byte SAX block
// that we attach to `XmlParserCtxt` via the `sax` pointer at offset 0
// of the ctxt itself.

const SAX_INTERNAL_SUBSET:         usize = 0;
const SAX_START_DOCUMENT:          usize = 96;
const SAX_END_DOCUMENT:            usize = 104;
const SAX_START_ELEMENT:           usize = 112; // SAX1 (HTML)
const SAX_END_ELEMENT:             usize = 120; // SAX1 (HTML)
const SAX_CHARACTERS:              usize = 136;
const SAX_PROCESSING_INSTRUCTION:  usize = 152;
const SAX_COMMENT:                 usize = 160;
const SAX_CDATA_BLOCK:             usize = 200;
const SAX_START_ELEMENT_NS:        usize = 232; // SAX2 (XML)
const SAX_END_ELEMENT_NS:          usize = 240; // SAX2 (XML)
/// `xmlSAXHandler.initialized` (offset 216).  When it holds
/// `XML_SAX2_MAGIC`, consumers install the SAX2 callbacks
/// (`startElementNs`/`endElementNs`); otherwise they fall back to the
/// SAX1 (`startElement`/`endElement`) slots.  lxml's
/// `XMLParser(target=…)` gates on this, so the handler block we hand
/// out must advertise SAX2.
const SAX_INITIALIZED:             usize = 216;
const XML_SAX2_MAGIC:              u32   = 0xDEED_BEAF;

const CTXT_SAX_OFFSET:  usize = 0;
const CTXT_NODE_OFFSET: usize = 80;

// libxml2's startElementNs SAX2 callback type.
type StartElementNsFn = unsafe extern "C" fn(
    ctx:            *mut c_void,
    localname:      *const c_char,
    prefix:         *const c_char,
    uri:            *const c_char,
    nb_namespaces:  c_int,
    namespaces:     *const *const c_char,
    nb_attributes:  c_int,
    nb_defaulted:   c_int,
    attributes:     *const *const c_char,
);

type EndElementNsFn = unsafe extern "C" fn(
    ctx:        *mut c_void,
    localname:  *const c_char,
    prefix:     *const c_char,
    uri:        *const c_char,
);

// libxml2's SAX1 startElement / endElement callbacks — used by the
// HTML parser, which is namespace-free.  `atts` is a NULL-terminated
// flat array [name0, val0, name1, val1, …, NULL], or NULL for none.
type StartElementFn = unsafe extern "C" fn(
    ctx:   *mut c_void,
    name:  *const c_char,
    atts:  *const *const c_char,
);

type EndElementFn = unsafe extern "C" fn(
    ctx:   *mut c_void,
    name:  *const c_char,
);

// libxml2's internalSubset SAX callback — fires the parsed `<!DOCTYPE>`
// (name, public id, system id) so a target's `doctype()` hook runs.
type InternalSubsetFn = unsafe extern "C" fn(
    ctx:         *mut c_void,
    name:        *const c_char,
    external_id: *const c_char,
    system_id:   *const c_char,
);

type CharactersFn = unsafe extern "C" fn(
    ctx:    *mut c_void,
    chars:  *const c_char,
    len:    c_int,
);

type CommentFn = unsafe extern "C" fn(
    ctx:    *mut c_void,
    value:  *const c_char,
);

/// libxml2 SAX `processingInstruction` callback — distinct from
/// `commentSAXFunc` because PIs carry both a target name AND data.
/// Calling a PI handler with the 2-arg comment signature corrupts
/// the data slot via the wrong register being read, then strlen
/// dereferences the garbage.
type PiFn = unsafe extern "C" fn(
    ctx:    *mut c_void,
    target: *const c_char,
    data:   *const c_char,
);

type DocumentFn = unsafe extern "C" fn(ctx: *mut c_void);

// ── no-op stubs ──────────────────────────────────────────────────────────

/// Default `startElementNs` slot — does nothing.  lxml saves this as
/// `_origSaxStart` and calls it inside its own wrapper; the call
/// must be safe to invoke even though we never want it to mutate
/// state (we've already built the tree).
pub unsafe extern "C" fn noop_start_ns(
    _ctx:           *mut c_void,
    _localname:     *const c_char,
    _prefix:        *const c_char,
    _uri:           *const c_char,
    _nb_namespaces: c_int,
    _namespaces:    *const *const c_char,
    _nb_attributes: c_int,
    _nb_defaulted:  c_int,
    _attributes:    *const *const c_char,
) {}

pub unsafe extern "C" fn noop_end_ns(
    _ctx:       *mut c_void,
    _localname: *const c_char,
    _prefix:    *const c_char,
    _uri:       *const c_char,
) {}

pub unsafe extern "C" fn noop_chars(
    _ctx:   *mut c_void,
    _chars: *const c_char,
    _len:   c_int,
) {}

pub unsafe extern "C" fn noop_comment(
    _ctx:   *mut c_void,
    _value: *const c_char,
) {}

pub unsafe extern "C" fn noop_pi(
    _ctx:    *mut c_void,
    _target: *const c_char,
    _data:   *const c_char,
) {}

pub unsafe extern "C" fn noop_start_element(
    _ctx:  *mut c_void,
    _name: *const c_char,
    _atts: *const *const c_char,
) {}

pub unsafe extern "C" fn noop_end_element(
    _ctx:  *mut c_void,
    _name: *const c_char,
) {}

/// Default `internalSubset` slot.  lxml saves this as `_origSaxDoctype`
/// *without* nulling it first (saxparser.pxi), then calls it inside
/// `_handleSaxTargetDoctype`; a NULL slot would segfault that call.
pub unsafe extern "C" fn noop_internal_subset(
    _ctx:         *mut c_void,
    _name:        *const c_char,
    _external_id: *const c_char,
    _system_id:   *const c_char,
) {}

pub unsafe extern "C" fn noop_document(_ctx: *mut c_void) {}

/// Write each no-op stub at its libxml2 offset within the 256-byte
/// SAX block.  Called from `xmlNewParserCtxt` so that lxml's
/// `_origSax*` captures are non-NULL.
///
/// # Safety
/// `sax` must point at a writable 256-byte buffer.
pub unsafe fn install_noop_sax_handlers(sax: *mut u8) {
    unsafe {
        write_ptr(sax, SAX_INTERNAL_SUBSET,        noop_internal_subset as *const ());
        write_ptr(sax, SAX_START_DOCUMENT,         noop_document       as *const ());
        write_ptr(sax, SAX_END_DOCUMENT,           noop_document       as *const ());
        write_ptr(sax, SAX_START_ELEMENT,          noop_start_element  as *const ());
        write_ptr(sax, SAX_END_ELEMENT,            noop_end_element    as *const ());
        write_ptr(sax, SAX_START_ELEMENT_NS,       noop_start_ns       as *const ());
        write_ptr(sax, SAX_END_ELEMENT_NS,         noop_end_ns         as *const ());
        write_ptr(sax, SAX_CHARACTERS,             noop_chars          as *const ());
        write_ptr(sax, SAX_COMMENT,                noop_comment        as *const ());
        write_ptr(sax, SAX_PROCESSING_INSTRUCTION, noop_pi             as *const ());
        write_ptr(sax, SAX_CDATA_BLOCK,            noop_chars          as *const ());
        // Advertise SAX2 so consumers install the namespace-aware
        // startElementNs/endElementNs callbacks (which `replay` fires).
        let magic = XML_SAX2_MAGIC.to_ne_bytes();
        std::ptr::copy_nonoverlapping(magic.as_ptr(), sax.add(SAX_INITIALIZED), magic.len());
    }
}

unsafe fn write_ptr(base: *mut u8, off: usize, p: *const ()) {
    unsafe {
        let bytes = (p as usize).to_ne_bytes();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(off), bytes.len());
    }
}

unsafe fn read_ptr(base: *const u8, off: usize) -> *mut c_void {
    let mut bytes = [0u8; std::mem::size_of::<usize>()];
    unsafe { std::ptr::copy_nonoverlapping(base.add(off), bytes.as_mut_ptr(), bytes.len()); }
    usize::from_ne_bytes(bytes) as *mut c_void
}

unsafe fn write_ctxt_node(ctxt: *mut XmlParserCtxt, node: *const Node<'static>) {
    unsafe {
        let bytes = (node as usize).to_ne_bytes();
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            (ctxt as *mut u8).add(CTXT_NODE_OFFSET),
            bytes.len(),
        );
    }
}

/// `xmlParserCtxt.myDoc` (offset 16) and the `last`(-child) pointer at
/// offset 32 shared by `xmlNode`/`xmlDoc`.  Used to make a consumer's
/// `_findLastEventNode`-style lookup resolve to the node we're firing.
const CTXT_MYDOC_OFFSET: usize = 16;
const LAST_CHILD_OFFSET: usize = 32;

/// Fire a comment / PI callback so the consumer's "last event node"
/// lookup resolves to THIS node.  lxml's `_findLastEventNode` returns
/// `ctxt.node.last` when `ctxt.node` is an element, else `myDoc.last`
/// when `ctxt.node` is NULL — both assuming the incremental parser just
/// appended the node as the relevant `last` child.  Our tree is already
/// fully built, so point that `last` slot (offset 32, shared by `xmlNode`
/// and `xmlDoc`) at `node` for the duration of the call, then restore it.
/// `myDoc` must already be planted (the push parser does so before replay).
unsafe fn fire_misc_event(ctx: *mut c_void, node: &Node<'static>, fire: impl FnOnce()) {
    let ctxt = ctx as *mut XmlParserCtxt;
    let node_ptr = node as *const Node<'static>;
    // Pick the object whose `last` lxml reads and the `ctxt.node` value
    // that selects the matching branch.
    let (obj, ctx_node): (*mut u8, *const Node<'static>) = match node.parent.get() {
        Some(p) if matches!(p.kind, NodeKind::Element) => (
            p as *const Node<'static> as *mut u8,
            p as *const Node<'static>,
        ),
        _ => (
            unsafe { read_ptr(ctxt as *const u8, CTXT_MYDOC_OFFSET) } as *mut u8,
            ptr::null(),
        ),
    };
    unsafe { write_ctxt_node(ctxt, ctx_node); }
    if obj.is_null() {
        fire();
        return;
    }
    // Swap `obj.last` → node, fire, restore.
    let slot = unsafe { obj.add(LAST_CHILD_OFFSET) };
    let mut saved = [0u8; std::mem::size_of::<usize>()];
    unsafe { std::ptr::copy_nonoverlapping(slot, saved.as_mut_ptr(), saved.len()); }
    let nb = (node_ptr as usize).to_ne_bytes();
    unsafe { std::ptr::copy_nonoverlapping(nb.as_ptr(), slot, nb.len()); }
    fire();
    unsafe { std::ptr::copy_nonoverlapping(saved.as_ptr(), slot, saved.len()); }
}

// ── replay entry point ──────────────────────────────────────────────────

/// Whether a consumer has installed a real SAX2 element handler on
/// `ctxt->sax` (i.e. replaced our `noop_start_ns` baseline).  This is
/// the signal that the caller — lxml's `XMLParser(target=…)` or
/// `iterparse` — wants events dispatched, and is what gates the push
/// parser's speculative replay-on-feed (otherwise a plain
/// buffer-then-doc consumer would pay a re-parse on every chunk).
///
/// # Safety
/// `ctxt` must be NULL or a live `XmlParserCtxt`.
pub(crate) unsafe fn has_event_handlers(ctxt: *const XmlParserCtxt) -> bool {
    if ctxt.is_null() { return false; }
    let sax = unsafe { read_ptr(ctxt as *const u8, CTXT_SAX_OFFSET) };
    if sax.is_null() { return false; }
    let sb = sax as *const u8;
    let active = |off: usize, baseline: *const ()| -> bool {
        let p = unsafe { read_ptr(sb, off) };
        !p.is_null() && p != baseline as *mut c_void
    };
    // Any of the structure / markup callbacks being non-baseline means a
    // consumer wants events (an `XMLPullParser` with `events=('comment',)`
    // installs only the comment handler, etc.).  `characters` is
    // deliberately excluded: a plain tree-building feed parser leaves the
    // structure handlers at baseline but may carry a text handler, and we
    // must not mistake it for an event consumer.
    active(SAX_START_ELEMENT_NS, noop_start_ns as *const ())
        || active(SAX_END_ELEMENT_NS, noop_end_ns as *const ())
        || active(SAX_COMMENT, noop_comment as *const ())
        || active(SAX_PROCESSING_INSTRUCTION, noop_pi as *const ())
}

/// Fire `ctxt->sax->startDocument` once, if the consumer installed a
/// real handler (not our no-op baseline).  lxml's `_initSaxDocument`
/// runs here to create `doc->ids` when `collect_ids` is on; on a
/// normal (non-event) parse nothing else drives it, since we build the
/// tree natively rather than through SAX.  `ctxt->myDoc` must already
/// be planted — the callback reads it.
///
/// # Safety
/// `ctxt` must be NULL or a live `XmlParserCtxt`.
pub(crate) unsafe fn fire_start_document(ctxt: *mut XmlParserCtxt) {
    if ctxt.is_null() {
        return;
    }
    let sax = unsafe { read_ptr(ctxt as *const u8, CTXT_SAX_OFFSET) };
    if sax.is_null() {
        return;
    }
    let p = unsafe { read_ptr(sax as *const u8, SAX_START_DOCUMENT) };
    if p.is_null() || p == noop_document as *mut c_void {
        return;
    }
    // SAFETY: the slot holds libxml2's `startDocumentSAXFunc`.
    let f: DocumentFn = unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(p) };
    unsafe { f(ctxt as *mut c_void); }
}

/// Synthesise SAX2 callbacks for every element/text/comment in the
/// parsed tree.  Safe no-op if `ctxt` is NULL, `sax` is NULL on the
/// ctxt, or `doc` has no element child.
///
/// We only fire callbacks that have been *replaced* away from our
/// no-op stubs — that's the signal a consumer (lxml) installed an
/// actual handler.  Firing the no-op stubs is harmless but wastes
/// cycles, and on a tree of millions of nodes that matters.
///
/// # Safety
/// `ctxt` must be NULL or a live `XmlParserCtxt`.  `doc` must be
/// NULL or a live `XmlDoc` that was built by the current parse.
pub unsafe fn replay(ctxt: *mut XmlParserCtxt, doc: *mut XmlDoc, sax1: bool) {
    if ctxt.is_null() || doc.is_null() { return; }
    let sax = unsafe { read_ptr(ctxt as *const u8, CTXT_SAX_OFFSET) };
    if sax.is_null() { return; }
    let sax_bytes = sax as *const u8;

    // HTML consumers install SAX1 handlers (startElement/endElement,
    // no namespaces) — libxml2's HTML parser is SAX1-only.  Replay
    // those instead of the SAX2 callbacks below.
    if sax1 {
        unsafe { replay_sax1(ctxt, doc, sax_bytes); }
        return;
    }

    // Snapshot the handlers we care about, transmuting only if they
    // differ from our no-op baseline.
    let startdoc_p = unsafe { read_ptr(sax_bytes, SAX_START_DOCUMENT)    };
    let enddoc_p   = unsafe { read_ptr(sax_bytes, SAX_END_DOCUMENT)      };
    let start_p    = unsafe { read_ptr(sax_bytes, SAX_START_ELEMENT_NS)  };
    let end_p      = unsafe { read_ptr(sax_bytes, SAX_END_ELEMENT_NS)    };
    let chars_p    = unsafe { read_ptr(sax_bytes, SAX_CHARACTERS)        };
    let cdata_p    = unsafe { read_ptr(sax_bytes, SAX_CDATA_BLOCK)       };
    let comm_p     = unsafe { read_ptr(sax_bytes, SAX_COMMENT)           };
    let pi_p       = unsafe { read_ptr(sax_bytes, SAX_PROCESSING_INSTRUCTION) };

    let baseline_document: *mut c_void = noop_document  as *mut c_void;
    let baseline_start:    *mut c_void = noop_start_ns  as *mut c_void;
    let baseline_end:      *mut c_void = noop_end_ns    as *mut c_void;
    let baseline_chars:    *mut c_void = noop_chars     as *mut c_void;
    let baseline_comm:     *mut c_void = noop_comment   as *mut c_void;

    // If the consumer hasn't replaced any of the start/end handlers,
    // they don't care about events — bail.
    if start_p == baseline_start && end_p == baseline_end {
        return;
    }

    let startdoc_fn: Option<DocumentFn> = if startdoc_p == baseline_document || startdoc_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(startdoc_p) })
    };
    let enddoc_fn: Option<DocumentFn> = if enddoc_p == baseline_document || enddoc_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(enddoc_p) })
    };

    // SAFETY: the pointers came from a writable SAX block; their
    // concrete signatures are libxml2's standard SAX2 callbacks.
    let start_fn: Option<StartElementNsFn> = if start_p == baseline_start || start_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, StartElementNsFn>(start_p) })
    };
    let end_fn: Option<EndElementNsFn> = if end_p == baseline_end || end_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, EndElementNsFn>(end_p) })
    };
    let chars_fn: Option<CharactersFn> = if chars_p == baseline_chars || chars_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(chars_p) })
    };
    let cdata_fn: Option<CharactersFn> = if cdata_p == baseline_chars || cdata_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(cdata_p) })
    };
    let comm_fn: Option<CommentFn> = if comm_p == baseline_comm || comm_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, CommentFn>(comm_p) })
    };
    let baseline_pi: *mut c_void = noop_pi as *mut c_void;
    let pi_fn: Option<PiFn> = if pi_p == baseline_pi || pi_p.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute::<*mut c_void, PiFn>(pi_p) })
    };

    // Fire startDocument BEFORE element events.  lxml's
    // _handleSaxStartDocument sets `context._doc = _documentFactory(...)`
    // which is what _pushSaxStartEvent's `assert context._doc is not None`
    // depends on.  Without this, every element-start callback would
    // hit that assert.
    if let Some(f) = startdoc_fn {
        unsafe { f(ctxt as *mut c_void); }
    }

    // Find the document's element root via XmlDoc->children walk.
    // SAFETY: doc is non-null.
    let root_ptr: *mut Node<'static> = unsafe { (*doc).children.get() };
    if !root_ptr.is_null() {
        let mut cursor = EventCursor::all();
        let mut cur: Option<&Node<'static>> = Some(unsafe { &*(root_ptr as *const Node<'static>) });
        while let Some(n) = cur {
            unsafe {
                visit(ctxt as *mut c_void, n, &mut cursor, start_fn, end_fn, chars_fn, cdata_fn, comm_fn, pi_fn);
            }
            cur = n.next_sibling.get();
        }
    }

    if let Some(f) = enddoc_fn {
        unsafe { f(ctxt as *mut c_void); }
    }
}

/// Replay only the document-order events in `[skip, skip + count)` from
/// `doc`, firing the consumer's SAX2 handlers for each.  `start_document`
/// / `end_document` are fired only when their flags are set (the caller
/// fires `startDocument` on the first incremental batch and `endDocument`
/// at close).  Returns the number of events fired *through* — i.e.
/// `skip + (events actually in range)` — capped at the document's total
/// event count, so the caller can advance its "already fired" cursor.
///
/// Used by the incremental push parser ([`crate::pushparse`]) to emit the
/// start/end events that became complete since the previous `feed()`,
/// without re-firing earlier ones.
///
/// # Safety
/// `ctxt`/`doc` must be live; `doc` is an XML (SAX2) tree.
pub(crate) unsafe fn replay_range(
    ctxt:           *mut XmlParserCtxt,
    doc:            *mut XmlDoc,
    skip:           usize,
    count:          usize,
    start_document: bool,
    end_document:   bool,
) -> usize {
    if ctxt.is_null() || doc.is_null() { return skip; }
    let sax = unsafe { read_ptr(ctxt as *const u8, CTXT_SAX_OFFSET) };
    if sax.is_null() { return skip; }
    let sax_bytes = sax as *const u8;

    let start_p = unsafe { read_ptr(sax_bytes, SAX_START_ELEMENT_NS) };
    let end_p   = unsafe { read_ptr(sax_bytes, SAX_END_ELEMENT_NS)   };
    let nonbaseline = |p: *mut c_void, base: *const ()| p != base as *mut c_void && !p.is_null();
    let start_fn: Option<StartElementNsFn> = nonbaseline(start_p, noop_start_ns as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, StartElementNsFn>(start_p) });
    let end_fn: Option<EndElementNsFn> = nonbaseline(end_p, noop_end_ns as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, EndElementNsFn>(end_p) });
    let chars_p = unsafe { read_ptr(sax_bytes, SAX_CHARACTERS) };
    let cdata_p = unsafe { read_ptr(sax_bytes, SAX_CDATA_BLOCK) };
    let comm_p  = unsafe { read_ptr(sax_bytes, SAX_COMMENT) };
    let pi_p    = unsafe { read_ptr(sax_bytes, SAX_PROCESSING_INSTRUCTION) };
    let chars_fn: Option<CharactersFn> = nonbaseline(chars_p, noop_chars as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(chars_p) });
    let cdata_fn: Option<CharactersFn> = nonbaseline(cdata_p, noop_chars as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(cdata_p) });
    let comm_fn: Option<CommentFn> = nonbaseline(comm_p, noop_comment as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CommentFn>(comm_p) });
    let pi_fn: Option<PiFn> = nonbaseline(pi_p, noop_pi as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, PiFn>(pi_p) });

    if start_document {
        let sd_p = unsafe { read_ptr(sax_bytes, SAX_START_DOCUMENT) };
        if nonbaseline(sd_p, noop_document as *const ()) {
            let f: DocumentFn = unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(sd_p) };
            unsafe { f(ctxt as *mut c_void); }
        }
    }

    let mut cursor = EventCursor { idx: 0, lo: skip, hi: skip.saturating_add(count) };
    let root_ptr: *mut Node<'static> = unsafe { (*doc).children.get() };
    if !root_ptr.is_null() {
        let mut cur: Option<&Node<'static>> = Some(unsafe { &*(root_ptr as *const Node<'static>) });
        while let Some(n) = cur {
            unsafe {
                visit(ctxt as *mut c_void, n, &mut cursor, start_fn, end_fn, chars_fn, cdata_fn, comm_fn, pi_fn);
            }
            cur = n.next_sibling.get();
        }
    }

    if end_document {
        let ed_p = unsafe { read_ptr(sax_bytes, SAX_END_DOCUMENT) };
        if nonbaseline(ed_p, noop_document as *const ()) {
            let f: DocumentFn = unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(ed_p) };
            unsafe { f(ctxt as *mut c_void); }
        }
    }
    // `idx` is now the document's total event count; the cursor fired
    // everything below `hi`, so the caller has now seen min(total, hi).
    cursor.idx.min(cursor.hi)
}

/// Cursor selecting a sub-range of a replay's event stream.
///
/// Every fireable SAX event (element start, element end, text, CDATA,
/// comment, PI) advances `idx`; the callback runs only when `idx` lies
/// in `[lo, hi)`.  A full replay uses `lo=0, hi=usize::MAX` (everything
/// fires).  Incremental push parsing uses it to fire only the events
/// that became complete since the previous `feed()` — see
/// [`replay_range`].
struct EventCursor {
    idx: usize,
    lo:  usize,
    hi:  usize,
}

impl EventCursor {
    fn all() -> Self { Self { idx: 0, lo: 0, hi: usize::MAX } }

    /// Account for one event in document order; return whether its
    /// callback should fire (index within `[lo, hi)`).
    #[inline]
    fn take(&mut self) -> bool {
        let fire = self.idx >= self.lo && self.idx < self.hi;
        self.idx += 1;
        fire
    }
}

unsafe fn visit(
    ctx:      *mut c_void,
    node:     &Node<'static>,
    cursor:   &mut EventCursor,
    start_fn: Option<StartElementNsFn>,
    end_fn:   Option<EndElementNsFn>,
    chars_fn: Option<CharactersFn>,
    cdata_fn: Option<CharactersFn>,
    comm_fn:  Option<CommentFn>,
    pi_fn:    Option<PiFn>,
) {
    match node.kind {
        NodeKind::Element => {
            // Name + element namespace are needed by both the start and
            // end callbacks, so resolve them once (cheap pointer reads).
            //
            // The dict-canonical name pointer (read straight out of
            // `Node::name`, not a fresh CString) is load-bearing: lxml's
            // `_MultiTagMatcher` (`iterparse(..., tag=…)`) pointer-compares
            // tags against the address `xmlDictLookup` returns, so the SAX
            // callbacks must yield that same dict address or tag-filtering
            // silently drops every event.
            #[cfg(feature = "c-abi")]
            let local_ptr = node.name.as_ptr() as *const c_char;
            #[cfg(not(feature = "c-abi"))]
            let _: () = compile_error!(
                "sup-xml-compat must be built with the `c-abi` feature \
                 (declared in its Cargo.toml) — every ABI symbol relies on \
                 the libxml2-layout `Node`/`Attribute` types from \
                 `sup-xml-tree`'s c-abi build"
            );
            let (el_prefix, el_uri) = match node.namespace.get() {
                Some(ns) => (
                    ns.prefix.map(|p| p.as_ptr() as *const c_char)
                        .unwrap_or(ptr::null()),
                    ns.href.as_ptr() as *const c_char,
                ),
                None => (ptr::null(), ptr::null()),
            };

            if cursor.take() {
                // Plant ctxt->node so lxml's _elementFactory wraps THIS
                // element, then fire startElementNs.
                unsafe {
                    write_ctxt_node(ctx as *mut XmlParserCtxt, node as *const Node<'static>);
                }
                // Namespace channel: any `xmlns` decls on this element as a
                // flat [prefix0, uri0, prefix1, uri1, …] array.  Without it,
                // namespace-scoped tag filters silently match nothing.
                let mut ns_decls: Vec<*const c_char> = Vec::new();
                let mut cur_ns = node.ns_def.get();
                while let Some(ns) = cur_ns {
                    ns_decls.push(
                        ns.prefix.map(|p| p.as_ptr() as *const c_char)
                            .unwrap_or(ptr::null()),
                    );
                    ns_decls.push(ns.href.as_ptr() as *const c_char);
                    cur_ns = ns.next.get();
                }
                let nb_namespaces = (ns_decls.len() / 2) as c_int;
                let ns_ptr: *const *const c_char = if ns_decls.is_empty() {
                    ptr::null()
                } else {
                    ns_decls.as_ptr()
                };
                // Attributes channel: 5 entries per attribute — localname,
                // prefix, URI, value start, value end.  The value is a
                // `[start, end)` byte range (NOT NUL-terminated), per
                // libxml2's startElementNs contract.
                let mut attrs: Vec<*const c_char> = Vec::new();
                for attr in node.attributes() {
                    let (a_prefix, a_uri) = match attr.namespace.get() {
                        Some(ns) => (
                            ns.prefix.map(|p| p.as_ptr() as *const c_char).unwrap_or(ptr::null()),
                            ns.href.as_ptr() as *const c_char,
                        ),
                        None => (ptr::null(), ptr::null()),
                    };
                    let val = attr.value();
                    let v_start = val.as_ptr() as *const c_char;
                    // SAFETY: v_start points at `val.len()` valid bytes in
                    // the arena; one-past-the-end is a valid range bound.
                    let v_end = unsafe { v_start.add(val.len()) };
                    attrs.push(attr.name().as_ptr() as *const c_char);
                    attrs.push(a_prefix);
                    attrs.push(a_uri);
                    attrs.push(v_start);
                    attrs.push(v_end);
                }
                let nb_attributes = (attrs.len() / 5) as c_int;
                let attrs_ptr: *const *const c_char = if attrs.is_empty() {
                    ptr::null()
                } else {
                    attrs.as_ptr()
                };
                if let Some(f) = start_fn {
                    unsafe {
                        f(ctx, local_ptr, el_prefix, el_uri,
                          nb_namespaces, ns_ptr,
                          nb_attributes, 0, attrs_ptr);
                    }
                }
                // Both Vecs must outlive the callback above.
                drop(attrs);
                drop(ns_decls);
            }
            // Recurse into children (always — keeps the cursor advancing
            // through their events even when this element didn't fire).
            let mut child = node.first_child.get();
            while let Some(c) = child {
                unsafe {
                    visit(ctx, c, cursor, start_fn, end_fn, chars_fn, cdata_fn, comm_fn, pi_fn);
                }
                child = c.next_sibling.get();
            }
            if cursor.take() {
                // Re-plant ctxt->node before endElementNs (children moved it).
                unsafe {
                    write_ctxt_node(ctx as *mut XmlParserCtxt, node as *const Node<'static>);
                }
                if let Some(f) = end_fn {
                    unsafe {
                        f(ctx, local_ptr, el_prefix, el_uri);
                    }
                }
            }
        }
        NodeKind::Text => {
            if cursor.take() {
                if let Some(f) = chars_fn {
                    let s = node.content();
                    unsafe { f(ctx, s.as_ptr() as *const c_char, s.len() as c_int); }
                }
            }
        }
        NodeKind::CData => {
            if cursor.take() {
                if let Some(f) = cdata_fn {
                    let s = node.content();
                    unsafe { f(ctx, s.as_ptr() as *const c_char, s.len() as c_int); }
                }
            }
        }
        NodeKind::Comment => {
            if cursor.take() {
                if let Some(f) = comm_fn {
                    let s = node.content();
                    let c = std::ffi::CString::new(s).unwrap_or_default();
                    unsafe { fire_misc_event(ctx, node, || f(ctx, c.as_ptr())); }
                }
            }
        }
        NodeKind::Pi => {
            if cursor.take() {
                if let Some(f) = pi_fn {
                    let target = std::ffi::CString::new(node.name()).unwrap_or_default();
                    let data   = std::ffi::CString::new(node.content()).unwrap_or_default();
                    unsafe {
                        fire_misc_event(ctx, node, || f(ctx, target.as_ptr(), data.as_ptr()));
                    }
                }
            }
        }
        _ => {}
    }
}

// ── SAX1 replay (HTML) ───────────────────────────────────────────────────

/// Read a `*const c_char` field at `off` within a libxml2 `xmlDtd`.
unsafe fn dtd_field(dtd: *const u8, off: usize) -> *const c_char {
    unsafe { read_ptr(dtd, off) as *const c_char }
}

/// Synthesise SAX1 callbacks for an HTML push parse.  Mirrors
/// [`replay`] but fires `startElement`/`endElement` (no namespaces)
/// plus the `internalSubset` doctype callback, matching libxml2's
/// namespace-free HTML parser.
unsafe fn replay_sax1(ctxt: *mut XmlParserCtxt, doc: *mut XmlDoc, sax_bytes: *const u8) {
    let start_p    = unsafe { read_ptr(sax_bytes, SAX_START_ELEMENT)         };
    let end_p      = unsafe { read_ptr(sax_bytes, SAX_END_ELEMENT)           };
    let isub_p     = unsafe { read_ptr(sax_bytes, SAX_INTERNAL_SUBSET)       };
    let startdoc_p = unsafe { read_ptr(sax_bytes, SAX_START_DOCUMENT)        };
    let enddoc_p   = unsafe { read_ptr(sax_bytes, SAX_END_DOCUMENT)          };
    let chars_p    = unsafe { read_ptr(sax_bytes, SAX_CHARACTERS)            };
    let cdata_p    = unsafe { read_ptr(sax_bytes, SAX_CDATA_BLOCK)           };
    let comm_p     = unsafe { read_ptr(sax_bytes, SAX_COMMENT)               };
    let pi_p       = unsafe { read_ptr(sax_bytes, SAX_PROCESSING_INSTRUCTION) };

    // A handler counts as "installed" only when it differs from the
    // no-op baseline we planted at ctxt creation — that's the signal
    // a consumer (lxml) overwrote it with a real callback.
    let nonbaseline = |p: *mut c_void, base: *const ()| -> bool {
        !p.is_null() && p != base as *mut c_void
    };
    let start_fn: Option<StartElementFn> = nonbaseline(start_p, noop_start_element as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, StartElementFn>(start_p) });
    let end_fn: Option<EndElementFn> = nonbaseline(end_p, noop_end_element as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, EndElementFn>(end_p) });

    // Nothing wired for element events → consumer doesn't want a replay.
    if start_fn.is_none() && end_fn.is_none() {
        return;
    }

    let isub_fn: Option<InternalSubsetFn> = nonbaseline(isub_p, noop_internal_subset as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, InternalSubsetFn>(isub_p) });
    let startdoc_fn: Option<DocumentFn> = nonbaseline(startdoc_p, noop_document as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(startdoc_p) });
    let enddoc_fn: Option<DocumentFn> = nonbaseline(enddoc_p, noop_document as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, DocumentFn>(enddoc_p) });
    let chars_fn: Option<CharactersFn> = nonbaseline(chars_p, noop_chars as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(chars_p) });
    let cdata_fn: Option<CharactersFn> = nonbaseline(cdata_p, noop_chars as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CharactersFn>(cdata_p) });
    let comm_fn: Option<CommentFn> = nonbaseline(comm_p, noop_comment as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, CommentFn>(comm_p) });
    let pi_fn: Option<PiFn> = nonbaseline(pi_p, noop_pi as *const ())
        .then(|| unsafe { std::mem::transmute::<*mut c_void, PiFn>(pi_p) });

    let ctx = ctxt as *mut c_void;

    if let Some(f) = startdoc_fn {
        unsafe { f(ctx); }
    }

    // internalSubset fires after startDocument, before the root —
    // read the parsed doctype from doc->intSubset (offset 80).
    if let Some(f) = isub_fn {
        let dtd = unsafe { read_ptr(doc as *const u8, 80) } as *const u8;
        if !dtd.is_null() {
            // libxml2's HTML parser leaves `dtd->name` NULL for a nameless
            // `<!DOCTYPE>`; html5ever instead yields an empty name.  Pass
            // NULL so a consumer (lxml's target `doctype()`) sees `None`,
            // not `''`.
            let name = match unsafe { dtd_field(dtd, 16) } {
                p if !p.is_null() && unsafe { *p } == 0 => std::ptr::null(),
                p => p,
            };
            let ext  = unsafe { dtd_field(dtd, 104) };
            let sys  = unsafe { dtd_field(dtd, 112) };
            unsafe { f(ctx, name, ext, sys); }
        }
    }

    let root_ptr: *mut Node<'static> = unsafe { (*doc).children.get() };
    if !root_ptr.is_null() {
        let mut cur: Option<&Node<'static>> = Some(unsafe { &*(root_ptr as *const Node<'static>) });
        while let Some(n) = cur {
            unsafe { visit_sax1(ctx, n, start_fn, end_fn, chars_fn, cdata_fn, comm_fn, pi_fn); }
            cur = n.next_sibling.get();
        }
    }

    if let Some(f) = enddoc_fn {
        unsafe { f(ctx); }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn visit_sax1(
    ctx:      *mut c_void,
    node:     &Node<'static>,
    start_fn: Option<StartElementFn>,
    end_fn:   Option<EndElementFn>,
    chars_fn: Option<CharactersFn>,
    cdata_fn: Option<CharactersFn>,
    comm_fn:  Option<CommentFn>,
    pi_fn:    Option<PiFn>,
) {
    match node.kind {
        NodeKind::Element => {
            unsafe { write_ctxt_node(ctx as *mut XmlParserCtxt, node as *const Node<'static>); }
            // Element name — local part (HTML is namespace-free); the
            // c-abi `Node::name` is a NUL-terminated dict pointer.
            #[cfg(feature = "c-abi")]
            let name_ptr = node.name.as_ptr() as *const c_char;
            // Flatten attributes into libxml2's [name0, val0, …, NULL]
            // array.  The CStrings own the bytes for the call's duration.
            let mut owned: Vec<std::ffi::CString> = Vec::new();
            for attr in node.attributes() {
                owned.push(std::ffi::CString::new(attr.name()).unwrap_or_default());
                owned.push(std::ffi::CString::new(attr.value()).unwrap_or_default());
            }
            let mut atts: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
            let atts_ptr: *const *const c_char = if atts.is_empty() {
                ptr::null()
            } else {
                atts.push(ptr::null());
                atts.as_ptr()
            };
            if let Some(f) = start_fn {
                unsafe { f(ctx, name_ptr, atts_ptr); }
            }
            drop(atts);
            drop(owned);

            let mut child = node.first_child.get();
            while let Some(c) = child {
                unsafe { visit_sax1(ctx, c, start_fn, end_fn, chars_fn, cdata_fn, comm_fn, pi_fn); }
                child = c.next_sibling.get();
            }

            unsafe { write_ctxt_node(ctx as *mut XmlParserCtxt, node as *const Node<'static>); }
            if let Some(f) = end_fn {
                unsafe { f(ctx, name_ptr); }
            }
        }
        NodeKind::Text => {
            if let Some(f) = chars_fn {
                let s = node.content();
                unsafe { f(ctx, s.as_ptr() as *const c_char, s.len() as c_int); }
            }
        }
        NodeKind::CData => {
            if let Some(f) = cdata_fn {
                let s = node.content();
                unsafe { f(ctx, s.as_ptr() as *const c_char, s.len() as c_int); }
            }
        }
        NodeKind::Comment => {
            if let Some(f) = comm_fn {
                let c = std::ffi::CString::new(node.content()).unwrap_or_default();
                unsafe { f(ctx, c.as_ptr()); }
            }
        }
        NodeKind::Pi => {
            if let Some(f) = pi_fn {
                let target = std::ffi::CString::new(node.name()).unwrap_or_default();
                let data   = std::ffi::CString::new(node.content()).unwrap_or_default();
                unsafe { f(ctx, target.as_ptr(), data.as_ptr()); }
            }
        }
        _ => {}
    }
}
