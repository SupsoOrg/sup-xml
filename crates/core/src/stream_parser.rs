//! Streaming pull parser that yields arena-allocated subtrees.
//!
//! The arena counterpart of [`crate::stream_parser`].  Each emitted subtree
//! is its own [`Document`] with its own [`bumpalo::Bump`] arena — drop the
//! `Document` and that subtree's memory is freed.  Memory is bounded by the
//! largest single emitted subtree, regardless of overall document size.
//!
//! # API surface
//!
//! * [`emit_at_depth(d)`](StreamParser::emit_at_depth) — emit elements
//!   at a fixed depth from the root.
//! * [`emit_at_path(&[…])`](StreamParser::emit_at_path) — emit elements
//!   whose root-anchored ancestor chain matches `path` exactly.
//! * [`emit_when(predicate)`](StreamParser::emit_when) — emit elements
//!   whose ancestor chain satisfies an arbitrary closure.  Useful for
//!   "any `<item>` regardless of nesting" / "anything two levels under
//!   `<rss>`" patterns the fixed-depth modes can't express.
//!
//! All three are *ancestor-bounded* modes: the parser inspects the
//! freshly-opened element's name + ancestor chain to decide whether
//! to materialize its subtree.  Elements above the emit boundary
//! cost just a depth counter + name on a `Vec<String>` ancestor
//! stack — independent of total document size.
//!
//! Namespace resolution runs per emission: each emitted `Document`'s
//! nodes carry `namespace` set to the in-scope binding at the time
//! the element was opened, inherited correctly through ancestor
//! `xmlns:*` declarations.
//!
//! # Example
//!
//! ```
//! use sup_xml_core::StreamParser;
//!
//! let xml = r#"<rss><channel>
//!   <item><title>a</title></item>
//!   <item><title>b</title></item>
//! </channel></rss>"#;
//!
//! let mut sp = StreamParser::from_str(xml)
//!     .emit_at_path(&["rss", "channel", "item"]);
//!
//! let mut titles = Vec::new();
//! while let Some(item) = sp.next().unwrap() {
//!     let t = item.root().find_child("title").unwrap();
//!     titles.push(t.text_content().unwrap().to_owned());
//!     // `item` (and its arena) drops at the end of this iteration.
//! }
//! assert_eq!(titles, vec!["a", "b"]);
//! ```

use std::io::Read;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::error::Result;
use crate::ns_helpers::{
    ns_err, validate_qname, validate_xmlns_decl, XML_NS_URI, XMLNS_NS_URI,
};
use crate::options::ParseOptions;
use crate::reader::{Attr, EventInto, XmlReader};
use crate::streaming_reader::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE};
use crate::xml_bytes_reader::BytesEvent;
use sup_xml_tree::dom::{Document, DocumentBuilder, Namespace, Node};

// ── public type ─────────────────────────────────────────────────────────────

/// Pull-based streaming parser that yields arena-allocated subtrees.
pub struct StreamParser<'src> {
    reader:   XmlReader<'src>,
    attr_buf: Vec<Attr<'src>>,
    /// Reusable scratch for the just-opened element's `(name, value)`
    /// attribute pairs, owned so the emission state machine in
    /// [`handle_event`] is independent of the borrowed event payload.
    scratch:  Vec<(String, String)>,
    emit:     EmitMode,
    state:    State,
}

/// Predicate evaluated against the ancestor-name chain of the
/// just-opened element.  Receives a slice whose last entry is the
/// just-opened element's QName and earlier entries are its
/// ancestors in order from the root.  Return `true` to emit that
/// element's subtree.
pub type EmitPredicate = Box<dyn Fn(&[String]) -> bool + Send + Sync + 'static>;

/// Selection mode.  Three flavours, ordered by specificity.
enum EmitMode {
    /// Default — emit nothing.  Caller must specify a strategy.
    None,
    /// Emit elements whose post-pop ancestor depth equals this value.
    /// `depth = 1` ⇒ direct children of the root.
    Depth(u32),
    /// Emit elements whose root-anchored ancestor chain matches `path`.
    Path(Vec<String>),
    /// Emit elements whose ancestor chain satisfies an arbitrary
    /// predicate.  Less constrained than [`EmitMode::Path`] /
    /// [`EmitMode::Depth`] — useful for "any `<book>` regardless of
    /// where it appears" or "anything whose 4th ancestor is `<rss>`"
    /// patterns.  Same memory profile as the fixed-depth modes:
    /// elements above the emit boundary cost only the ancestor stack.
    When(EmitPredicate),
}

struct State {
    /// SAX depth.  0 before the root opens, 1 inside the root, etc.
    sax_depth:  u32,
    /// Names of currently-open ancestors (root at index 0).
    name_stack: Vec<String>,
    /// Persistent namespace scope tracked across every element, whether or
    /// not it's being emitted.  Owned `(prefix, href)` pairs; layered by
    /// `ns_frames`.  Built-in `xml`/`xmlns` bindings occupy the first two
    /// entries and never get popped.
    ns_bindings: Vec<(Option<String>, String)>,
    /// Frame markers — one entry per open SAX element.  Indices into
    /// `ns_bindings` recording where each element's bindings start.
    ns_frames:   Vec<usize>,
    /// In-progress emission, if we're inside a to-be-emitted subtree.
    current:    Option<Emission>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            sax_depth:  0,
            name_stack: Vec::new(),
            ns_bindings: vec![
                (Some("xml".to_string()),   XML_NS_URI.to_string()),
                (Some("xmlns".to_string()), XMLNS_NS_URI.to_string()),
            ],
            ns_frames:  Vec::new(),
            current:    None,
        }
    }
}

