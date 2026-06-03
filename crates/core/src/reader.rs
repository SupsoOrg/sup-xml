//! `XmlReader` — string-typed streaming SAX-style API.
//!
//! Thin wrapper around [`XmlBytesReader`](crate::xml_bytes_reader::XmlBytesReader).
//! The engine is byte-level (raw `Cow<'src, [u8]>` events with no UTF-8
//! cast); this module converts each event's payload to `Cow<'src, str>` at
//! the boundary via `from_utf8_unchecked`.  The bytes are valid UTF-8 by
//! the Scanner's construction-time invariant, so the cast is a no-op.
//!
//! ## Why two readers exist
//!
//! Some callers prefer raw bytes — byte-literal tag matching
//! (`name == b"item"`), hash/digest pipelines, format conversion, byte
//! forwarding — and would rather not pay for the type system's `&str`
//! guarantee.  `XmlBytesReader` serves them.  Most callers want validated
//! strings; `XmlReader` (this module) serves them and is the recommended
//! default.  The two share a single parser; the only difference is the
//! payload type each emits.

use std::borrow::Cow;

use memchr::memchr;

use crate::error::Result;
use crate::options::ParseOptions;
use crate::xml_bytes_reader::{
    BytesAttrs, BytesCData, BytesComment, BytesEndTag, BytesEvent, BytesPi, BytesStartTag,
    BytesText, XmlBytesReader, XmlDeclInfo,
};

// ── public types ──────────────────────────────────────────────────────────────

/// A single attribute from a start tag, with a zero-copy value when possible.
#[derive(Debug)]
pub struct Attr<'src> {
    /// Source-borrowed attribute name — XML names can't contain entity
    /// refs, so no allocation is ever required.
    pub name:  &'src str,
    /// Attribute value.  Borrowed from source when no entity references
    /// appeared in the literal; owned otherwise.
    pub value: Cow<'src, str>,
}

impl<'src> Attr<'src> {
    /// Attribute name as a borrowed source slice.  Same as `self.name`.
    pub fn name(&self)  -> &'src str { self.name }
    /// Attribute value, borrowing from the source when possible.
    pub fn value(&self) -> &str      { &self.value }
}

/// Lazy iterator over the attributes of a start tag.
///
/// Returned by [`StartTag::attrs`].  Each call to `.next()` parses one
/// `name="value"` pair from the source.  Attributes you never iterate
/// cost nothing — this is the win over the eager
/// [`XmlReader::next_into`] API.
///
/// # Error semantics
///
/// If an attribute fails to parse, the iterator yields `Some(Err(_))`
/// once and returns `None` on every subsequent call.  A malformed
/// attribute terminates iteration; the caller should bail.
pub struct Attrs<'r, 'src> {
    inner: BytesAttrs<'r, 'src>,
}

impl<'src> Iterator for Attrs<'_, 'src> {
    type Item = Result<Attr<'src>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|res| res.map(|ba| Attr {
            // SAFETY: Scanner invariant — name bytes are valid UTF-8.
            name:  unsafe { std::str::from_utf8_unchecked(ba.name) },
            value: cow_bytes_to_str(ba.value),
        }))
    }
}

// ── tag types (str-typed wrappers over the bytes-typed BytesXxx) ─────────────

/// A start-tag event.  Source offsets only — no name extraction or
/// attribute parsing happens until you call a method.
pub struct StartTag<'r, 'src> {
    inner: BytesStartTag<'r, 'src>,
}

impl<'r, 'src> StartTag<'r, 'src> {
    /// Element name as a string slice.  Borrowed from the source on
    /// the common path; tied to `&self` for start tags read from
    /// inside an entity-replacement stream.  Use [`name_cow`] when
    /// you need a `'src`-lifetime string.
    ///
    /// [`name_cow`]: StartTag::name_cow
    #[inline]
    pub fn name(&self) -> &str {
        // SAFETY: Scanner invariant — name bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(self.inner.name()) }
    }

    /// Element name with the `'src` lifetime preserved when possible.
    /// Source-borrowed names round-trip without copying;
    /// entity-stream names come back as `Cow::Owned`.
    pub fn name_cow(&self) -> Cow<'src, str> {
        match self.inner.name_cow() {
            // SAFETY: Scanner invariant — name bytes are valid UTF-8.
            Cow::Borrowed(b) => Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(b) }),
            Cow::Owned(v)    => Cow::Owned(unsafe { String::from_utf8_unchecked(v) }),
        }
    }

    /// Iterate the attributes (consumes the tag).
    pub fn attrs(self) -> Attrs<'r, 'src> {
        Attrs { inner: self.inner.attrs() }
    }

    /// Raw byte range of the attrs region (between the name and the
    /// closing `>` / `/>`).
    #[inline]
    pub fn attrs_str(&self) -> &'src str {
        // SAFETY: Scanner invariant — bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(self.inner.attrs_bytes()) }
    }
}

