#![forbid(unsafe_code)]

//! Streaming HTML parser surfaces.
//!
//! Two shapes, both built on the same underlying `StreamingSink`:
//!
//! - [`HtmlReader`] / [`HtmlBytesReader`] — pull-based event iterator
//!   (XmlReader twin).  Caller drives via `next()`.
//! - [`HtmlSaxParser`] — push-based callback parser
//!   (libxml2 / lxml twin).  Caller feeds bytes; sink dispatches
//!   events directly to a registered handler.
//!
//! Both surfaces emit tree-construction events (post-tokenizer +
//! post-tree-builder), not raw tokens.  Implicit `<html>`/`<head>`
//! /`<body>` insertion, void-element handling, named entity
//! decoding, and tag-soup recovery have all happened by the time
//! the consumer sees an event.
//!
//! # Bridging html5ever's push API to our pull API
//!
//! html5ever is push-based: you call `Tokenizer::feed(buffer)` and it
//! synchronously invokes our sink's TreeSink methods for every token.
//! To expose a pull iterator on top, we use a sink that buffers
//! events into a `VecDeque` instead of building a tree.
//! `HtmlReader::next()` drains that queue, feeding the next 8 KiB
//! chunk of input when the queue empties.
//!
//! For the push surface, the same sink is parameterised with a
//! callback emitter that dispatches each event directly to the
//! caller's `HtmlSaxHandler` instead of buffering — saving the
//! VecDeque allocation but losing the ability to pull.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use html5ever::driver::{parse_document, ParseOpts, Parser};
use html5ever::interface::tree_builder::{
    ElementFlags, NodeOrText, QuirksMode as H5QuirksMode, TreeSink,
};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{Attribute as H5Attribute, QualName};
use markup5ever::interface::tree_builder::ElemName;
use markup5ever::{LocalName, Namespace as H5Namespace};

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};

use super::events::{HtmlAttrs, HtmlEvent, OwnedAttr, OwnedEvent};
use super::options::HtmlParseOptions;

// ── EventEmit trait — the shared escape hatch out of the sink ────────────────

/// Strategy for "what to do with each event the sink produces."
/// Buffered emitters queue events for later pull; callback emitters
/// dispatch immediately.
pub(crate) trait EventEmit {
    fn emit(&mut self, event: OwnedEvent);
}

/// Buffered emitter — pushes events into a `VecDeque` for the pull
/// reader to consume via `next()`.
pub(crate) struct BufferedEmit {
    pub queue: VecDeque<OwnedEvent>,
}

impl BufferedEmit {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(32),
        }
    }
}

impl EventEmit for BufferedEmit {
    fn emit(&mut self, event: OwnedEvent) {
        // Coalesce adjacent text events so consumers don't see
        // arbitrarily-fragmented runs of character data.
        if let OwnedEvent::Text(ref new_text) = event {
            if let Some(OwnedEvent::Text(last)) = self.queue.back_mut() {
                last.push_str(new_text);
                return;
            }
        }
        self.queue.push_back(event);
    }
}

/// Callback emitter — dispatches each event directly into the
/// caller's `HtmlSaxHandler`.  Used by `HtmlSaxParser`.
pub(crate) struct CallbackEmit<H: HtmlSaxHandler> {
    pub handler: H,
    /// Pending text accumulator — coalesces adjacent text events
    /// the same way `BufferedEmit` does.  Flushed on the next
    /// non-text event or on `finish`.
    pub pending_text: String,
}

impl<H: HtmlSaxHandler> CallbackEmit<H> {
    fn new(handler: H) -> Self {
        Self {
            handler,
            pending_text: String::new(),
        }
    }

    fn flush_text(&mut self) {
        if !self.pending_text.is_empty() {
            let text = std::mem::take(&mut self.pending_text);
            self.handler.text(&text);
        }
    }
}

impl<H: HtmlSaxHandler> EventEmit for CallbackEmit<H> {
    fn emit(&mut self, event: OwnedEvent) {
        match event {
            OwnedEvent::Text(t) => {
                self.pending_text.push_str(&t);
            }
            OwnedEvent::StartElement { name, attrs } => {
                self.flush_text();
                self.handler
                    .start_element(&name, HtmlAttrs { inner: &attrs });
            }
            OwnedEvent::EndElement { name } => {
                self.flush_text();
                self.handler.end_element(&name);
            }
            OwnedEvent::Comment(c) => {
                self.flush_text();
                self.handler.comment(&c);
            }
            OwnedEvent::Doctype {
                name,
                public_id,
                system_id,
            } => {
                self.flush_text();
                self.handler.doctype(&name, &public_id, &system_id);
            }
            OwnedEvent::ParseError(err) => {
                self.handler.parse_error(&err);
            }
            OwnedEvent::Eof => {
                self.flush_text();
                self.handler.end_document();
            }
        }
    }
}