/// State for the subtree currently being assembled into its own arena.
struct Emission {
    builder:     DocumentBuilder,
    /// SAX depth at which this emission started.  Ship when we close it.
    start_depth: u32,
    /// Stack of in-progress descendants (root of THIS subtree at index 0).
    /// Type-erased — see parser for the same trick.
    stack:       Vec<*const ()>,
    /// Cache mapping `(prefix, href)` to an arena-allocated [`Namespace`]
    /// pointer in this emission's `builder.bump`.  First lookup allocates;
    /// repeated uses (within this subtree) share the same allocation.
    ns_cache:    FxHashMap<(Option<String>, String), *const ()>,
}

// SAFETY note for raw pointers in `Emission::stack`: each pointer was obtained
// from `&Node` allocated in `self.builder.bump`, which lives as long as the
// `Emission` does.  Dereferencing while the `Emission` is alive is sound.

#[inline] fn erase(node: &Node<'_>) -> *const () {
    node as *const Node<'_> as *const ()
}

/// # Safety
///
/// `p` must have been produced by `erase` from a live `&Node`, and the
/// caller-chosen lifetime `'a` must not outlive that node's arena.
#[inline] unsafe fn unerase<'a>(p: *const ()) -> &'a Node<'a> {
    unsafe { &*(p as *const Node<'a>) }
}

// ── public API ──────────────────────────────────────────────────────────────

impl<'src> StreamParser<'src> {
    pub fn from_str(input: &'src str) -> Self {
        Self {
            reader:   XmlReader::from_str(input),
            attr_buf: Vec::new(),
            scratch:  Vec::new(),
            emit:     EmitMode::None,
            state:    State::default(),
        }
    }

    pub fn from_bytes(input: &'src [u8]) -> Result<Self> {
        Ok(Self {
            reader:   XmlReader::from_bytes(input)?,
            attr_buf: Vec::new(),
            scratch:  Vec::new(),
            emit:     EmitMode::None,
            state:    State::default(),
        })
    }

    /// Replace [`ParseOptions`] on the underlying reader.  Takes a reference
    /// (the reader internally clones once — amortized over the whole stream).
    pub fn with_options(mut self, opts: &ParseOptions) -> Self {
        self.reader = self.reader.with_options(opts.clone());
        self
    }

    /// Emit elements at the given depth.  `depth = 1` matches direct children
    /// of the root.  Memory-bounded.
    pub fn emit_at_depth(mut self, depth: u32) -> Self {
        self.emit = EmitMode::Depth(depth);
        self
    }

    /// Emit elements whose ancestor chain (from root) matches `path` exactly.
    /// `path[0]` is the root's name, `path[1]` its child, …, `path[N-1]` the
    /// element to emit.  Memory-bounded.
    pub fn emit_at_path(mut self, path: &[&str]) -> Self {
        self.emit = EmitMode::Path(path.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Emit elements whose ancestor chain satisfies `predicate`.  The
    /// predicate receives a slice of QNames from root to the
    /// just-opened element (inclusive at the end); return `true` to
    /// emit that element's subtree.
    ///
    /// More flexible than [`emit_at_path`](Self::emit_at_path) /
    /// [`emit_at_depth`](Self::emit_at_depth): handy when the
    /// emission criterion is "any `<item>` regardless of where it
    /// appears in the tree" or "any element under `<rss>` that is
    /// itself named `entry`".
    ///
    /// Memory profile matches the fixed-depth modes — elements
    /// above the matching boundary cost only the ancestor stack.
    ///
    /// # Example
    ///
    /// ```
    /// use sup_xml_core::StreamParser;
    /// let xml = r#"<root>
    ///   <section><item>a</item></section>
    ///   <other><item>b</item></other>
    /// </root>"#;
    /// let mut sp = StreamParser::from_str(xml)
    ///     .emit_when(|chain| chain.last().map(String::as_str) == Some("item"));
    /// while sp.next().unwrap().is_some() {}
    /// ```
    pub fn emit_when<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&[String]) -> bool + Send + Sync + 'static,
    {
        self.emit = EmitMode::When(Box::new(predicate));
        self
    }

    /// Pull the next emitted subtree.  Returns `Ok(Some(doc))` for each match,
    /// `Ok(None)` on EOF, `Err(_)` on parse error.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Document>> {
        loop {
            let ev = pull_str(&mut self.reader, &mut self.attr_buf, &mut self.scratch)?;
            if matches!(ev, SubtreeEvent::Eof) {
                return Ok(None);
            }
            if let Some(doc) = handle_event(&mut self.state, &self.emit, ev, &self.scratch)? {
                return Ok(Some(doc));
            }
        }
    }
}

// ── shared emission state machine ───────────────────────────────────────────
//
// The subtree-emission logic is identical whether events come from an
// in-memory [`XmlReader`] (string source) or the byte-level
// [`XmlByteStreamReader`] (`io::Read` source).  Each reader is drained by a
// tiny `pull_*` adapter into a normalized [`SubtreeEvent`], and the shared
// [`handle_event`] runs the depth/namespace/build machinery.

/// A reader-independent parse event.  Payloads are owned, decoupling the
/// emission machine from the borrowed event types of the two readers.
enum SubtreeEvent {
    Start(String),
    End,
    Text(String),
    CData(String),
    Comment(String),
    Pi(String, String),
    EntityRef(String),
    Eof,
}

/// Adapt one [`XmlReader`] event (string source) into a [`SubtreeEvent`],
/// filling `scratch` with the start element's `(name, value)` attribute pairs.
fn pull_str<'src>(
    reader:   &mut XmlReader<'src>,
    attr_buf: &mut Vec<Attr<'src>>,
    scratch:  &mut Vec<(String, String)>,
) -> Result<SubtreeEvent> {
    scratch.clear();
    Ok(match reader.next_into(attr_buf)? {
        EventInto::StartElement { name } => {
            for a in attr_buf.drain(..) {
                scratch.push((a.name.to_owned(), a.value.into_owned()));
            }
            SubtreeEvent::Start(name.into_owned())
        }
        EventInto::EndElement { .. }       => SubtreeEvent::End,
        EventInto::Text(t)                 => SubtreeEvent::Text(t.into_owned()),
        EventInto::CData(t)                => SubtreeEvent::CData(t.into_owned()),
        EventInto::Comment(t)              => SubtreeEvent::Comment(t.into_owned()),
        EventInto::Pi { target, content }  => SubtreeEvent::Pi(target.into_owned(), content.into_owned()),
        EventInto::EntityRef { name }      => SubtreeEvent::EntityRef(name.into_owned()),
        EventInto::Eof                     => SubtreeEvent::Eof,
    })
}