impl std::fmt::Debug for StartTag<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StartTag")
            .field("name",  &self.name())
            .field("attrs", &self.attrs_str())
            .finish()
    }
}

/// An end-tag event (`</element>` — or the synthetic close emitted
/// after every self-closing `<element/>`).
pub struct EndTag<'src> {
    inner: BytesEndTag<'src>,
}

impl<'src> EndTag<'src> {
    #[inline]
    pub fn name(&self) -> &'src str {
        // SAFETY: Scanner invariant — name bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(self.inner.name()) }
    }
}

impl std::fmt::Debug for EndTag<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndTag").field("name", &self.name()).finish()
    }
}

/// Character-data text between elements.
pub struct Text<'src> { inner: BytesText<'src> }
impl<'src> Text<'src> {
    #[inline] pub fn as_str(&self) -> &str {
        // SAFETY: Scanner invariant.
        unsafe { std::str::from_utf8_unchecked(self.inner.as_bytes()) }
    }
    pub fn into_str(self) -> Cow<'src, str> { cow_bytes_to_str(self.inner.into_bytes()) }
}
impl std::fmt::Debug for Text<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Text({:?})", self.as_str())
    }
}

/// A `<![CDATA[…]]>` section.
pub struct CData<'src> { inner: BytesCData<'src> }
impl<'src> CData<'src> {
    #[inline] pub fn as_str(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.inner.as_bytes()) }
    }
    pub fn into_str(self) -> Cow<'src, str> { cow_bytes_to_str(self.inner.into_bytes()) }
}
impl std::fmt::Debug for CData<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CData({:?})", self.as_str())
    }
}

/// An XML comment (`<!-- ... -->`).  The payload is the text strictly
/// between the delimiters.
pub struct Comment<'src> { inner: BytesComment<'src> }
impl<'src> Comment<'src> {
    #[inline] pub fn as_str(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.inner.as_bytes()) }
    }
    pub fn into_str(self) -> Cow<'src, str> { cow_bytes_to_str(self.inner.into_bytes()) }
}
impl std::fmt::Debug for Comment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Comment({:?})", self.as_str())
    }
}

/// A processing instruction (`<?target content?>`).
pub struct Pi<'src> { inner: BytesPi<'src> }
impl<'src> Pi<'src> {
    #[inline] pub fn target(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.inner.target()) }
    }
    #[inline] pub fn content(&self) -> &str {
        unsafe { std::str::from_utf8_unchecked(self.inner.content()) }
    }
    pub fn into_parts(self) -> (Cow<'src, str>, Cow<'src, str>) {
        let (t, c) = self.inner.into_parts();
        (cow_bytes_to_str(t), cow_bytes_to_str(c))
    }
}
impl std::fmt::Debug for Pi<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pi")
            .field("target",  &self.target())
            .field("content", &self.content())
            .finish()
    }
}