// ── The shared TreeSink that drives both surfaces ────────────────────────────

/// One slot in the streaming sink's small arena.  Used to carry
/// element identity between html5ever's `create_element` and the
/// later `append` / `pop` / `add_attrs_if_missing` calls.  Unlike
/// `BatchSink::SinkNode` we don't track parent / children pointers
/// here — we emit events on the fly instead of building a tree.
enum StreamNode {
    Document,
    Element {
        name: QualName,
        attrs: Vec<H5Attribute>,
        /// Has a `StartElement` event been emitted for this node yet?
        /// We emit on the first `append` (to know its position).
        /// Subsequent re-parents (adoption-agency) don't re-emit.
        emitted: bool,
    },
    Comment(StrTendril),
}

/// HTML5 void elements — never have content, never get an end tag
/// in source, never appear on the open-elements stack.  We exclude
/// them from our own stack tracking so they don't accumulate as
/// "still open" entries to be drained at finish-time.
fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
            | "keygen"
            | "menuitem"
    )
}

/// Owned form of an element's qualified name, returned from
/// `TreeSink::elem_name`.  Same trick as in the batch sink — clone
/// the cheap atoms so the returned value doesn't borrow from the
/// sink's `RefCell`.
pub(crate) struct OwnedElemName {
    inner: QualName,
}

impl std::fmt::Debug for OwnedElemName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl ElemName for OwnedElemName {
    fn ns(&self) -> &H5Namespace {
        &self.inner.ns
    }
    fn local_name(&self) -> &LocalName {
        &self.inner.local
    }
}

struct StreamState<E: EventEmit> {
    arena: Vec<StreamNode>,
    emit: E,
    /// Our own open-elements stack, tracked from `append` calls.
    /// We can't rely on html5ever's `pop()` callback — it isn't
    /// invoked for every element that gets implicitly closed (e.g.
    /// `</p>` close_p_element doesn't always go through the sink
    /// pop path).  Tracking it ourselves from append parents lets
    /// us emit `EndElement` for every opened element correctly.
    open_stack: Vec<usize>,
    /// Total bytes of accumulated text content — checked against
    /// [`HtmlParseOptions::max_text_bytes`].
    text_bytes: u64,
    /// First fatal error encountered, propagated from `finish()`.
    fatal: Option<XmlError>,
}

pub(crate) struct StreamingSink<E: EventEmit> {
    state: RefCell<StreamState<E>>,
    quirks: Cell<H5QuirksMode>,
    opts: HtmlParseOptions,
    aborted: Cell<bool>,
}

impl<E: EventEmit> StreamingSink<E> {
    fn new(opts: HtmlParseOptions, emit: E) -> Self {
        let mut arena = Vec::with_capacity(32);
        // Slot 0 is the Document.
        arena.push(StreamNode::Document);
        Self {
            state: RefCell::new(StreamState {
                arena,
                emit,
                open_stack: Vec::with_capacity(32),
                text_bytes: 0,
                fatal: None,
            }),
            quirks: Cell::new(H5QuirksMode::NoQuirks),
            opts,
            aborted: Cell::new(false),
        }
    }

    /// Close all elements on the open stack down to (but not
    /// including) `parent_id`.  If `parent_id` is the Document
    /// (id 0) or isn't on the stack at all, close everything.
    /// Emits `EndElement` for each closed element.
    fn close_to_parent(s: &mut StreamState<E>, parent_id: usize) {
        // Find parent_id from the top of the stack.  Pop everything above it.
        if parent_id == 0 {
            // Document parent — nothing to pop down to (Document is
            // not on the stack).  Don't touch the stack.
            return;
        }
        // Walk from top downward.
        while let Some(&top) = s.open_stack.last() {
            if top == parent_id {
                return;
            }
            // Top is not the parent.  Pop it and emit EndElement.
            let popped = s.open_stack.pop().unwrap();
            if let StreamNode::Element { name, .. } = &s.arena[popped] {
                let local = name.local.to_string();
                s.emit.emit(OwnedEvent::EndElement { name: local });
            }
            // If parent_id isn't on the stack at all, this loop
            // would drain the whole stack.  That's the right
            // behaviour for foster-parented / re-parented nodes
            // where html5ever has lost its tree shape.
        }
    }