/// Adapt one [`XmlByteStreamReader`] event (`io::Read` source) into a
/// [`SubtreeEvent`], filling `scratch` with the start element's attributes.
fn pull_bytes<R: Read>(
    reader:  &mut XmlByteStreamReader<R>,
    scratch: &mut Vec<(String, String)>,
) -> Result<SubtreeEvent> {
    scratch.clear();
    Ok(match reader.next_event()? {
        BytesEvent::StartElement(tag) => {
            let name = bytes_to_string(tag.name())?;
            for a in tag.attrs() {
                let a = a?;
                scratch.push((bytes_to_string(a.name)?, bytes_to_string(&a.value)?));
            }
            SubtreeEvent::Start(name)
        }
        BytesEvent::EndElement(_) => SubtreeEvent::End,
        BytesEvent::Text(t)       => SubtreeEvent::Text(bytes_to_string(t.as_bytes())?),
        BytesEvent::CData(t)      => SubtreeEvent::CData(bytes_to_string(t.as_bytes())?),
        BytesEvent::Comment(t)    => SubtreeEvent::Comment(bytes_to_string(t.as_bytes())?),
        BytesEvent::Pi(p)         => SubtreeEvent::Pi(bytes_to_string(p.target())?, bytes_to_string(p.content())?),
        BytesEvent::EntityRef(e)  => SubtreeEvent::EntityRef(bytes_to_string(e.name())?),
        BytesEvent::Eof           => SubtreeEvent::Eof,
    })
}

fn bytes_to_string(b: &[u8]) -> Result<String> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| ns_err("invalid UTF-8 in streamed document".to_string()))
}

/// Advance the emission state machine by one normalized event.  Returns
/// `Ok(Some(doc))` when an emission subtree has just completed, `Ok(None)`
/// to keep pulling.  `attrs` holds the just-opened element's attributes
/// (only consulted for [`SubtreeEvent::Start`]).
fn handle_event(
    state: &mut State,
    emit:  &EmitMode,
    ev:    SubtreeEvent,
    attrs: &[(String, String)],
) -> Result<Option<Document>> {
    match ev {
        SubtreeEvent::Start(name) => {
            state.sax_depth += 1;
            state.name_stack.push(name.clone());
            validate_qname(&name, "element")?;

            // Open a namespace frame, parse xmlns decls into it.
            state.ns_frames.push(state.ns_bindings.len());
            for (an, av) in attrs {
                if an == "xmlns" {
                    state.ns_bindings.push((None, av.clone()));
                } else if let Some(local) = an.strip_prefix("xmlns:") {
                    validate_xmlns_decl(local, av)?;
                    if local == "xml" { continue; }  // legal no-op redeclare
                    state.ns_bindings.push((Some(local.to_string()), av.clone()));
                }
            }

            let starts_emission = state.current.is_none() && element_matches(emit, state);
            if starts_emission {
                let mut emis = Emission {
                    builder:     DocumentBuilder::new(),
                    start_depth: state.sax_depth,
                    stack:       Vec::new(),
                    ns_cache:    FxHashMap::default(),
                };
                let el_ptr = build_element_with_ns(&mut emis, &name, attrs, &state.ns_bindings)?;
                emis.stack.push(el_ptr);
                state.current = Some(emis);
            } else if let Some(emis) = state.current.as_mut() {
                let el_ptr = build_element_with_ns(emis, &name, attrs, &state.ns_bindings)?;
                // SAFETY: pointers point into emis.builder (still alive).
                let parent: &Node<'_> = unsafe { unerase(*emis.stack.last().unwrap()) };
                let el:     &Node<'_> = unsafe { unerase(el_ptr) };
                emis.builder.append_child(parent, el);
                emis.stack.push(el_ptr);
            } else {
                // Outside any emission — still validate the element's QName /
                // namespace resolution (so undeclared prefixes fail fast).
                validate_element_ns(&name, attrs, &state.ns_bindings)?;
            }
            Ok(None)
        }

        SubtreeEvent::End => {
            state.name_stack.pop();
            if let Some(frame_start) = state.ns_frames.pop() {
                state.ns_bindings.truncate(frame_start);
            }
            let closed_depth = state.sax_depth;
            state.sax_depth -= 1;

            if let Some(emis) = state.current.as_mut() {
                if emis.start_depth == closed_depth {
                    // Closing the emission anchor — ship it.
                    let emis = state.current.take().unwrap();
                    // SAFETY: stack[0] is this subtree's root, still in emis.builder.
                    let root: &Node<'_> = unsafe { unerase(emis.stack[0]) };
                    emis.builder.set_root(root);
                    return Ok(Some(emis.builder.build()));
                }
                emis.stack.pop();
            }
            Ok(None)
        }

        SubtreeEvent::Text(t) => {
            if let Some(emis) = state.current.as_mut() {
                let parent_ptr = *emis.stack.last().unwrap();
                let s    = emis.builder.alloc_str(&t);
                let node = emis.builder.new_text(s);
                let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                emis.builder.append_child(parent, node);
            }
            Ok(None)
        }
        SubtreeEvent::CData(t) => {
            if let Some(emis) = state.current.as_mut() {
                let parent_ptr = *emis.stack.last().unwrap();
                let s    = emis.builder.alloc_str(&t);
                let node = emis.builder.new_cdata(s);
                let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                emis.builder.append_child(parent, node);
            }
            Ok(None)
        }
        SubtreeEvent::Comment(t) => {
            if let Some(emis) = state.current.as_mut() {
                let parent_ptr = *emis.stack.last().unwrap();
                let s    = emis.builder.alloc_str(&t);
                let node = emis.builder.new_comment(s);
                let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                emis.builder.append_child(parent, node);
            }
            Ok(None)
        }
        SubtreeEvent::Pi(target, content) => {
            if let Some(emis) = state.current.as_mut() {
                let parent_ptr = *emis.stack.last().unwrap();
                let t    = emis.builder.alloc_str(&target);
                let c    = if content.is_empty() { None } else { Some(&*emis.builder.alloc_str(&content)) };
                let node = emis.builder.new_pi(t, c);
                let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                emis.builder.append_child(parent, node);
            }
            Ok(None)
        }
        SubtreeEvent::EntityRef(name) => {
            if let Some(emis) = state.current.as_mut() {
                let parent_ptr = *emis.stack.last().unwrap();
                let n  = emis.builder.alloc_str(&name);
                // Reconstruct the literal `&name;` source form for round-trip.
                let lit = format!("&{name};");
                let c  = emis.builder.alloc_str(&lit);
                let node = emis.builder.new_entity_ref(n, c);
                let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                emis.builder.append_child(parent, node);
            }
            Ok(None)
        }

        SubtreeEvent::Eof => Ok(None),
    }
}