/// A streaming XML event with **lazy** access to its payload.
///
/// Returned by [`XmlReader::next`].  Each variant wraps a tag struct
/// whose methods (`name()`, `attrs()`, `as_str()`, etc.) extract data
/// on demand.  For an eager API that fills a caller-owned buffer see
/// [`XmlReader::next_into`] which returns [`EventInto`].
#[derive(Debug)]
pub enum Event<'r, 'src> {
    /// An opening (or empty-element) start tag.
    StartElement(StartTag<'r, 'src>),
    /// A closing tag.  Emitted once for each `StartElement`, including
    /// for empty elements (`<br/>` emits `StartElement` then `EndElement`).
    EndElement(EndTag<'src>),
    /// Character data between tags.
    Text(Text<'src>),
    /// A `<![CDATA[…]]>` section.
    CData(CData<'src>),
    /// An XML comment.
    Comment(Comment<'src>),
    /// A processing instruction.
    Pi(Pi<'src>),
    /// An unresolved entity reference (`&name;`) — emitted only when
    /// [`ParseOptions::resolve_entities`] is `false`.  Carries the
    /// entity name without the surrounding `&` / `;`.
    EntityRef(EntityRef<'src>),
    /// The document has been fully consumed.
    Eof,
}

/// `Event::EntityRef` payload — entity name only.  String-typed
/// counterpart to [`crate::xml_bytes_reader::BytesEntityRef`].
pub struct EntityRef<'src> {
    inner: crate::xml_bytes_reader::BytesEntityRef<'src>,
}

impl<'src> EntityRef<'src> {
    /// Entity name as a borrowed source slice (e.g. `"foo"` for
    /// the reference `&foo;`).
    #[inline]
    pub fn name(&self) -> &'src str {
        // SAFETY: Scanner UTF-8 invariant — name bytes are valid UTF-8.
        unsafe { std::str::from_utf8_unchecked(self.inner.name()) }
    }
}

impl std::fmt::Debug for EntityRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntityRef")
            .field("name", &self.name())
            .finish()
    }
}

/// A streaming XML event with **attributes already parsed into a caller-owned
/// buffer**.
///
/// Returned by [`XmlReader::next_into`].  The buffer the caller passes is
/// cleared on each call and filled with the start tag's attributes, allowing
/// buffer reuse across many events.  For lazy attribute access without a
/// buffer see [`XmlReader::next`] which returns [`Event`].
#[derive(Debug)]
pub enum EventInto<'src> {
    /// An opening (or empty-element) start tag.  Attributes are in the
    /// caller-owned buffer passed to [`XmlReader::next_into`].
    StartElement { name: Cow<'src, str> },
    /// A closing tag.  Emitted once for each `StartElement`, including for
    /// empty elements.
    EndElement { name: Cow<'src, str> },
    Text(Cow<'src, str>),
    CData(Cow<'src, str>),
    Comment(Cow<'src, str>),
    Pi { target: Cow<'src, str>, content: Cow<'src, str> },
    /// An unresolved entity reference (`&name;`) — emitted only when
    /// [`ParseOptions::resolve_entities`] is `false`.
    EntityRef { name: Cow<'src, str> },
    Eof,
}

// ── reader ────────────────────────────────────────────────────────────────────

/// Streaming XML reader with string-typed events.  Thin wrapper around
/// [`XmlBytesReader`](crate::xml_bytes_reader::XmlBytesReader) — the engine
/// runs in raw bytes and we re-label each event's payload as `Cow<'src, str>`
/// at the boundary.  The conversion is a no-op: bytes are valid UTF-8 by
/// the Scanner's construction-time invariant.
pub struct XmlReader<'src> {
    inner: XmlBytesReader<'src>,
}

impl<'src> XmlReader<'src> {
    /// Create a reader from a string slice.  The string must remain alive for
    /// the lifetime of the reader and all events it produces.
    #[allow(clippy::should_implement_trait)] // intentional: mirrors `FromStr` but `XmlReader<'src>` borrows
    pub fn from_str(input: &'src str) -> Self {
        Self { inner: XmlBytesReader::from_str(input) }
    }

    /// Create a reader from a byte slice.  Returns an error if the bytes are
    /// not valid UTF-8.
    pub fn from_bytes(src: &'src [u8]) -> Result<Self> {
        XmlBytesReader::from_bytes(src).map(|inner| Self { inner })
    }

    /// Create a reader from a byte slice, **skipping** the upfront UTF-8
    /// validation that [`from_bytes`](Self::from_bytes) performs.
    ///
    /// # Safety
    ///
    /// The bytes pointed to by `src` must be valid UTF-8 for the lifetime of
    /// the returned reader and for any [`Event`] it emits.  Violating this
    /// invariant is **undefined behaviour** — `Event` payloads are `&str`
    /// slices into the input, produced via [`std::str::from_utf8_unchecked`].
    pub unsafe fn from_bytes_unchecked(src: &'src [u8]) -> Self {
        Self { inner: unsafe { XmlBytesReader::from_bytes_unchecked(src) } }
    }

    /// Destructive in-place reader.  The reader is permitted to mutate
    /// `src` during parsing — used by [`crate::parser::parse_bytes_in_place`].
    ///
    /// # Safety
    ///
    /// `src` must be valid UTF-8.  Caller transfers exclusive write
    /// access to `src` for the reader's lifetime.
    pub unsafe fn from_bytes_in_place_unchecked(src: &'src mut [u8]) -> Self {
        Self { inner: unsafe { XmlBytesReader::from_bytes_in_place_unchecked(src) } }
    }

    /// Override the [`ParseOptions`] that the reader was constructed with.
    /// Use this to lower limits or skip checks for trusted input.
    pub fn with_options(self, opts: ParseOptions) -> Self {
        Self { inner: self.inner.with_options(opts) }
    }

    /// XML declaration fields parsed from the prolog.  Returns `None`
    /// before the first event has been read, or when the document has
    /// no `<?xml ... ?>` declaration.  See
    /// [`XmlBytesReader::xml_decl`] for the underlying definition.
    pub fn xml_decl(&self) -> Option<&XmlDeclInfo> {
        self.inner.xml_decl()
    }

    /// Non-fatal errors logged while parsing with
    /// `ParseOptions::recovery_mode = true`.  Empty in strict mode (errors
    /// are returned via `Err` from `next`/`next_into` instead).  See
    /// [`XmlBytesReader::recovered_errors`] for full semantics.
    pub fn recovered_errors(&self) -> &[crate::error::XmlError] {
        self.inner.recovered_errors()
    }

    /// Current byte offset into the original source.  Use with
    /// [`line_col`](Self::line_col) (or [`src_bytes`](Self::src_bytes)
    /// + [`crate::scanner::compute_line_col`]) to attribute
    /// diagnostics to a source position.
    ///
    /// For start-of-element offsets specifically, prefer
    /// [`last_start_offset`](Self::last_start_offset): it returns the
    /// `<` of the most recent StartElement, whereas `src_offset` will
    /// already be past the start tag's closing `>` by the time you
    /// read it.
    #[inline]
    pub fn src_offset(&self) -> usize {
        self.inner.src_offset()
    }

    /// Source byte offset of the `<` of the most recently emitted
    /// StartElement.  `None` before the first start tag, or when the
    /// start tag was read from inside an entity-replacement stream.
    /// Pairs with [`line_col_at`](Self::line_col_at) to anchor
    /// diagnostics at the right source position.
    #[inline]
    pub fn last_start_offset(&self) -> Option<usize> {
        self.inner.last_start_offset()
    }

    /// The original source bytes the reader is parsing.  Pairs with
    /// [`src_offset`](Self::src_offset) for translating byte positions
    /// into line/column.
    #[inline]
    pub fn src_bytes(&self) -> &'src [u8] {
        self.inner.src_bytes()
    }

    /// Translate the current reader position into a 1-based
    /// `(line, column)` pair.  Lazy: scans the prefix from byte 0
    /// to the current offset once per call (cost ~O(offset)).  Cheap
    /// enough for error paths; not for tight loops.
    #[inline]
    pub fn line_col(&self) -> (u32, u32) {
        crate::scanner::compute_line_col(self.src_bytes(), self.src_offset())
    }

    /// Translate an arbitrary byte offset (typically captured earlier
    /// via [`src_offset`](Self::src_offset)) into 1-based
    /// `(line, column)`.  Same cost model as
    /// [`line_col`](Self::line_col).
    #[inline]
    pub fn line_col_at(&self, offset: usize) -> (u32, u32) {
        crate::scanner::compute_line_col(self.src_bytes(), offset)
    }

    /// Read the next event with **lazy** attribute access.
    ///
    /// Returns an [`Event`] borrowing the reader for its lifetime.  Start tag
    /// events carry an [`Attrs`] iterator — iterate it to read attributes,
    /// ignore it to skip attribute parsing entirely.
    ///
    /// For an eager API that fills a caller-owned buffer with parsed
    /// attributes, see [`next_into`](Self::next_into).
    #[allow(clippy::should_implement_trait)] // can't impl `Iterator`: events borrow the reader
    pub fn next(&mut self) -> Result<Event<'_, 'src>> {
        match self.inner.next()? {
            BytesEvent::StartElement(t) => Ok(Event::StartElement(StartTag { inner: t })),
            BytesEvent::EndElement(t)   => Ok(Event::EndElement(EndTag { inner: t })),
            BytesEvent::Text(t)         => Ok(Event::Text(Text { inner: t })),
            BytesEvent::CData(s)        => Ok(Event::CData(CData { inner: s })),
            BytesEvent::Comment(s)      => Ok(Event::Comment(Comment { inner: s })),
            BytesEvent::Pi(p)           => Ok(Event::Pi(Pi { inner: p })),
            BytesEvent::EntityRef(e)    => Ok(Event::EntityRef(EntityRef { inner: e })),
            BytesEvent::Eof             => Ok(Event::Eof),
        }
    }

    /// Read the next event, eagerly parsing start-tag attributes into `buf`.
    ///
    /// `buf` is cleared on every call.  For `StartElement` events `buf` is
    /// filled with the element's attributes in source order; for other events
    /// `buf` is left empty.  Pass the same `Vec` across many calls to reuse
    /// its allocation.
    ///
    /// For lazy attribute access (zero work when you never read attrs), see
    /// [`next`](Self::next).
    pub fn next_into(&mut self, buf: &mut Vec<Attr<'src>>) -> Result<EventInto<'src>> {
        buf.clear();
        match self.next()? {
            Event::StartElement(tag) => {
                let name = tag.name_cow();
                for attr in tag.attrs() { buf.push(attr?); }
                Ok(EventInto::StartElement { name })
            }
            Event::EndElement(tag) => Ok(EventInto::EndElement {
                name: Cow::Borrowed(tag.name()),
            }),
            Event::Text(t)     => Ok(EventInto::Text(t.into_str())),
            Event::CData(s)    => Ok(EventInto::CData(s.into_str())),
            Event::Comment(s)  => Ok(EventInto::Comment(s.into_str())),
            Event::Pi(p)       => {
                let (target, content) = p.into_parts();
                Ok(EventInto::Pi { target, content })
            }
            Event::EntityRef(e) => Ok(EventInto::EntityRef {
                name: Cow::Borrowed(e.name()),
            }),
            Event::Eof         => Ok(EventInto::Eof),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Zero-cost conversion from `Cow<'src, [u8]>` to `Cow<'src, str>` — the
/// bytes are valid UTF-8 by the Scanner's invariant.
///
/// `&[u8]` ↔ `&str` and `Vec<u8>` ↔ `String` have identical memory layout,
/// so both arms compile to no-op casts (just type-system relabeling).
#[inline]
fn cow_bytes_to_str(c: Cow<'_, [u8]>) -> Cow<'_, str> {
    match c {
        // SAFETY: Scanner invariant — slices come from valid-UTF-8 input
        // (either the original source, which the constructor validated, or
        // an entity replacement built up by entity expansion which writes
        // only complete UTF-8 sequences).
        Cow::Borrowed(b) => Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(b) }),
        Cow::Owned(v)    => Cow::Owned(unsafe { String::from_utf8_unchecked(v)   }),
    }
}

/// Expand the five XML predefined entity references (`&amp;`, `&lt;`,
/// `&gt;`, `&quot;`, `&apos;`) and numeric character references (`&#NN;`,
/// `&#xNN;`) inside `s`.  Intended for callers using
/// [`ParseOptions::skip_entity_expansion`] who want to decode a specific
/// text payload on demand.
///
/// Returns `Cow::Borrowed(s)` when `s` contains no `&` — i.e. the
/// no-entity case is zero-copy.
///
/// **General entities** declared in a DTD (`<!ENTITY foo "...">`) are not
/// expanded here; the helper has no access to the document's entity table.
/// If you need DTD-defined entity expansion, don't enable
/// `skip_entity_expansion` in the first place.
pub fn unescape(s: &str) -> Cow<'_, str> {
    if memchr(b'&', s.as_bytes()).is_none() {
        return Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Find the `;`.  If missing or too far away, treat `&` as literal.
        let rest = &bytes[i + 1..];
        let semi = match memchr(b';', rest) {
            Some(n) if n <= 16 => n,
            _ => { out.push('&'); i += 1; continue; }
        };
        let name = &rest[..semi];
        match name {
            b"amp"  => { out.push('&'); }
            b"lt"   => { out.push('<'); }
            b"gt"   => { out.push('>'); }
            b"quot" => { out.push('"'); }
            b"apos" => { out.push('\''); }
            _ if name.starts_with(b"#") => {
                let cp: Option<u32> = if name.len() >= 2 && (name[1] == b'x' || name[1] == b'X') {
                    std::str::from_utf8(&name[2..]).ok()
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                } else {
                    std::str::from_utf8(&name[1..]).ok()
                        .and_then(|d| d.parse::<u32>().ok())
                };
                match cp.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None    => {
                        out.push('&');
                        out.push_str(unsafe { std::str::from_utf8_unchecked(name) });
                        out.push(';');
                    }
                }
            }
            _ => {
                out.push('&');
                out.push_str(unsafe { std::str::from_utf8_unchecked(name) });
                out.push(';');
            }
        }
        i += 1 + semi + 1;
    }
    Cow::Owned(out)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn events(src: &str) -> Vec<String> {
        let mut r = XmlReader::from_str(src);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        loop {
            match r.next_into(&mut buf).unwrap() {
                EventInto::StartElement { name } => {
                    let a: Vec<_> = buf.iter().map(|a| format!("{}={}", a.name, a.value)).collect();
                    if a.is_empty() { out.push(format!("<{name}>")); }
                    else            { out.push(format!("<{name} {}>", a.join(" "))); }
                }
                EventInto::EndElement { name }  => out.push(format!("</{name}>")),
                EventInto::Text(t)              => out.push(format!("T:{t}")),
                EventInto::CData(s)             => out.push(format!("CD:{s}")),
                EventInto::Comment(s)           => out.push(format!("C:{s}")),
                EventInto::Pi { target, .. }    => out.push(format!("PI:{target}")),
                EventInto::EntityRef { name }   => out.push(format!("E:{name}")),
                EventInto::Eof                  => break,
            }
        }
        out
    }

    #[test]
    fn minimal() {
        assert_eq!(events("<r/>"), vec!["<r>", "</r>"]);
    }

    #[test]
    fn nested() {
        assert_eq!(
            events("<a><b>hello</b></a>"),
            vec!["<a>", "<b>", "T:hello", "</b>", "</a>"],
        );
    }

    #[test]
    fn attributes_borrowed() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlReader::from_str(src);
        let mut buf = Vec::new();
        let ev = r.next_into(&mut buf).unwrap();
        assert!(matches!(&ev, EventInto::StartElement { name } if name == "el"));
        assert_eq!(buf.len(), 2);
        assert!(matches!(buf[0].value, Cow::Borrowed(_)), "no entity → should borrow");
    }

    #[test]
    fn attribute_entity_owned() {
        let src = r#"<el v="a&amp;b"/>"#;
        let mut r = XmlReader::from_str(src);
        let mut buf = Vec::new();
        r.next_into(&mut buf).unwrap();
        assert_eq!(buf[0].value.as_ref(), "a&b");
        assert!(matches!(buf[0].value, Cow::Owned(_)), "entity → must allocate");
    }

    #[test]
    fn text_borrowed() {
        let src = "<r>hello world</r>";
        let mut r = XmlReader::from_str(src);
        let mut buf = Vec::new();
        r.next_into(&mut buf).unwrap(); // StartElement
        let ev = r.next_into(&mut buf).unwrap();
        assert!(matches!(ev, EventInto::Text(Cow::Borrowed("hello world"))));
    }

    #[test]
    fn cdata_borrowed() {
        let src = "<r><![CDATA[raw <data>]]></r>";
        assert_eq!(events(src), vec!["<r>", "CD:raw <data>", "</r>"]);
    }

    #[test]
    fn empty_element_emits_both_events() {
        assert_eq!(events("<root><br/></root>"), vec!["<root>", "<br>", "</br>", "</root>"]);
    }

    #[test]
    fn buffer_reuse() {
        let src = "<a x='1'/><b y='2'/>";
        let src = format!("<root>{src}</root>");
        let mut r = XmlReader::from_str(&src);
        let mut buf: Vec<Attr> = Vec::new();
        let cap_before;
        loop {
            match r.next_into(&mut buf).unwrap() {
                EventInto::StartElement { name } if name == "a" => {
                    cap_before = buf.capacity();
                    break;
                }
                EventInto::Eof => panic!("unexpected EOF"),
                _ => {}
            }
        }
        loop {
            match r.next_into(&mut buf).unwrap() {
                EventInto::StartElement { name } if name == "b" => {
                    assert_eq!(buf.capacity(), cap_before, "capacity should not grow for same-size attrs");
                    break;
                }
                EventInto::Eof => panic!("unexpected EOF"),
                _ => {}
            }
        }
    }

    #[test]
    fn lazy_attrs_iter() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlReader::from_str(src);
        match r.next().unwrap() {
            Event::StartElement(tag) => {
                assert_eq!(tag.name(), "el");
                let pairs: Vec<(String, String)> = tag.attrs()
                    .map(|a| a.map(|a| (a.name.to_owned(), a.value.into_owned())).unwrap())
                    .collect();
                assert_eq!(pairs, vec![("id".into(), "1".into()), ("class".into(), "x".into())]);
            }
            _ => panic!("expected StartElement"),
        }
    }

    #[test]
    fn lazy_attrs_skipped_costs_nothing() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlReader::from_str(src);
        match r.next().unwrap() {
            Event::StartElement(tag) => assert_eq!(tag.name(), "el"),
            _ => panic!(),
        }
        match r.next().unwrap() {
            Event::EndElement(tag) => assert_eq!(tag.name(), "el"),
            _ => panic!(),
        }
        match r.next().unwrap() {
            Event::Eof => {}
            _ => panic!(),
        }
    }

    #[test]
    fn debug_impl_shows_contents() {
        // Custom Debug should print the *content* (lossy-decoded), not
        // raw byte offsets.  Sanity-check each tag flavour.
        let mut r = XmlReader::from_str("<a x='1'>hi<!-- c --><![CDATA[cd]]><?p y?></a>");
        let mut seen: Vec<String> = Vec::new();
        loop {
            match r.next().unwrap() {
                Event::Eof => break,
                ev => seen.push(format!("{ev:?}")),
            }
        }
        let combined = seen.join("\n");
        assert!(combined.contains("StartTag"),  "Debug missing StartTag — {combined}");
        assert!(combined.contains("\"a\""),     "Debug should show element name — {combined}");
        assert!(combined.contains("Comment(\" c \")"), "Debug for Comment — {combined}");
        assert!(combined.contains("CData(\"cd\")"),    "Debug for CData — {combined}");
        assert!(combined.contains("EndTag"),    "Debug missing EndTag — {combined}");
    }

    #[test]
    fn text_method_access() {
        // Text exposes as_str() and into_str().
        let src = "<r>hello</r>";
        let mut r = XmlReader::from_str(src);
        let _ = r.next().unwrap(); // StartElement
        match r.next().unwrap() {
            Event::Text(t) => {
                assert_eq!(t.as_str(), "hello");
                let owned = t.into_str();
                assert_eq!(owned, "hello");
                assert!(matches!(owned, Cow::Borrowed("hello")));
            }
            _ => panic!(),
        }
    }

    // ── Attr accessors ────────────────────────────────────────────────────

    #[test]
    fn attr_name_and_value_accessors() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlReader::from_str(src);
        match r.next().unwrap() {
            Event::StartElement(tag) => {
                let attrs: Vec<_> = tag.attrs().map(|a| a.unwrap()).collect();
                // .name() and .value() methods (vs direct fields).
                assert_eq!(attrs[0].name(),  "id");
                assert_eq!(attrs[0].value(), "1");
                assert_eq!(attrs[1].name(),  "class");
                assert_eq!(attrs[1].value(), "x");
            }
            _ => panic!(),
        }
    }

    // ── from_bytes / from_bytes_unchecked / from_bytes_in_place_unchecked ─

    #[test]
    fn from_bytes_valid_utf8() {
        let src = b"<r/>";
        let mut r = XmlReader::from_bytes(src).expect("valid utf-8");
        match r.next().unwrap() {
            Event::StartElement(tag) => assert_eq!(tag.name(), "r"),
            _ => panic!(),
        }
    }

    #[test]
    fn from_bytes_invalid_utf8_errors() {
        let bad = &[b'<', 0xFF, 0xFE, b'>'];
        assert!(XmlReader::from_bytes(bad).is_err());
    }

    #[test]
    fn from_bytes_unchecked_skips_validation() {
        let src = b"<r/>";
        // SAFETY: the bytes are valid UTF-8.
        let mut r = unsafe { XmlReader::from_bytes_unchecked(src) };
        match r.next().unwrap() {
            Event::StartElement(tag) => assert_eq!(tag.name(), "r"),
            _ => panic!(),
        }
    }

    #[test]
    fn from_bytes_in_place_unchecked() {
        let mut src = b"<r/>".to_vec();
        // SAFETY: bytes are valid UTF-8 and we don't touch `src` until the
        // reader is dropped.
        let mut r = unsafe { XmlReader::from_bytes_in_place_unchecked(&mut src) };
        match r.next().unwrap() {
            Event::StartElement(tag) => assert_eq!(tag.name(), "r"),
            _ => panic!(),
        }
        drop(r);
    }

    // ── with_options / xml_decl / recovered_errors ───────────────────────

    #[test]
    fn with_options_changes_behavior() {
        // With skip_entity_expansion the parser emits raw text without
        // decoding &amp;.  Verify the Text payload is "&amp;" not "&".
        let opts = ParseOptions { skip_entity_expansion: true, ..ParseOptions::default() };
        let mut r = XmlReader::from_str("<r>&amp;</r>").with_options(opts);
        let _ = r.next().unwrap();    // StartElement
        match r.next().unwrap() {
            Event::Text(t) => assert_eq!(t.as_str(), "&amp;"),
            ev => panic!("expected Text, got {ev:?}"),
        }
    }

    #[test]
    fn xml_decl_returns_some_after_reading_prolog() {
        let mut r = XmlReader::from_str(r#"<?xml version="1.0" encoding="UTF-8"?><r/>"#);
        let _ = r.next().unwrap();    // StartElement — prolog has now been read
        let decl = r.xml_decl().expect("xml decl");
        assert_eq!(decl.version, "1.0");
    }

    #[test]
    fn xml_decl_returns_none_when_absent() {
        let mut r = XmlReader::from_str("<r/>");
        let _ = r.next().unwrap();
        assert!(r.xml_decl().is_none());
    }

    #[test]
    fn recovered_errors_empty_for_well_formed_input() {
        let mut r = XmlReader::from_str("<r/>");
        let _ = r.next().unwrap();
        let _ = r.next().unwrap();
        assert!(r.recovered_errors().is_empty());
    }

    // ── EntityRef event surface ──────────────────────────────────────────

    #[test]
    fn entity_ref_event_via_next() {
        // EntityRef events are only emitted for *user-defined* entities
        // and only when resolve_entities = false.  Need a DOCTYPE so
        // 'foo' is a valid declared entity name.  Run through events
        // until we either find an EntityRef or hit EOF.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY foo "bar">]>
<r>&foo;</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let mut r = XmlReader::from_str(src).with_options(opts);
        let mut found = false;
        loop {
            match r.next().unwrap() {
                Event::EntityRef(e) => {
                    assert_eq!(e.name(), "foo");
                    let s = format!("{e:?}");
                    assert!(s.contains("foo"), "got {s}");
                    found = true;
                    break;
                }
                Event::Eof => break,
                _ => continue,
            }
        }
        assert!(found, "EntityRef event was not emitted");
    }

    #[test]
    fn entity_ref_event_via_next_into() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY bar "x">]>
<r>&bar;</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let mut r = XmlReader::from_str(src).with_options(opts);
        let mut buf = Vec::new();
        let mut found = false;
        loop {
            match r.next_into(&mut buf).unwrap() {
                EventInto::EntityRef { name } => {
                    assert_eq!(name, "bar");
                    found = true;
                    break;
                }
                EventInto::Eof => break,
                _ => continue,
            }
        }
        assert!(found, "EntityRef event was not emitted");
    }

    // ── unescape ─────────────────────────────────────────────────────────

    #[test]
    fn unescape_no_amp_returns_borrowed() {
        let s = "no entities here";
        let out = unescape(s);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, "no entities here");
    }