    fn alloc(&self, node: StreamNode) -> usize {
        let mut s = self.state.borrow_mut();
        let id = s.arena.len();
        s.arena.push(node);
        id
    }

    fn record_error(&self, msg: impl Into<String>, level: ErrorLevel) {
        let err = XmlError::new(ErrorDomain::Html, level, msg);
        let mut s = self.state.borrow_mut();
        if level == ErrorLevel::Fatal && s.fatal.is_none() {
            s.fatal = Some(err.clone());
        }
        s.emit.emit(OwnedEvent::ParseError(err));
    }

    fn abort(&self, msg: impl Into<String>) {
        if !self.aborted.get() {
            self.aborted.set(true);
            self.record_error(msg, ErrorLevel::Fatal);
        }
    }
}

impl<E: EventEmit + 'static> TreeSink for StreamingSink<E> {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a>
        = OwnedElemName
    where
        Self: 'a;

    fn finish(self) -> Self {
        self
    }

    fn parse_error(&self, msg: Cow<'static, str>) {
        let level = if self.opts.recovery_mode {
            ErrorLevel::Error
        } else if self.state.borrow().fatal.is_none() {
            ErrorLevel::Fatal
        } else {
            ErrorLevel::Error
        };
        self.record_error(msg.into_owned(), level);
    }

    fn get_document(&self) -> usize {
        0
    }

    fn elem_name<'a>(&'a self, target: &'a usize) -> OwnedElemName {
        let s = self.state.borrow();
        match &s.arena[*target] {
            StreamNode::Element { name, .. } => OwnedElemName { inner: name.clone() },
            _ => panic!("elem_name called on non-element node {target}"),
        }
    }

    fn create_element(
        &self,
        name: QualName,
        attrs: Vec<H5Attribute>,
        _flags: ElementFlags,
    ) -> usize {
        self.alloc(StreamNode::Element {
            name,
            attrs,
            emitted: false,
        })
    }

    fn create_comment(&self, text: StrTendril) -> usize {
        self.alloc(StreamNode::Comment(text))
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> usize {
        // HTML5 treats source PIs as bogus comments — html5ever will
        // not normally call this for HTML input, but the trait
        // requires it.
        self.alloc(StreamNode::Comment(StrTendril::new()))
    }

    fn append(&self, parent: &usize, child: NodeOrText<usize>) {
        if self.aborted.get() {
            return;
        }
        let parent = *parent;
        match child {
            NodeOrText::AppendText(text) => {
                let mut s = self.state.borrow_mut();
                let added = text.len() as u64;
                if s.text_bytes.saturating_add(added) > self.opts.max_text_bytes {
                    drop(s);
                    self.abort(format!(
                        "max_text_bytes ({}) exceeded",
                        self.opts.max_text_bytes
                    ));
                    return;
                }
                s.text_bytes += added;
                // Text doesn't move the open-elements stack.  But
                // if the text's parent isn't on top, we're either
                // appending into an ancestor (close down to it) or
                // into something fully outside the stack (drain).
                Self::close_to_parent(&mut s, parent);
                s.emit.emit(OwnedEvent::Text(text.to_string()));
            }
            NodeOrText::AppendNode(child_id) => {
                let mut s = self.state.borrow_mut();
                // Close everything above `parent` in the open-elements
                // stack — html5ever doesn't always call sink.pop()
                // for implicit closes, so this is how we get
                // EndElement events out for `<p>foo<p>bar` etc.
                Self::close_to_parent(&mut s, parent);
                match &mut s.arena[child_id] {
                    StreamNode::Element {
                        name,
                        attrs,
                        emitted,
                    } => {
                        if *emitted {
                            // Re-parent (adoption agency) — element
                            // already emitted, don't double-emit.
                            // Push it back onto the stack so closes
                            // continue to work.
                            s.open_stack.push(child_id);
                            return;
                        }
                        *emitted = true;
                        let local_name = name.local.to_string();
                        let owned_attrs: Vec<OwnedAttr> = attrs
                            .iter()
                            .map(|a| OwnedAttr {
                                name: a.name.local.to_string(),
                                value: a.value.to_string(),
                            })
                            .collect();
                        // Void elements never close — don't push.
                        // Non-void elements get pushed so we can
                        // emit EndElement when they're closed.
                        if !is_void_element(&local_name) {
                            s.open_stack.push(child_id);
                        }
                        s.emit.emit(OwnedEvent::StartElement {
                            name: local_name,
                            attrs: owned_attrs,
                        });
                    }
                    StreamNode::Comment(text) => {
                        let content = text.to_string();
                        s.emit.emit(OwnedEvent::Comment(content));
                    }
                    StreamNode::Document => {
                        // Document never gets appended to anything.
                    }
                }
            }
        }
    }

    fn append_based_on_parent_node(
        &self,
        element: &usize,
        prev_element: &usize,
        child: NodeOrText<usize>,
    ) {
        // Same logic as BatchSink: if the element has a parent we
        // append before-sibling, otherwise append to prev_element.
        // For streaming, both paths go through `append` — the event
        // semantics don't depend on physical insertion position.
        let _ = prev_element;
        self.append(element, child);
    }

    fn append_doctype_to_document(
        &self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        if self.aborted.get() {
            return;
        }
        let mut s = self.state.borrow_mut();
        s.emit.emit(OwnedEvent::Doctype {
            name: name.to_string(),
            public_id: public_id.to_string(),
            system_id: system_id.to_string(),
        });
    }

    fn pop(&self, _node: &usize) {
        // Intentionally no-op: html5ever doesn't call pop() for
        // every implicitly-closed element (close_p_element and
        // similar paths bypass it), so we can't rely on it as the
        // sole "end of element" signal.  Instead we track the
        // open-elements stack ourselves from `append` calls and
        // emit `EndElement` via `close_to_parent` when needed
        // (and from `finish`-time draining of remaining opens).
    }

    fn get_template_contents(&self, target: &usize) -> usize {
        // We don't model template-contents documents in v1 — see
        // batch sink note.  Returning the element itself works for
        // html5ever's bookkeeping purposes; consumers see the
        // template element's children in the regular event stream.
        *target
    }

    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, mode: H5QuirksMode) {
        self.quirks.set(mode);
    }

    fn append_before_sibling(&self, sibling: &usize, new_node: NodeOrText<usize>) {
        // Streaming doesn't model insertion position — just emit
        // the event the same way `append` does.  Consumers
        // operating on event streams can't observe the structural
        // distinction anyway.
        self.append(sibling, new_node);
    }

    fn add_attrs_if_missing(&self, target: &usize, attrs: Vec<H5Attribute>) {
        // The HTML5 spec calls this in two cases:
        //   - Duplicate `<html>` / `<body>` start tags (rare)
        //   - Attribute deduplication
        // For streaming, we've already emitted StartElement by now;
        // we'd have to emit a synthetic "attr-update" event to
        // surface this, which doesn't fit the standard event model.
        // We update the arena entry so subsequent elem_name and
        // pop work consistently, but don't emit anything.
        let mut s = self.state.borrow_mut();
        if let StreamNode::Element { attrs: existing, .. } = &mut s.arena[*target] {
            for new_attr in attrs {
                if !existing.iter().any(|a| a.name == new_attr.name) {
                    existing.push(new_attr);
                }
            }
        }
    }

    fn remove_from_parent(&self, _target: &usize) {
        // Adoption agency uses this before re-inserting elsewhere.
        // We don't emit any synthetic "removed" event — the
        // subsequent re-insert is suppressed by the `emitted` flag,
        // so the consumer sees the element only once (in its
        // original position).  Acceptable approximation; full
        // adoption-agency event modelling is out of scope for v1.
    }

    fn reparent_children(&self, _node: &usize, _new_parent: &usize) {
        // Same rationale as remove_from_parent — children stay
        // emitted in their original position; we do not emit
        // synthetic events for the move.
    }
}