/// Does the just-opened element (whose name + ancestors are on `state`'s
/// stacks) satisfy the emit predicate?
fn element_matches(emit: &EmitMode, state: &State) -> bool {
    match emit {
        EmitMode::None       => false,
        // Depth(d) emits when post-pop ancestor stack length == d.  At
        // StartElement time, sax_depth already reflects the just-opened
        // element, so its parent depth (post-pop) is `sax_depth - 1`.
        EmitMode::Depth(d)   => state.sax_depth.saturating_sub(1) == *d,
        EmitMode::Path(path) => path.len() == state.name_stack.len()
            && state.name_stack.iter().zip(path.iter()).all(|(a, b)| a == b),
        EmitMode::When(pred) => pred(&state.name_stack),
    }
}

/// Streaming pull parser over an [`io::Read`](std::io::Read) source.
///
/// The byte-level counterpart of [`StreamParser`]: it pulls bytes from the
/// reader on demand into a bounded rolling buffer, so neither the source nor
/// the working set is ever fully resident.  Combined with one-subtree-at-a-
/// time emission, total memory is bounded by `max(buffer, largest subtree)`
/// regardless of document size — suitable for multi-gigabyte inputs from a
/// file or pipe.  The emit-selection API mirrors [`StreamParser`].
pub struct ByteStreamParser<R: Read> {
    reader:  XmlByteStreamReader<R>,
    scratch: Vec<(String, String)>,
    emit:    EmitMode,
    state:   State,
}

impl<R: Read> ByteStreamParser<R> {
    /// Construct over `read` with the default working-buffer size.
    pub fn new(read: R) -> Result<Self> {
        Self::with_buffer_size(read, DEFAULT_BUFFER_SIZE)
    }

    /// Construct over `read` with an explicit working-buffer size (which also
    /// caps the largest single XML token — see [`XmlByteStreamReader`]).
    pub fn with_buffer_size(read: R, buffer_size: usize) -> Result<Self> {
        Ok(Self {
            reader:  XmlByteStreamReader::new(read, buffer_size)?,
            scratch: Vec::new(),
            emit:    EmitMode::None,
            state:   State::default(),
        })
    }

    /// Emit elements at the given depth (`depth = 1` ⇒ children of the root).
    pub fn emit_at_depth(mut self, depth: u32) -> Self {
        self.emit = EmitMode::Depth(depth);
        self
    }

    /// Emit elements whose root-anchored ancestor chain matches `path`.
    pub fn emit_at_path(mut self, path: &[&str]) -> Self {
        self.emit = EmitMode::Path(path.iter().map(|s| (*s).to_owned()).collect());
        self
    }

    /// Emit elements whose ancestor chain satisfies `predicate` (see
    /// [`StreamParser::emit_when`]).
    pub fn emit_when<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&[String]) -> bool + Send + Sync + 'static,
    {
        self.emit = EmitMode::When(Box::new(predicate));
        self
    }

    /// Pull the next emitted subtree.  `Ok(Some(doc))` per match, `Ok(None)`
    /// on EOF, `Err(_)` on parse / I/O error.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Document>> {
        loop {
            let ev = pull_bytes(&mut self.reader, &mut self.scratch)?;
            if matches!(ev, SubtreeEvent::Eof) {
                return Ok(None);
            }
            if let Some(doc) = handle_event(&mut self.state, &self.emit, ev, &self.scratch)? {
                return Ok(Some(doc));
            }
        }
    }
}

// ── namespace helpers (per-emission) ────────────────────────────────────────

