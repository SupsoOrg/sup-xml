//! Incremental tree building for the push parser.
//!
//! lxml's `XMLPullParser` / `iterparse` feed bytes a chunk at a time and
//! expect SAX events (`start`, `end`, `comment`, …) to fire as soon as
//! each token completes — and, crucially, the element an event carries
//! must be the *same object* that appears in the finished tree
//! (`iterparse` compares them by identity).  That rules out re-parsing
//! the buffer into a throwaway tree per feed.
//!
//! Instead we grow **one persistent document** ([`PushState::inc_doc`]).
//! On every feed we re-tokenize the buffer's complete prefix (cheap —
//! tokenizing, not tree-building), skip the events already applied, and
//! build only the new nodes into the persistent tree.  The
//! [`open_stack`](PushState::open_stack) of currently-open elements
//! carries the parent context across feeds, so a start tag in one feed
//! and its children in the next attach to the same node.  Firing then
//! walks that stable tree (see [`crate::saxreplay::replay_range`]).
//!
//! Names are interned through the shared thread dict so element-name
//! pointers stay dict-canonical for lxml's tag matcher.
//!
//! Re-tokenizing the whole prefix each feed is `O(n²)` in chunk count;
//! the correctness-critical tree build is `O(n)` (each node once).  A
//! resumable reader would drop the re-tokenize to `O(n)`, but that needs
//! reader-internal checkpointing and is a separate optimization.

use std::ffi::CString;
use std::ptr;

use sup_xml_tree::dom::{Node, XmlDoc};

/// Create an empty document whose embedded arena/dict are the shared
/// thread dict (so node names interned during the incremental build are
/// pointer-equal to what other parses and lxml's tag matcher produce)
/// and a per-document arena held by the thread keep-alive.
pub(crate) fn new_incremental_doc() -> *mut XmlDoc {
    let version = std::ffi::CString::new("1.0").unwrap();
    // SAFETY: a NUL-terminated version string.
    unsafe { crate::mutate::xmlNewDoc(version.as_ptr()) }
}

/// Split a `prefix:local` qname (as raw bytes) into its parts.  No colon
/// means the whole thing is the local name and there is no prefix.
fn split_qname(qname: &[u8]) -> (Option<&[u8]>, &[u8]) {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => (Some(&qname[..i]), &qname[i + 1..]),
        None => (None, qname),
    }
}

fn cstr(bytes: &[u8]) -> CString {
    // Element/attribute names and values are NUL-free in well-formed XML;
    // default to empty on the impossible NUL case rather than panicking.
    CString::new(bytes).unwrap_or_default()
}