// ── Push API: HtmlSaxHandler + HtmlSaxParser ─────────────────────────────────

/// Callback trait for the push-based [`HtmlSaxParser`].  All methods
/// have default no-op implementations — implementers override only
/// the events they care about (e.g. an `<a href>` extractor only
/// needs `start_element`).
///
/// libxml2 / lxml / Nokogiri analogue.  Argument shape mirrors
/// libxml2's xmlSAXHandler — flat parameters per callback rather
/// than packing them into payload structs.
pub trait HtmlSaxHandler {
    fn start_element(&mut self, _name: &str, _attrs: HtmlAttrs<'_>) {}
    fn end_element(&mut self, _name: &str) {}
    fn text(&mut self, _content: &str) {}
    fn comment(&mut self, _content: &str) {}
    fn doctype(&mut self, _name: &str, _public_id: &str, _system_id: &str) {}
    fn parse_error(&mut self, _err: &XmlError) {}
    /// Called once after the last token has been processed.
    fn end_document(&mut self) {}
}

/// Push-based HTML parser.  Caller drives input chunks via `feed`;
/// the parser dispatches events synchronously into the registered
/// handler as they're produced.
///
/// libxml2 `htmlCreatePushParserCtxt` + `htmlParseChunk` analogue,
/// or lxml's `HTMLParser(target=…)` + `parser.feed(bytes)`.
///
/// # Example
/// ```
/// use sup_xml_core::html::{HtmlAttrs, HtmlSaxHandler, HtmlSaxParser};
///
/// #[derive(Default)]
/// struct LinkCollector {
///     hrefs: Vec<String>,
/// }
///
/// impl HtmlSaxHandler for LinkCollector {
///     fn start_element(&mut self, name: &str, attrs: HtmlAttrs<'_>) {
///         if name == "a" {
///             if let Some(href) = attrs.get("href") {
///                 self.hrefs.push(href.to_string());
///             }
///         }
///     }
/// }
///
/// let html = r#"<a href="/x">x</a><a href="/y">y</a>"#;
/// let mut p = HtmlSaxParser::new(LinkCollector::default());
/// p.feed(html).unwrap();
/// let collector = p.finish().unwrap();
/// assert_eq!(collector.hrefs, vec!["/x", "/y"]);
/// ```
pub struct HtmlSaxParser<H: HtmlSaxHandler + 'static> {
    parser: Parser<StreamingSink<CallbackEmit<H>>>,
}