/// Build an arena element with its full namespace state resolved against
/// `global_ns_bindings`.  Caches Namespace allocations within `emis.ns_cache`.
///
/// Returns a type-erased pointer to the freshly-allocated element so the
/// caller can continue using `emis` afterwards without conflicting borrows.
fn build_element_with_ns(
    emis:               &mut Emission,
    name:               &str,
    attrs:              &[(String, String)],
    global_ns_bindings: &[(Option<String>, String)],
) -> Result<*const ()> {
    // Split-borrow: `builder` (shared) and `ns_cache` (exclusive) come from
    // different fields, so the borrow checker tracks them independently —
    // letting us hold a long-lived borrow on `builder` (via `el`) while still
    // mutating `ns_cache`.
    let Emission { builder, ns_cache, .. } = emis;

    let aname = builder.alloc_str(name);
    let el    = builder.new_element(aname);

    // Resolve element's QName.
    if let Some((p, h)) = resolve_qname(name, global_ns_bindings, /*is_attribute=*/ false)? {
        let ns_ptr = lookup_or_alloc_ns(builder, ns_cache, p, h);
        // SAFETY: ns_ptr came from builder.new_namespace; same lifetime as el.
        let ns: &Namespace<'_> = unsafe { &*(ns_ptr as *const Namespace<'_>) };
        el.namespace.set(Some(ns));
    }

    // Materialize attributes; resolve namespaces; check duplicate-by-expanded-name.
    let mut seen: FxHashSet<(String, String)> = FxHashSet::default();
    for (an, av) in attrs {
        let aname  = builder.alloc_str(an);
        let avalue = builder.alloc_str(av);
        let attr   = builder.new_attribute(aname, avalue);
        builder.append_attribute(el, attr);

        if an == "xmlns" || an.starts_with("xmlns:") {
            continue;
        }
        validate_qname(an, "attribute")?;
        if let Some((p, h)) = resolve_qname(an, global_ns_bindings, /*is_attribute=*/ true)? {
            let ns_ptr = lookup_or_alloc_ns(builder, ns_cache, p, h);
            let ns: &Namespace<'_> = unsafe { &*(ns_ptr as *const Namespace<'_>) };
            attr.namespace.set(Some(ns));
        }
        // Duplicate-by-expanded-name check (URI + local part).
        let ns_uri = attr.namespace.get().map(|n| n.href()).unwrap_or("");
        let local  = an.rfind(':').map(|i| &an[i+1..]).unwrap_or(an);
        if !seen.insert((ns_uri.to_string(), local.to_string())) {
            return Err(ns_err(format!(
                "duplicate attribute '{local}' in namespace '{ns_uri}' after namespace expansion"
            )));
        }
    }

    Ok(erase(el))
}

/// Look up an arena Namespace for `(prefix, href)` in this emission's cache,
/// allocating one if it doesn't exist.  Split-borrow form — takes `builder`
/// and `ns_cache` separately so it can be called while another borrow on
/// `Emission.builder` is active.
fn lookup_or_alloc_ns(
    builder:  &DocumentBuilder,
    ns_cache: &mut FxHashMap<(Option<String>, String), *const ()>,
    prefix:   Option<&str>,
    href:     &str,
) -> *const () {
    let key = (prefix.map(str::to_owned), href.to_owned());
    if let Some(&ptr) = ns_cache.get(&key) {
        return ptr;
    }
    let pref_arena = prefix.map(|p| builder.alloc_str(p));
    let href_arena = builder.alloc_str(href);
    let ns         = builder.new_namespace(pref_arena, href_arena);
    let ptr        = ns as *const Namespace<'_> as *const ();
    ns_cache.insert(key, ptr);
    ptr
}

/// Resolve a QName against the global namespace scope.  Returns
/// `Some((prefix, href))` for any element/attribute that maps to a real
/// namespace; `None` for in-no-namespace (unprefixed elements without a
/// default ns, or unprefixed attributes).
fn resolve_qname<'a>(
    qname:        &'a str,
    bindings:     &'a [(Option<String>, String)],
    is_attribute: bool,
) -> Result<Option<(Option<&'a str>, &'a str)>> {
    if let Some(colon) = qname.find(':') {
        let prefix = &qname[..colon];
        match lookup_prefix(Some(prefix), bindings) {
            Some(href) => Ok(Some((Some(prefix), href))),
            None       => Err(ns_err(format!("undeclared namespace prefix '{prefix}' in '{qname}'"))),
        }
    } else if is_attribute {
        Ok(None)
    } else {
        match lookup_prefix(None, bindings) {
            Some(href) if !href.is_empty() => Ok(Some((None, href))),
            _                              => Ok(None),
        }
    }
}

/// Lookup `prefix` (None = default ns) in the global scope, innermost wins.
fn lookup_prefix<'a>(
    prefix:   Option<&str>,
    bindings: &'a [(Option<String>, String)],
) -> Option<&'a str> {
    for (p, h) in bindings.iter().rev() {
        if p.as_deref() == prefix {
            return Some(h);
        }
    }
    None
}