    #[test]
    fn unescape_predefined_entities() {
        assert_eq!(unescape("&amp;").as_ref(),  "&");
        assert_eq!(unescape("&lt;").as_ref(),   "<");
        assert_eq!(unescape("&gt;").as_ref(),   ">");
        assert_eq!(unescape("&quot;").as_ref(), "\"");
        assert_eq!(unescape("&apos;").as_ref(), "'");
    }

    #[test]
    fn unescape_mixed_text() {
        assert_eq!(
            unescape("a &amp; b &lt; c &gt; d").as_ref(),
            "a & b < c > d",
        );
    }

    #[test]
    fn unescape_numeric_decimal_char_ref() {
        assert_eq!(unescape("&#65;").as_ref(),    "A");
        assert_eq!(unescape("&#8364;").as_ref(),  "€");
    }

    #[test]
    fn unescape_numeric_hex_char_ref() {
        assert_eq!(unescape("&#x41;").as_ref(),   "A");
        assert_eq!(unescape("&#xX41;").as_ref(),  "&#xX41;"); // invalid → literal pass-through
        assert_eq!(unescape("&#X41;").as_ref(),   "A");
    }

    #[test]
    fn unescape_invalid_char_ref_passes_through() {
        // Unparseable codepoint → keep the original text.
        assert_eq!(unescape("&#abc;").as_ref(), "&#abc;");
        // Out-of-range codepoint → keep the original text.
        assert_eq!(unescape("&#99999999;").as_ref(), "&#99999999;");
    }

    #[test]
    fn unescape_unknown_named_entity_passes_through() {
        // Unknown named entity → keep the original `&name;`.
        assert_eq!(unescape("&bogus;").as_ref(), "&bogus;");
    }

    #[test]
    fn unescape_ampersand_without_semicolon() {
        // `&` not followed by a `;` within 16 chars → literal '&'.
        assert_eq!(unescape("&just an ampersand").as_ref(), "&just an ampersand");
        // `&...` where `;` is too far away — kept as literal.
        assert_eq!(unescape("&very_long_name_that_exceeds_sixteen_chars_threshold;").as_ref(),
                   "&very_long_name_that_exceeds_sixteen_chars_threshold;");
    }
}