impl<H: HtmlSaxHandler + 'static> HtmlSaxParser<H> {
    /// Create a push parser with default options.
    pub fn new(handler: H) -> Self {
        Self::with_opts(handler, HtmlParseOptions::default())
    }

    /// Create a push parser with explicit options.
    pub fn with_opts(handler: H, opts: HtmlParseOptions) -> Self {
        let sink = StreamingSink::new(opts.clone(), CallbackEmit::new(handler));
        let parser = parse_document(sink, make_h5_opts(&opts));
        Self { parser }
    }

    /// Feed a chunk of UTF-8 text.  Drives the tokenizer and
    /// dispatches any emitted events synchronously into the handler
    /// before returning.
    ///
    /// Errors only when an internal limit (e.g. `max_text_bytes`)
    /// is exceeded.
    pub fn feed(&mut self, chunk: &str) -> Result<()> {
        let tendril = StrTendril::from_slice(chunk);
        self.parser.process(tendril);
        // After process(), check for fatal aborts.
        let s = self.parser.tokenizer.sink.sink.state.borrow();
        if let Some(err) = &s.fatal {
            if self.parser.tokenizer.sink.sink.aborted.get() {
                return Err(err.clone());
            }
        }
        Ok(())
    }

    /// Signal end-of-input.  Flushes any pending text, dispatches
    /// EndElement events for any still-open elements, then calls
    /// `end_document`.  Returns the handler.
    pub fn finish(self) -> Result<H> {
        // Capture fatal-error state before consuming the parser.
        let fatal = self.parser.tokenizer.sink.sink.state.borrow().fatal.clone();
        let aborted = self.parser.tokenizer.sink.sink.aborted.get();
        let sink = self.parser.finish();
        // sink.finish() returned `Self` from our TreeSink impl.
        let mut state = sink.state.into_inner();
        // Drain any still-open elements as EndElement events first
        // — same reason as `HtmlReader::finish_parser`: html5ever
        // doesn't reliably call pop() for everything at end-of-input.
        while let Some(open_id) = state.open_stack.pop() {
            if let StreamNode::Element { name, .. } = &state.arena[open_id] {
                let local = name.local.to_string();
                state.emit.emit(OwnedEvent::EndElement { name: local });
            }
        }
        // Final end_document dispatch (also flushes any pending text).
        state.emit.emit(OwnedEvent::Eof);
        if aborted {
            return Err(fatal.unwrap_or_else(|| {
                XmlError::new(ErrorDomain::Html, ErrorLevel::Fatal, "aborted")
            }));
        }
        Ok(state.emit.handler)
    }

    // No `handler()` borrow accessor in v1: the natural path
    // through `RefCell::borrow().emit.handler` produces a `Ref<'_,
    // _>` that doesn't compose cleanly with returning `&H`.  Users
    // who want to inspect progress should look at the returned
    // handler after `finish()`.
}