/// Validate namespace resolution for an element we won't materialize (we're
/// outside any emission).  Catches undeclared prefixes early so the streaming
/// parser fails fast rather than silently skipping malformed content.
fn validate_element_ns(
    name:     &str,
    attrs:    &[(String, String)],
    bindings: &[(Option<String>, String)],
) -> Result<()> {
    validate_qname(name, "element")?;
    let _ = resolve_qname(name, bindings, false)?;
    for (an, _av) in attrs {
        if an == "xmlns" || an.starts_with("xmlns:") { continue; }
        validate_qname(an, "attribute")?;
        let _ = resolve_qname(an, bindings, true)?;
    }
    Ok(())
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_tree::dom::NodeKind;

    fn collect_names(mut sp: StreamParser) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(doc) = sp.next().unwrap() {
            out.push(doc.root().name().to_string());
        }
        out
    }

    // ── emit_at_depth ───────────────────────────────────────────────────

    #[test]
    fn depth_one_matches_children_of_root() {
        let xml = "<r><a/><b/><c/></r>";
        let sp = StreamParser::from_str(xml).emit_at_depth(1);
        assert_eq!(collect_names(sp), vec!["a", "b", "c"]);
    }

    #[test]
    fn depth_zero_emits_root() {
        let xml = "<root><a/><b/></root>";
        let mut sp = StreamParser::from_str(xml).emit_at_depth(0);
        let doc = sp.next().unwrap().expect("should yield root");
        assert_eq!(doc.root().name(), "root");
        assert_eq!(doc.root().children().count(), 2);
        assert!(sp.next().unwrap().is_none());
    }

    #[test]
    fn depth_two_matches_grandchildren() {
        let xml = "<r><a><b/><b/></a><a><b/></a></r>";
        let sp = StreamParser::from_str(xml).emit_at_depth(2);
        assert_eq!(collect_names(sp), vec!["b", "b", "b"]);
    }

    #[test]
    fn no_emit_mode_yields_nothing() {
        let mut sp = StreamParser::from_str("<r><a/></r>");
        assert!(sp.next().unwrap().is_none());
    }

    // ── emit_at_path ────────────────────────────────────────────────────

    #[test]
    fn path_root_anchored() {
        // Nested <item> inside an <item> must NOT match — its ancestors
        // are rss/channel/item, not rss/channel.
        let xml = "<rss><channel>\
                     <item><id>1</id></item>\
                     <item><id>2</id><item><id>2a</id></item></item>\
                   </channel></rss>";
        let sp = StreamParser::from_str(xml)
            .emit_at_path(&["rss", "channel", "item"]);
        assert_eq!(collect_names(sp), vec!["item", "item"]);
    }

    #[test]
    fn path_wrong_root_yields_nothing() {
        let xml = "<feed><channel><item/></channel></feed>";
        let mut sp = StreamParser::from_str(xml)
            .emit_at_path(&["rss", "channel", "item"]);
        assert!(sp.next().unwrap().is_none());
    }

    #[test]
    fn path_single_element_emits_root() {
        let xml = "<root><a/></root>";
        let mut sp = StreamParser::from_str(xml).emit_at_path(&["root"]);
        let doc = sp.next().unwrap().unwrap();
        assert_eq!(doc.root().name(), "root");
        assert!(sp.next().unwrap().is_none());
    }

    // ── emit_when (predicate mode) ─────────────────────────────────────

    /// Predicate over the just-opened element's name matches any
    /// `<item>` anywhere — different depths, different ancestors.
    /// `emit_at_path` couldn't express this; `emit_at_depth` would
    /// over-emit.
    #[test]
    fn predicate_matches_by_name_across_nesting_levels() {
        let xml = r#"<root>
            <section><item>one</item></section>
            <wrapper><inner><item>two</item></inner></wrapper>
            <thing/>
        </root>"#;
        let sp = StreamParser::from_str(xml)
            .emit_when(|chain| chain.last().map(String::as_str) == Some("item"));
        let names: Vec<String> = {
            let mut sp = sp;
            let mut out = Vec::new();
            while let Some(d) = sp.next().unwrap() {
                out.push(d.root().text_content().unwrap_or_default().to_string());
            }
            out
        };
        assert_eq!(names, vec!["one", "two"],
            "predicate should pick both <item>s irrespective of depth");
    }

    /// Predicate can reach into the full ancestor chain.  Matching
    /// "any `book` whose immediate parent is `library`" excludes a
    /// `book` nested inside `archive/library/book/book`.
    #[test]
    fn predicate_can_inspect_ancestor_chain() {
        let xml = r#"<archive>
            <library><book>L1</book></library>
            <library><book>L2</book></library>
            <library><book>L3<book>nested</book></book></library>
        </archive>"#;
        let sp = StreamParser::from_str(xml).emit_when(|chain| {
            chain.len() >= 2
                && chain.last().map(String::as_str)  == Some("book")
                && chain.get(chain.len() - 2).map(String::as_str) == Some("library")
        });
        let texts: Vec<String> = {
            let mut sp = sp;
            let mut out = Vec::new();
            while let Some(d) = sp.next().unwrap() {
                let t = d.root().text_content().unwrap_or_default().to_string();
                out.push(t);
            }
            out
        };
        // The nested `<book>nested</book>` has ancestor chain
        // archive/library/book/book — last 2 are book/book, not
        // library/book — so it should be skipped.
        assert_eq!(texts.len(), 3,
            "expected exactly the three immediate-child <book>s, got {texts:?}");
    }

    /// Predicate mode plays well with namespace resolution — emitted
    /// subtrees carry resolved Namespace pointers just like the
    /// fixed-depth modes.
    #[test]
    fn predicate_emission_preserves_namespace_resolution() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
            <entry><title>t1</title></entry>
            <entry><title>t2</title></entry>
        </feed>"#;
        let mut sp = StreamParser::from_str(xml)
            .emit_when(|chain| chain.last().map(String::as_str) == Some("entry"));
        let entry = sp.next().unwrap().expect("should yield entry");
        let ns = entry.root().namespace.get()
            .expect("entry must inherit feed's default namespace");
        assert_eq!(ns.href(), "http://www.w3.org/2005/Atom");
    }

    // ── subtree contents ────────────────────────────────────────────────

    #[test]
    fn emitted_subtree_contains_children() {
        let xml = "<r><item><title>X</title><body>hello</body></item></r>";
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        let root = item.root();
        assert_eq!(root.name(), "item");
        let title = root.find_child("title").unwrap();
        assert_eq!(title.text_content(), Some("X"));
        let body = root.find_child("body").unwrap();
        assert_eq!(body.text_content(), Some("hello"));
    }

    #[test]
    fn emitted_subtree_contains_attributes() {
        let xml = r#"<r><page id="1" lang="en">x</page></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let page = sp.next().unwrap().unwrap();
        let pairs: Vec<(&str, &str)> = page.root().attributes()
            .map(|a| (a.name(), a.value())).collect();
        assert_eq!(pairs, vec![("id", "1"), ("lang", "en")]);
    }

    #[test]
    fn mixed_content_preserved() {
        let xml = "<r><item>before<!-- c --><b>x</b><![CDATA[raw]]>after</item></r>";
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        let kinds: Vec<NodeKind> = item.root().children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![
            NodeKind::Text, NodeKind::Comment, NodeKind::Element,
            NodeKind::CData, NodeKind::Text,
        ]);
    }

    #[test]
    fn entity_in_attr_and_text_expanded() {
        let xml = r#"<r><x v="a&amp;b">c&lt;d</x></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let x = sp.next().unwrap().unwrap();
        assert_eq!(x.root().attributes().next().unwrap().value(), "a&b");
        assert_eq!(x.root().text_content(), Some("c<d"));
    }

    // ── streaming property ──────────────────────────────────────────────

    #[test]
    fn many_emissions_independent_arenas() {
        // Each emitted Document has its own Bump.  Dropping one shouldn't
        // affect any other.  We hold all in a Vec and verify they're distinct.
        let mut xml = String::from("<r>");
        for i in 0..50 { xml.push_str(&format!("<i>{i}</i>")); }
        xml.push_str("</r>");

        let mut sp = StreamParser::from_str(&xml).emit_at_depth(1);
        let mut docs = Vec::new();
        while let Some(d) = sp.next().unwrap() { docs.push(d); }
        assert_eq!(docs.len(), 50);
        // Each has its own arena — bytes per-doc reflect just one <i>N</i>.
        for d in &docs {
            assert!(d.memory_bytes() > 0);
            assert!(d.memory_bytes() < 4096, "single-item arena should be small");
        }
        // Drop docs[0] doesn't invalidate docs[1] etc.
        let removed = docs.remove(0);
        drop(removed);
        assert_eq!(docs[0].root().name(), "i");
    }

    #[test]
    fn eof_then_none_idempotent() {
        let mut sp = StreamParser::from_str("<r><a/></r>").emit_at_depth(1);
        assert!(sp.next().unwrap().is_some());
        assert!(sp.next().unwrap().is_none());
        assert!(sp.next().unwrap().is_none());
    }

    #[test]
    fn errors_propagate() {
        let mut sp = StreamParser::from_str("<r><a></b></r>").emit_at_depth(1);
        assert!(sp.next().is_err());
    }

    #[test]
    fn prolog_events_are_silently_discarded() {
        let xml = "<?xml version='1.0'?><!-- pre --><?stylesheet?><r><a/></r>";
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let a = sp.next().unwrap().unwrap();
        assert_eq!(a.root().name(), "a");
        assert!(sp.next().unwrap().is_none());
    }

    // ── namespace resolution ────────────────────────────────────────────

    #[test]
    fn ns_inherited_from_ancestor_above_emit_boundary() {
        // <feed xmlns="..."> declares default ns; emitted <entry> inherits it.
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>X</title></entry></feed>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let entry = sp.next().unwrap().unwrap();
        let ns = entry.root().namespace.get().expect("entry must inherit feed's default ns");
        assert!(ns.prefix.is_none());
        assert_eq!(ns.href(), "http://www.w3.org/2005/Atom");
        // <title> inside entry also resolves to the same ns
        let title = entry.root().find_child("title").unwrap();
        assert_eq!(title.namespace.get().unwrap().href(), "http://www.w3.org/2005/Atom");
    }

    #[test]
    fn ns_prefix_inherited_from_ancestor() {
        let xml = r#"<rss xmlns:dc="http://purl.org/dc/elements/1.1/"><channel><dc:title>X</dc:title></channel></rss>"#;
        let mut sp = StreamParser::from_str(xml)
            .emit_at_path(&["rss", "channel"]);
        let channel = sp.next().unwrap().unwrap();
        let title = channel.root().find_child("dc:title").unwrap();
        let ns = title.namespace.get().unwrap();
        assert_eq!(ns.prefix(), Some("dc"));
        assert_eq!(ns.href(),   "http://purl.org/dc/elements/1.1/");
    }

    #[test]
    fn ns_declared_inside_emission() {
        // Namespace declared on the emitted element itself.
        let xml = r#"<feed><entry xmlns:atom="http://www.w3.org/2005/Atom" atom:rel="self"/></feed>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let entry = sp.next().unwrap().unwrap();
        let rel = entry.root().attributes().find(|a| a.name() == "atom:rel").unwrap();
        let ns = rel.namespace.get().unwrap();
        assert_eq!(ns.href(), "http://www.w3.org/2005/Atom");
    }

    #[test]
    fn ns_xml_prefix_resolved_inside_emission() {
        let xml = r#"<r><item xml:lang="en"/></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        let lang = item.root().attributes().next().unwrap();
        assert_eq!(lang.namespace.get().unwrap().href(),
                   "http://www.w3.org/XML/1998/namespace");
    }

    #[test]
    fn ns_undeclared_prefix_above_emit_boundary_errors() {
        // Undeclared prefix outside the emitted region — streaming parser
        // must catch it (fail-fast) even though it's not materializing this element.
        let xml = r#"<r><foo:bad/><item/></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        // The first emit fails when we hit <foo:bad/>.
        assert!(sp.next().is_err());
    }

    #[test]
    fn ns_undeclared_prefix_inside_emission_errors() {
        let xml = r#"<r><item><bad:thing/></item></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        assert!(sp.next().is_err());
    }

    #[test]
    fn ns_cache_dedups_repeated_prefix_uses() {
        // Two children using the same prefix should share one Namespace allocation
        // in the emission's arena.  We can't directly assert pointer identity from
        // outside, but we can assert that arena bytes don't grow linearly with
        // repeated uses (compared to a single use).
        let single = r#"<r><item><a><x:c xmlns:x="http://x.com/"/></a></item></r>"#;
        let many = r#"<r><item><a xmlns:x="http://x.com/"><x:c/><x:c/><x:c/><x:c/><x:c/><x:c/><x:c/><x:c/></a></item></r>"#;

        let mut sp1 = StreamParser::from_str(single).emit_at_depth(1);
        let mut sp2 = StreamParser::from_str(many).emit_at_depth(1);
        let d1 = sp1.next().unwrap().unwrap();
        let d2 = sp2.next().unwrap().unwrap();
        // d2 should be slightly larger (more nodes + their attrs/strings) but not
        // grow by ~8 Namespace allocations.  Allow generous slack — just verify
        // it's not catastrophically larger.
        let ratio = d2.memory_bytes() as f64 / d1.memory_bytes() as f64;
        assert!(ratio < 4.0, "8× uses shouldn't bloat 8× — got ratio {ratio:.2}");
    }

    #[test]
    fn ns_override_in_emitted_subtree() {
        // Outer default ns overridden inside the emitted subtree.
        let xml = r#"<root xmlns="http://outer.com/"><item><inner xmlns="http://inner.com/"/></item></root>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        assert_eq!(item.root().namespace.get().unwrap().href(), "http://outer.com/");
        let inner = item.root().find_child("inner").unwrap();
        assert_eq!(inner.namespace.get().unwrap().href(), "http://inner.com/");
    }

    #[test]
    fn ns_unprefixed_attr_not_in_default_ns() {
        // xmlns="..." default ns applies to elements but NOT unprefixed attrs.
        let xml = r#"<r xmlns="http://ex.com/"><item id="1"/></r>"#;
        let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        assert!(item.root().namespace.get().is_some());
        let id = item.root().attributes().next().unwrap();
        assert!(id.namespace.get().is_none(), "default ns must not apply to unprefixed attrs");
    }

    // ── ByteStreamParser (io::Read source) ──────────────────────────────

    use std::io::Cursor;

    fn byte_collect_names<R: std::io::Read>(mut sp: ByteStreamParser<R>) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(doc) = sp.next().unwrap() {
            out.push(doc.root().name().to_string());
        }
        out
    }

    #[test]
    fn byte_path_matches_records() {
        let xml = "<data><item><name>a</name></item><item><name>b</name></item></data>";
        let sp = ByteStreamParser::new(Cursor::new(xml.as_bytes().to_vec()))
            .unwrap()
            .emit_at_path(&["data", "item"]);
        assert_eq!(byte_collect_names(sp), vec!["item", "item"]);
    }

    #[test]
    fn byte_depth_and_content_match_str_parser() {
        // The Read path must produce the same subtrees as the &str path.
        let xml = "<r><a x=\"1\">t1</a><a x=\"2\">t2</a></r>";
        let str_doc = {
            let mut sp = StreamParser::from_str(xml).emit_at_depth(1);
            let mut v = Vec::new();
            while let Some(d) = sp.next().unwrap() {
                v.push(d.root().text_content().unwrap_or_default().to_string());
            }
            v
        };
        let byte_doc = {
            let mut sp = ByteStreamParser::new(Cursor::new(xml.as_bytes().to_vec()))
                .unwrap()
                .emit_at_depth(1);
            let mut v = Vec::new();
            while let Some(d) = sp.next().unwrap() {
                v.push(d.root().text_content().unwrap_or_default().to_string());
            }
            v
        };
        assert_eq!(str_doc, byte_doc);
        assert_eq!(byte_doc, vec!["t1", "t2"]);
    }

    #[test]
    fn byte_namespace_inherited_from_ancestor() {
        let xml = r#"<root xmlns:p="http://ex.com/"><p:item/><p:item/></root>"#;
        let mut sp = ByteStreamParser::new(Cursor::new(xml.as_bytes().to_vec()))
            .unwrap()
            .emit_at_depth(1);
        let item = sp.next().unwrap().unwrap();
        assert_eq!(item.root().namespace.get().unwrap().href(), "http://ex.com/");
    }

    #[test]
    fn byte_small_buffer_handles_large_document() {
        // A document far larger than the working buffer, pulled with a
        // tiny buffer — exercises refill/compact between records.
        let mut xml = String::from("<data>");
        for i in 0..2000 {
            xml.push_str(&format!("<item><name>n{i}</name></item>"));
        }
        xml.push_str("</data>");
        let mut sp = ByteStreamParser::with_buffer_size(
            Cursor::new(xml.into_bytes()),
            4096,
        )
        .unwrap()
        .emit_at_path(&["data", "item"]);
        let mut count = 0;
        while let Some(_doc) = sp.next().unwrap() {
            count += 1;
        }
        assert_eq!(count, 2000);
    }
}