/// Build one start-tag element into `doc` under `parent` (or as the
/// document root when `parent` is NULL), resolving its namespace and
/// attributes through the live tree.  Returns the new element.
///
/// # Safety
/// `doc` is a live incremental document; `parent` is NULL or a live
/// element node within it.
unsafe fn build_element(
    doc:    *mut XmlDoc,
    parent: *mut Node<'static>,
    tag:    sup_xml_core::xml_bytes_reader::BytesStartTag<'_, '_>,
    line:   u32,
) -> *mut Node<'static> {
    // Copy the qname out before `attrs()` (which consumes `tag`).
    let qname = tag.name().to_vec();
    let (prefix, local) = split_qname(&qname);
    let local_c = cstr(local);
    let elem = unsafe { crate::mutate::xmlNewDocNode(doc, ptr::null_mut(), local_c.as_ptr(), ptr::null()) };
    if elem.is_null() {
        return ptr::null_mut();
    }
    // `Element.sourceline` — the c-abi node stores the line in a u16;
    // `full_line` keeps the uncapped value for files past 65535 lines.
    unsafe {
        (*elem).line = line.min(u16::MAX as u32) as u16;
        (*elem).full_line = line;
    }
    let parent_node = if parent.is_null() { doc as *mut Node<'static> } else { parent };
    unsafe { crate::mutate::xmlAddChild(parent_node, elem); }

    // First pass: install the element's own `xmlns` declarations so the
    // namespace lookups below (for the element and its prefixed
    // attributes) can see them.  Regular attributes are deferred.
    let mut regular: Vec<(Option<Vec<u8>>, Vec<u8>, Vec<u8>)> = Vec::new();
    for attr in tag.attrs() {
        let Ok(attr) = attr else { continue };
        let name = attr.name();
        if name == b"xmlns" {
            let href = cstr(attr.value());
            unsafe { crate::mutate::xmlNewNs(elem, href.as_ptr(), ptr::null()); }
        } else if let Some(p) = name.strip_prefix(b"xmlns:".as_slice()) {
            let href = cstr(attr.value());
            let pfx = cstr(p);
            unsafe { crate::mutate::xmlNewNs(elem, href.as_ptr(), pfx.as_ptr()); }
        } else {
            let (ap, al) = split_qname(name);
            regular.push((ap.map(|s| s.to_vec()), al.to_vec(), attr.value().to_vec()));
        }
    }

    // Resolve the element's own binding: a prefix searches by that
    // prefix, no prefix picks up the in-scope default namespace.
    let elem_ns = match prefix {
        Some(p) => {
            let pc = cstr(p);
            unsafe { crate::ns::xmlSearchNs(doc, elem, pc.as_ptr()) }
        }
        None => unsafe { crate::ns::xmlSearchNs(doc, elem, ptr::null()) },
    };
    if !elem_ns.is_null() {
        unsafe { crate::mutate::xmlSetNs(elem, elem_ns); }
    }

    // Attributes: an unprefixed attribute is in no namespace (XML
    // Namespaces §6.2 — the default namespace does not apply to
    // attributes); a prefixed one resolves through the element's scope.
    for (ap, al, av) in &regular {
        let al_c = cstr(al);
        let av_c = cstr(av);
        let ns = match ap {
            Some(p) => {
                let pc = cstr(p);
                unsafe { crate::ns::xmlSearchNs(doc, elem, pc.as_ptr()) }
            }
            None => ptr::null_mut(),
        };
        unsafe { crate::mutate::xmlNewNsProp(elem, ns, al_c.as_ptr(), av_c.as_ptr()); }
    }
    elem
}