// ── Pull API: HtmlReader / HtmlBytesReader ───────────────────────────────────

/// Pull-based HTML event reader (XmlReader twin).  Iterate events
/// with `next()`; the reader feeds the next 8 KiB chunk into
/// html5ever whenever its internal queue runs dry.
///
/// # Example
/// ```
/// use sup_xml_core::html::{HtmlReader, HtmlEvent};
///
/// let mut r = HtmlReader::new("<p>hello <b>world</b></p>");
/// loop {
///     match r.next().unwrap() {
///         HtmlEvent::Eof => break,
///         HtmlEvent::StartElement { name, .. } => println!("start: {name}"),
///         _ => {}
///     }
/// }
/// ```
pub struct HtmlReader<'src> {
    inner: ReaderInner<'src>,
}

/// Bytes-flavoured pull reader.  Same as [`HtmlReader`] but takes
/// raw bytes — html5ever does lossy UTF-8 decoding internally.
pub struct HtmlBytesReader<'src> {
    inner: ReaderInner<'src>,
}

struct ReaderInner<'src> {
    parser: Parser<StreamingSink<BufferedEmit>>,
    /// Input bytes the reader feeds to html5ever.  Borrowed for the
    /// str path (already UTF-8), owned for the bytes path after
    /// encoding sniffing + transcoding.
    input: Cow<'src, [u8]>,
    fed_pos: usize,
    chunk_size: usize,
    finished: bool,
    /// Owned data backing the most recent event.  Held so the
    /// borrowed `HtmlEvent<'_>` returned from `next()` stays valid
    /// until the next call.
    current: Option<OwnedEvent>,
    /// Errors recovered while parsing — same contract as
    /// `parse_html_str_with_recovered`.
    recovered: Vec<XmlError>,
    /// Whether to return Err on the next strict-mode parse error.
    strict: bool,
}

impl<'src> ReaderInner<'src> {
    /// Create from already-UTF-8 input.  Used by `HtmlReader::new`.
    fn from_utf8_str(input: &'src str, opts: HtmlParseOptions) -> Self {
        Self::with_cow(Cow::Borrowed(input.as_bytes()), opts)
    }

    /// Create from raw bytes.  Runs WHATWG byte-stream sniffing and
    /// transcodes to UTF-8 before feeding to html5ever.
    fn from_bytes(input: &'src [u8], opts: HtmlParseOptions) -> Result<Self> {
        let (decoded, _enc) = super::encoding::decode_html_input(input, &opts)?;
        let cow: Cow<'src, [u8]> = match decoded {
            Cow::Borrowed(b) => {
                // We can borrow as long as transcoding was a no-op
                // (input was already UTF-8 / ASCII).  But the
                // borrow's lifetime is tied to the *input* slice,
                // and `decode_html_input` may have stripped a BOM
                // — which still produces a borrow into `input`,
                // so this is sound.
                if std::ptr::eq(b.as_ptr(), input.as_ptr()) || input.starts_with(b) {
                    // SAFETY-equivalent (no unsafe used): we're just
                    // re-asserting that `b` is a subslice of `input`,
                    // which has lifetime 'src.  The Cow we got from
                    // `decode_html_input` had a shorter lifetime
                    // because of the function signature, but the
                    // bytes themselves live as long as `input`.
                    // Reconstruct the borrow with the wider lifetime
                    // by slicing `input` directly.
                    let offset = b.as_ptr() as usize - input.as_ptr() as usize;
                    Cow::Borrowed(&input[offset..offset + b.len()])
                } else {
                    Cow::Owned(b.to_vec())
                }
            }
            Cow::Owned(v) => Cow::Owned(v),
        };
        Ok(Self::with_cow(cow, opts))
    }

    fn with_cow(input: Cow<'src, [u8]>, opts: HtmlParseOptions) -> Self {
        let strict = !opts.recovery_mode;
        let sink = StreamingSink::new(opts.clone(), BufferedEmit::new());
        let parser = parse_document(sink, make_h5_opts(&opts));
        Self {
            parser,
            input,
            fed_pos: 0,
            chunk_size: 8 * 1024,
            finished: false,
            current: None,
            recovered: Vec::new(),
            strict,
        }
    }

    /// Borrow the queue mutably to pop the next event.
    fn pop_event(&mut self) -> Option<OwnedEvent> {
        let mut s = self.parser.tokenizer.sink.sink.state.borrow_mut();
        s.emit.queue.pop_front()
    }

    /// True if the sink has aborted (depth/text-byte limit, or
    /// strict-mode parse error).
    fn aborted(&self) -> bool {
        self.parser.tokenizer.sink.sink.aborted.get()
    }

    fn fatal(&self) -> Option<XmlError> {
        self.parser.tokenizer.sink.sink.state.borrow().fatal.clone()
    }

    /// Feed the next chunk into the tokenizer.  Returns true if any
    /// input was fed.
    fn feed_chunk(&mut self) -> bool {
        if self.fed_pos >= self.input.len() {
            return false;
        }
        let end = (self.fed_pos + self.chunk_size).min(self.input.len());
        // Walk back to a UTF-8 char boundary so we don't split
        // mid-codepoint between chunks.  At most 3 bytes back.
        let chunk_bytes = trim_to_utf8_boundary(&self.input[self.fed_pos..end]);
        let actual_end = self.fed_pos + chunk_bytes.len();
        if chunk_bytes.is_empty() {
            // Pathological — single multi-byte char split across
            // 8 KiB?  Feed the original chunk lossily.
            let lossy = String::from_utf8_lossy(&self.input[self.fed_pos..end]).into_owned();
            self.parser.process(StrTendril::from_slice(&lossy));
            self.fed_pos = end;
        } else {
            let lossy = String::from_utf8_lossy(chunk_bytes);
            self.parser.process(StrTendril::from_slice(&lossy));
            self.fed_pos = actual_end;
        }
        true
    }

    fn finish_parser(&mut self) {
        if !self.finished {
            // `Parser::finish` consumes; we replace with a sentinel
            // by swapping a fresh parser.  But the parser owns the
            // sink which holds our queue — we can't just discard it.
            // Instead, drive the tokenizer to end-of-input directly.
            self.parser.tokenizer.end();
            self.finished = true;
            // Drain any still-open elements as EndElement events.
            // (html5ever doesn't reliably call pop() at end-of-input
            // for everything; we close them ourselves.)
            let mut s = self.parser.tokenizer.sink.sink.state.borrow_mut();
            while let Some(open_id) = s.open_stack.pop() {
                if let StreamNode::Element { name, .. } = &s.arena[open_id] {
                    let local = name.local.to_string();
                    s.emit.emit(OwnedEvent::EndElement { name: local });
                }
            }
        }
    }

    /// Pull events repeatedly: drain queue, feed more, until we
    /// have an event to return or hit Eof.
    fn pull(&mut self) -> Result<OwnedEvent> {
        loop {
            if let Some(event) = self.pop_event() {
                // Check for parse errors / fatals that should
                // surface as Err in strict mode or as recovered
                // entries in lenient mode.
                if let OwnedEvent::ParseError(err) = &event {
                    let err = err.clone();
                    if self.strict {
                        return Err(err);
                    } else {
                        self.recovered.push(err);
                        continue;
                    }
                }
                if self.aborted() {
                    if let Some(fatal) = self.fatal() {
                        return Err(fatal);
                    }
                }
                return Ok(event);
            }
            // Queue empty — feed more or finalise.
            if !self.finished && self.feed_chunk() {
                continue;
            }
            if !self.finished {
                self.finish_parser();
                // After end(), the sink may have queued a final
                // burst of events.  Loop once more to drain them.
                continue;
            }
            // No more input, no more events.
            return Ok(OwnedEvent::Eof);
        }
    }

    /// Recovered errors so far.  Same semantics as
    /// `XmlReader::recovered_errors`.
    fn recovered_errors(&self) -> &[XmlError] {
        &self.recovered
    }
}