/// Append a leaf node (text / CDATA / comment / PI) built from `make`
/// under `parent` (or the document when NULL).
unsafe fn append_leaf(doc: *mut XmlDoc, parent: *mut Node<'static>, node: *mut Node<'static>) {
    if node.is_null() { return; }
    let parent_node = if parent.is_null() { doc as *mut Node<'static> } else { parent };
    unsafe { crate::mutate::xmlAddChild(parent_node, node); }
    let _ = doc;
}

/// Re-tokenize the complete prefix of `buf` and build the events past
/// `events_built` into the persistent document `doc`, threading the
/// `open_stack` of currently-open elements across calls.  Returns the
/// total number of complete (fireable) events in the prefix (the new
/// fire watermark) and whether the buffer parsed to a clean EOF — i.e. a
/// complete, well-formed document, which the close uses to set the
/// `wellFormed` flag.
///
/// # Safety
/// `doc` is a live incremental document; `open_stack` holds live element
/// nodes within it from previous calls.
pub(crate) unsafe fn build_prefix(
    doc:          *mut XmlDoc,
    open_stack:   &mut Vec<*mut Node<'static>>,
    events_built: usize,
    raw:          &[u8],
    opts:         &sup_xml_core::options::ParseOptions,
) -> (usize, bool, Option<sup_xml_core::error::XmlError>) {
    use sup_xml_core::xml_bytes_reader::{XmlBytesReader, BytesEvent};
    use sup_xml_core::compute_line_col;

    // Decode to UTF-8 first (handles a `<?xml encoding?>` declaration, a
    // byte-order mark, or an `encoding=` override), as the one-shot parser
    // does — element/text bytes downstream are then plain UTF-8.
    let decoded = match &opts.forced_encoding {
        Some(enc) => sup_xml_core::encoding::transcode_to_utf8_as(raw, enc.clone()),
        None => sup_xml_core::encoding::transcode_to_utf8_strict(raw),
    };
    let buf: &[u8] = match &decoded {
        Ok(b) => b,
        Err(e) => return (events_built, false, Some(e.clone())),
    };

    let mut reader = match XmlBytesReader::from_bytes(buf) {
        Ok(r) => r,
        Err(e) => return (events_built, false, Some(e)),
    }
    .with_options(opts.clone());
    let mut idx = 0usize;
    let mut clean_eof = false;
    let mut stop_err: Option<sup_xml_core::error::XmlError> = None;
    loop {
        match reader.next() {
            Ok(BytesEvent::Eof) => { clean_eof = true; break; }
            // Ran off the end mid-token: the remainder isn't complete yet.
            // (In recovery mode the reader recovers from *malformed* tokens
            // and only stops here on a genuinely truncated tail.)
            Err(e) => { stop_err = Some(e); break; }
            Ok(ev) => {
                let new = idx >= events_built;
                let parent = open_stack.last().copied().unwrap_or(ptr::null_mut());
                match ev {
                    BytesEvent::StartElement(s) => {
                        if new {
                            let line = compute_line_col(buf, s.name_offset() as usize).0;
                            let node = unsafe { build_element(doc, parent, s, line) };
                            open_stack.push(node);
                        }
                        idx += 1;
                    }
                    BytesEvent::EndElement(_) => {
                        if new { open_stack.pop(); }
                        idx += 1;
                    }
                    BytesEvent::Text(t) => {
                        if new {
                            let c = cstr(t.as_bytes());
                            let node = unsafe { crate::mutate::xmlNewDocText(doc, c.as_ptr()) };
                            unsafe { append_leaf(doc, parent, node); }
                        }
                        idx += 1;
                    }
                    BytesEvent::CData(t) => {
                        if new {
                            let c = cstr(t.as_bytes());
                            // libxml2 `XML_PARSE_NOCDATA` / lxml's default
                            // `strip_cdata=True` delivers CDATA content as
                            // an ordinary text node.
                            let node = if opts.cdata_as_text {
                                unsafe { crate::mutate::xmlNewDocText(doc, c.as_ptr()) }
                            } else {
                                unsafe { crate::mutate::xmlNewCDataBlock(doc, c.as_ptr(), c.as_bytes().len() as std::os::raw::c_int) }
                            };
                            unsafe { append_leaf(doc, parent, node); }
                        }
                        idx += 1;
                    }
                    BytesEvent::Comment(c) => {
                        // `remove_comments` (lxml NULLs the comment handler)
                        // drops the node from the tree AND the event stream.
                        if !opts.remove_comments {
                            if new {
                                let cc = cstr(c.as_bytes());
                                let node = unsafe { crate::mutate::xmlNewDocComment(doc, cc.as_ptr()) };
                                unsafe { append_leaf(doc, parent, node); }
                            }
                            idx += 1;
                        }
                    }
                    BytesEvent::Pi(pi) => {
                        if !opts.remove_pis {
                            if new {
                                let target = cstr(pi.target());
                                let data = cstr(pi.content());
                                let node = unsafe { crate::mutate::xmlNewDocPI(doc, target.as_ptr(), data.as_ptr()) };
                                unsafe { append_leaf(doc, parent, node); }
                            }
                            idx += 1;
                        }
                    }
                    // Entity-reference boundaries etc. are not fireable
                    // events and don't advance the watermark.
                    _ => {}
                }
            }
        }
    }
    // Apply DTD `<!ATTLIST … "default">` attribute defaults to the tree
    // before the events replay, so iterparse / pull-parser `start` and
    // `end` callbacks observe them — the one-shot parse path does the
    // same via `inject_defaults`.  The reader has loaded any external
    // subset (when `load_external_dtd` + a base URL are set) by now, so
    // its captured DTD carries the defaults.  Idempotent: re-running each
    // feed only fills attributes still missing.
    let dtd = reader.dtd();
    if !dtd.is_empty() && !doc.is_null() {
        // Find the root element among the document's top-level children
        // (skipping any prolog comment/PI).  The streaming document's own
        // `root` pointer isn't wired during the incremental build, so we
        // inject from the located root rather than `doc.root()`.
        let mut cur = unsafe { (*doc).children.get() };
        while !cur.is_null() {
            let n = unsafe { &*cur };
            if n.is_element() {
                // SAFETY: `_doc` is the document's embedded arena.
                sup_xml_core::dtd::inject_defaults_from(n, dtd, unsafe { &(*doc)._doc });
                break;
            }
            cur = n.next_sibling.get()
                .map(|s| s as *const Node<'static> as *mut Node<'static>)
                .unwrap_or(ptr::null_mut());
        }
    }
    (idx, clean_eof, stop_err)
}