/// Walk back from the end of `bytes` to the nearest UTF-8 char
/// boundary.  Returns the truncated slice.  Used to avoid splitting
/// a multi-byte codepoint across two chunks fed to html5ever.
fn trim_to_utf8_boundary(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }
    let mut end = bytes.len();
    while end > 0 {
        // ASCII byte: definitely a boundary.
        // UTF-8 continuation byte (10xxxxxx): not a boundary.
        // UTF-8 start byte (0xxxxxxx, 11xxxxxx): boundary.
        let b = bytes[end - 1];
        if b < 0x80 || b >= 0xC0 {
            // Either ASCII (already a boundary at `end`) or a
            // start byte — meaning the byte at `end - 1` starts a
            // multi-byte sequence.  Truncate to *before* the start
            // byte so we don't split it.
            if b >= 0xC0 {
                end -= 1;
            }
            break;
        }
        end -= 1;
        // Limit the walk-back to 4 bytes (max UTF-8 sequence length).
        if bytes.len() - end > 4 {
            break;
        }
    }
    &bytes[..end]
}

impl<'src> HtmlReader<'src> {
    pub fn new(input: &'src str) -> Self {
        Self::with_opts(input, HtmlParseOptions::default())
    }

    pub fn with_opts(input: &'src str, opts: HtmlParseOptions) -> Self {
        Self {
            inner: ReaderInner::from_utf8_str(input, opts),
        }
    }

    /// Pull the next event.  Returns `Eof` when input is exhausted.
    pub fn next(&mut self) -> Result<HtmlEvent<'_>> {
        let event = self.inner.pull()?;
        self.inner.current = Some(event);
        Ok(event_borrow(self.inner.current.as_ref().unwrap()))
    }

    /// Errors recovered while parsing (only populated in lenient
    /// mode).  Strict mode returns an `Err` from `next()` instead.
    pub fn recovered_errors(&self) -> &[XmlError] {
        self.inner.recovered_errors()
    }
}

impl<'src> HtmlBytesReader<'src> {
    /// Construct from raw bytes using default options.  Runs WHATWG
    /// byte-stream encoding sniffing (BOM → meta charset prescan →
    /// Windows-1252 fallback) and transcodes to UTF-8 before
    /// streaming events.  See [`HtmlParseOptions::encoding_override`]
    /// to bypass sniffing with a known encoding.
    pub fn new(input: &'src [u8]) -> Result<Self> {
        Self::with_opts(input, HtmlParseOptions::default())
    }

    pub fn with_opts(input: &'src [u8], opts: HtmlParseOptions) -> Result<Self> {
        Ok(Self {
            inner: ReaderInner::from_bytes(input, opts)?,
        })
    }

    pub fn next(&mut self) -> Result<HtmlEvent<'_>> {
        let event = self.inner.pull()?;
        self.inner.current = Some(event);
        Ok(event_borrow(self.inner.current.as_ref().unwrap()))
    }

    pub fn recovered_errors(&self) -> &[XmlError] {
        self.inner.recovered_errors()
    }
}

/// Convert an owned event into the borrowing public form.
fn event_borrow(owned: &OwnedEvent) -> HtmlEvent<'_> {
    match owned {
        OwnedEvent::StartElement { name, attrs } => HtmlEvent::StartElement {
            name: name.as_str(),
            attributes: HtmlAttrs { inner: attrs.as_slice() },
        },
        OwnedEvent::EndElement { name } => HtmlEvent::EndElement {
            name: name.as_str(),
        },
        OwnedEvent::Text(content) => HtmlEvent::Text(content.as_str()),
        OwnedEvent::Comment(content) => HtmlEvent::Comment(content.as_str()),
        OwnedEvent::Doctype {
            name,
            public_id,
            system_id,
        } => HtmlEvent::Doctype {
            name: name.as_str(),
            public_id: public_id.as_str(),
            system_id: system_id.as_str(),
        },
        OwnedEvent::Eof => HtmlEvent::Eof,
        OwnedEvent::ParseError(_) => {
            // ParseErrors are filtered out in `pull()` (returned as
            // Err in strict mode or pushed into `recovered` in
            // lenient mode).  Should never reach here.
            HtmlEvent::Eof
        }
    }
}

// ── shared helper: html5ever ParseOpts construction ──────────────────────────

fn make_h5_opts(opts: &HtmlParseOptions) -> ParseOpts {
    ParseOpts {
        tokenizer: TokenizerOpts {
            discard_bom: opts.discard_bom,
            ..Default::default()
        },
        tree_builder: TreeBuilderOpts {
            scripting_enabled: opts.scripting_enabled,
            iframe_srcdoc: opts.iframe_srcdoc,
            ..Default::default()
        },
    }
}
