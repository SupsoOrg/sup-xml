//! `XmlBytesReader` — bytes-typed streaming SAX-style API.
//!
//! The byte-level parsing engine.  Events carry `Cow<'src, [u8]>` payloads
//! sliced directly from the source — no UTF-8 cast at the emission
//! boundary.  [`XmlReader`](crate::reader::XmlReader) is a thin wrapper
//! over this type that adds the `&[u8] → &str` conversion.
//!
//! ## Why two readers exist
//!
//! Some callers prefer raw bytes — byte-literal tag matching
//! (`name == b"item"`), hash/digest pipelines, format conversion, byte
//! forwarding — and would rather not pay for the type system's `&str`
//! guarantee.  `XmlBytesReader` (this module) serves them.  Most callers
//! want validated strings; [`XmlReader`](crate::reader::XmlReader) serves
//! them and is the recommended default.  The two share a single parser;
//! the only difference is the payload type each emits.
//!
//! ## UTF-8 invariant
//!
//! Bytes emitted here are still valid UTF-8 — the [`Scanner`] enforces
//! that at construction time (`from_bytes` validates; `from_bytes_unchecked`
//! documents the caller obligation).  The bytes-typed API simply chooses
//! not to surface that fact through the type system, which is exactly
//! what lets the str-typed wrapper convert via `from_utf8_unchecked` for
//! free.
//!
//! ## When to choose which
//!
//! Reach for `XmlBytesReader` when you want to compare against byte
//! literals, forward bytes to another sink, or feed a hash/digest
//! pipeline.  Reach for [`XmlReader`](crate::reader::XmlReader) when you
//! want `&str` payloads — string formatting, regex, anything that wants
//! Unicode-typed input.  Performance is identical; the choice is purely
//! about the payload type you'd rather work with.

use std::borrow::Cow;
use rustc_hash::{FxHashMap as HashMap, FxHashSet};

use memchr::{memchr, memchr3};

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::options::ParseOptions;
use crate::scanner::{Scanner, is_pubid_char, is_xml_char, is_xml_11_char, validate_xml_chars};

// ── public types ──────────────────────────────────────────────────────────────

/// A single attribute from a start tag, with a zero-copy value when possible.
///
/// The `name` is always borrowed directly from the source — XML name
/// chars can't include entity refs, so no allocation is ever needed.
/// The `value` is `Cow::Borrowed` for the common case (no entity
/// references in the literal) and `Cow::Owned` only when the value
/// contained `&entity;` references that the parser had to expand.
#[derive(Debug)]
pub struct BytesAttr<'src> {
    /// Source-borrowed attribute name (e.g. `b"id"`, `b"xmlns:foo"`).
    pub name:  &'src [u8],
    /// Attribute value.  Borrowed from source when no entity expansion
    /// happened; owned otherwise.
    pub value: Cow<'src, [u8]>,
}

impl<'src> BytesAttr<'src> {
    /// Attribute name as a borrowed source slice.  Same as `self.name`.
    pub fn name(&self) -> &'src [u8] { self.name }

    /// Attribute value, borrowing from the source when no entity
    /// references appeared in the literal.  Same as `&self.value`.
    pub fn value(&self) -> &[u8] { &self.value }
}

/// Lazy iterator over the attributes of a start tag, yielding raw byte
/// payloads — see [`BytesAttr`] for the shape of each item.
///
/// `BytesAttrs` is returned inside [`BytesEvent::StartElement`].  Each
/// `.next()` call parses one `name="value"` pair on demand from a slice
/// of the source.  Drop it without iterating and you pay roughly the
/// cost of recording two byte offsets — a deliberate trade-off, since
/// many streaming consumers care only about element names and skip
/// attribute parsing entirely.
///
/// # When to use this vs [`XmlBytesReader::next_into`]
///
/// * Use the lazy iterator (this) for selective access — say, "give me
///   the `id=` attribute but ignore the rest" — or when you don't read
///   attributes at all on most elements.
/// * Use [`XmlBytesReader::next_into`] for eager access into a
///   reusable buffer when you'll consume *every* attribute on every
///   element anyway; it amortises the iterator setup across calls.
///
/// # Internals
///
/// The iterator owns a sub-`Scanner` over the byte range between the
/// element name and the closing `>` (or `/>`) of the start tag.  That
/// sub-scanner means the parent `XmlBytesReader` can move on to the
/// next event independently of whether the caller iterates the attrs.
/// Internal entity-expansion state (`entities`, `expansion_bytes`) is
/// borrowed mutably from the parent so attribute values containing
/// `&entity;` references expand against the same expansion-byte budget
/// the rest of the document uses.
///
/// # Error semantics
///
/// If an attribute fails to parse, the iterator yields `Some(Err(_))`
/// once and returns `None` on every subsequent call.  A malformed
/// attribute terminates iteration; the caller should bail.  Names and
/// values both validate against [`ParseOptions`](crate::ParseOptions)
/// (the same options that govern the parent reader).
pub struct BytesAttrs<'r, 'src> {
    scan:            Scanner<'src, 'r>,
    entities:        &'r HashMap<String, EntityDecl>,
    expansion_bytes: &'r mut u64,
    done:            bool,
    /// Mirror of [`XmlBytesReader::standalone_yes`] for the standalone
    /// WFC check on entity references inside attribute values.
    standalone_yes:  bool,
    /// Mirror of [`XmlBytesReader::is_xml_11`] for the text-decl
    /// version check in any external entities referenced from
    /// inside an attribute value's expansion.
    is_xml_11:       bool,
}

/// Replacement-text + kind for a single DTD-declared entity.
///
/// XML 1.0 §4.1 distinguishes *internal* entities (whose body is
/// an inline literal at declaration time) from *external* entities
/// (whose body comes from a referenced SYSTEM / PUBLIC resource).
/// The distinction matters in several places:
///
/// * §4.3.1 — only *external* parsed entities may begin with a
///   text declaration (`<?xml … ?>`).  An identical-looking PI in
///   an internal entity's content is not-wf.
/// * §3.3.3 / §4.4.4 — *external* general entity references are
///   forbidden in attribute values.
/// * §4.4.2 — an *external* parameter entity's replacement text
///   is processed as if external markup.
/// * libxml2-compat mode silently expands references to declared
///   but unloaded external entities to empty rather than erroring.
///
/// Tracking the kind on the entity value itself lets every consumer
/// branch on the right rule without consulting a side-table — and
/// the type system guarantees we handle each variant.
#[derive(Debug, Clone)]
pub(crate) enum EntityKind {
    /// `<!ENTITY name "literal text">`.  The string IS the
    /// replacement text, taken verbatim from the EntityValue.
    InternalText(String),
    /// `<!ENTITY name SYSTEM "uri">` (or PUBLIC) whose bytes were
    /// successfully loaded by the resolver / `load_external_dtd`
    /// path.  The string is the transcoded UTF-8 replacement text.
    ExternalLoaded(String),
    /// `<!ENTITY name SYSTEM "uri">` declared but not loaded
    /// (typically: no resolver was configured, or the resolver
    /// refused).  References in `libxml2_compat` mode expand to
    /// empty; references in strict mode raise the usual
    /// "undefined entity" error.
    ExternalUnloaded,
}

impl EntityKind {
    /// The replacement text for this entity, if we have one.
    /// `ExternalUnloaded` returns `None`.
    pub(crate) fn replacement(&self) -> Option<&str> {
        match self {
            EntityKind::InternalText(s) | EntityKind::ExternalLoaded(s) => Some(s.as_str()),
            EntityKind::ExternalUnloaded => None,
        }
    }
    /// `true` for `ExternalLoaded` / `ExternalUnloaded` — i.e.
    /// when the entity's *replacement text* comes from outside
    /// (via the resolver).  Distinct from `declared_external`
    /// on [`EntityDecl`], which tracks where the *declaration*
    /// itself appeared (internal subset vs. external subset).
    pub(crate) fn is_external_value(&self) -> bool {
        matches!(self, EntityKind::ExternalLoaded(_) | EntityKind::ExternalUnloaded)
    }
}

/// An entity declaration: its replacement-text [`EntityKind`] plus
/// the WFC-relevant fact of where the declaration itself appeared.
///
/// XML 1.0 § 4.1 "WFC: Entity Declared" (and § 2.9 the
/// `standalone="yes"` interpretation): in a standalone document,
/// references to entities whose declaration lived in the external
/// subset (or in an external parameter-entity's replacement text)
/// are not well-formed.  Tracking that here lets the reference-time
/// check apply the rule precisely.
#[derive(Debug, Clone)]
pub(crate) struct EntityDecl {
    pub(crate) kind: EntityKind,
    /// `true` when the declaration was read from the external
    /// subset *or* from a PE-replacement text (which is "external"
    /// in the spec's sense).  Initialised at `parse_entity_decl`
    /// time from `Scanner::on_original_source()`.
    pub(crate) declared_external: bool,
    /// Absolute URL the entity's bytes were loaded from, when this
    /// is an external entity that the resolver successfully
    /// returned bytes for.  Used at reference-expansion time to
    /// seed the new entity-stream frame's `base_uri` so nested
    /// SYSTEM identifiers inside this entity's replacement text
    /// can be resolved against the entity's own URL rather than
    /// the document's (XML 1.0 § 4.2.2 + errata E18).  `None` for
    /// internal entities and for externals that failed to load.
    pub(crate) source_uri: Option<String>,
}

impl EntityDecl {
    fn replacement(&self) -> Option<&str> { self.kind.replacement() }
}

/// A declared-but-not-yet-loaded external *general* entity.  XML 1.0
/// § 4.4.3: a parsed external general entity is loaded only when it is
/// referenced — declaring `<!ENTITY e SYSTEM "x">` must not, on its
/// own, trigger any I/O (an unreferenced entity whose target is missing
/// is not an error, and eagerly fetching it would be an XXE/SSRF
/// vector).  We stash the identifiers at declaration time and resolve
/// them through the configured `external_resolver` on first reference.
#[derive(Debug, Clone)]
pub(crate) struct DeferredExternal {
    pub(crate) system_id: String,
    pub(crate) public_id: Option<String>,
}

impl<'src> Iterator for BytesAttrs<'_, 'src> {
    type Item = Result<BytesAttr<'src>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done { return None; }
        // Skip any whitespace between the previous attr and this one.
        self.scan.skip_ws();
        if self.scan.is_eof() {
            self.done = true;
            return None;
        }
        match self.read_attr() {
            Ok(a)  => Some(Ok(a)),
            Err(e) => { self.done = true; Some(Err(e)) }
        }
    }
}

// ── BytesAttrs internals ────────────────────────────────────────────────────
//
// One-attr-at-a-time parsing logic invoked from `Iterator::next` above.
// `read_attr` handles the `name="value"` shape; `scan_att_value_cow` is
// the value-side fast path (memchr3 to find the closing quote, `&`, or
// `<`) with a slow path for entity expansion.
impl<'src> BytesAttrs<'_, 'src> {
    fn read_attr(&mut self) -> Result<BytesAttr<'src>> {
        // Attribute names are always source-borrowed: XML disallows
        // entity refs inside names, so the scanner's name range is
        // guaranteed to be a clean slice into `'src`.
        let (n_start, n_end) = self.scan.scan_name_raw()?;
        let name = match self.scan.current_borrowed_bytes() {
            Some(src) => &src[n_start..n_end],
            None => return Err(self.scan.err(
                "attribute name from inside entity-expansion stream not supported"
            )),
        };
        self.scan.skip_ws();
        self.scan.expect(b'=')?;
        self.scan.skip_ws();
        let value = self.scan_att_value_cow()?;
        Ok(BytesAttr { name, value })
    }

    /// Read one attribute *value* — the part between the matching quotes
    /// after `=`.  Two paths:
    ///
    /// * **Fast path:** the value contains no `&` (no entity refs) — we
    ///   `memchr3` for `"`/`'`/`&`/`<` in one SIMD step, find the closing
    ///   quote, and return a `Cow::Borrowed` slice into the source.
    /// * **Slow path:** an `&` was seen.  Copy the clean prefix into an
    ///   owned buffer, then loop expanding entity refs (with the parent
    ///   reader's expansion-byte budget) until we hit the closing quote.
    ///   Returns `Cow::Owned`.
    ///
    /// `<` inside an attribute value is a hard error (XML 1.0 § 3.1).
    fn scan_att_value_cow(&mut self) -> Result<Cow<'src, [u8]>> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            Some(b) => return Err(self.scan.err(format!("expected quote, got '{}'", b as char))),
            None     => return Err(self.scan.err("expected quote, got EOF")),
        };

        let start = self.scan.cur_pos();

        // SIMD fast path: scan for closing quote, `&`, or `<` simultaneously.
        // memchr3 always finds *something* at the first interesting byte, so
        // this is a single match — no loop needed (each branch terminates).
        let tail = self.scan.cur_tail();
        match memchr3(q, b'&', b'<', tail) {
            None => return Err(self.scan.err("unterminated attribute value")),
            Some(off) => {
                self.scan.cur_advance_pos(off);
                match self.scan.cur_bytes()[self.scan.cur_pos()] {
                    b if b == q => {
                        // §3.3.3 CDATA-default normalization — rewrite
                        // `\t` / `\n` / `\r` (and in XML 1.1 also NEL
                        // and LS) to `#x20`.  Stays zero-copy when the
                        // value carries only literal spaces.
                        let s = self.scan.cur_slice(start, self.scan.cur_pos());
                        let s = maybe_normalize_attr_value(s, self.is_xml_11);
                        self.scan.advance();
                        return Ok(s);
                    }
                    b'<' => return Err(self.scan.err("'<' not allowed in attribute value")),
                    b'&' => {} // fall through to slow path
                    _ => unreachable!(),
                }
            }
        }

        // Slow path: entity / char reference found — copy clean prefix
        // (applying §3.3.3 normalization to those literal source
        // bytes), then expand.  Subsequent literal segments and the
        // expansion outputs are appended separately so that character
        // references can deliver raw `\t` / `\n` / `\r` without being
        // rewritten away.
        let mut buf = Vec::<u8>::new();
        append_attr_segment(&self.scan, start, self.scan.cur_pos(), &mut buf, self.is_xml_11);
        let budget = self.scan.opts.max_entity_expansion_bytes;

        // Attribute values cannot contain `<`, so no element can open inside
        // one — the depth check is trivially satisfied at depth 0.
        let depth: u32 = 0;

        loop {
            let tail = self.scan.cur_tail();
            match memchr3(q, b'&', b'<', tail) {
                None => {
                    // End of current stream.  In an entity-replacement stream,
                    // pop and continue — the value's closing quote must come
                    // from the original stream (XML 1.0 § 4.4.5 — quotes in
                    // replacement text are normal data characters).
                    let end = self.scan.cur_len();
                    let from = self.scan.cur_pos();
                    append_attr_segment(&self.scan, from, end, &mut buf, self.is_xml_11);
                    self.scan.cur_set_pos(end);
                    if self.scan.try_pop_entity_stream() {
                        continue;
                    }
                    return Err(self.scan.err("unterminated attribute value"));
                }
                Some(off) => {
                    let from = self.scan.cur_pos();
                    append_attr_segment(&self.scan, from, from + off, &mut buf, self.is_xml_11);
                    self.scan.cur_advance_pos(off);
                    match self.scan.cur_bytes()[self.scan.cur_pos()] {
                        b if b == q => {
                            // Closing quote only counts when we're back at
                            // the original stream — quotes inside an entity
                            // replacement are literal data per spec.
                            if self.scan.in_entity() {
                                buf.push(b);
                                self.scan.advance();
                            } else {
                                self.scan.advance();
                                break;
                            }
                        }
                        b'<' => return Err(self.scan.err("'<' not allowed in attribute value")),
                        // Pass `None` for the recovery sink: BytesAttrs
                        // doesn't carry a reference to the parent
                        // reader's recovered_errors list (would
                        // require a struct change).  Recovery for
                        // attribute-value entity references would be
                        // a later add-on requiring that struct change.
                        b'&' => expand_reference_bytes(
                            &mut self.scan, &mut buf, self.entities, &mut *self.expansion_bytes, budget, depth, None,
                            // Attribute values don't get the XML 1.0
                            // errata E13 relaxation; only text-content
                            // refs do.  Attr-value refs stay strict-WF.
                            false,
                            self.standalone_yes,
                            self.is_xml_11,
                        )?,
                        _ => unreachable!(),
                    }
                }
            }
        }
        // No wholesale normalization here — literal source segments
        // were already normalized as they were appended via
        // `append_attr_segment`, and the expansion outputs from
        // character references must arrive verbatim.
        Ok(Cow::Owned(buf))
    }
}

// ── tag types ────────────────────────────────────────────────────────────────
//
// Each `BytesEvent` variant wraps one of the structs below.  They carry
// only what's needed to produce their public payload on demand — usually
// a few `u32` offsets into the source buffer.  Discarding an event
// without calling any method does no extraction work; the structs are
// trivially droppable.

/// A start-tag event (`<element ...>` or `<element/>`).
///
/// Carries source offsets only — no name extraction or attribute
/// parsing happens until you call a method.  Drop without calling
/// anything and you've paid nothing.
pub struct BytesStartTag<'r, 'src> {
    src:             &'src [u8],
    name_start:      u32,
    name_end:        u32,
    /// Cold path: start tag parsed inside an entity-replacement
    /// stream.  The `(name_start, name_end)` range would index into
    /// the entity-stream bytes, not `src`, so we capture the name
    /// here instead.  `None` for the common source-borrowed case.
    owned_name:      Option<Box<[u8]>>,
    /// Cold path: when the start tag was parsed inside an
    /// entity-replacement stream, the lazy [`attrs()`] iterator has
    /// no way to surface bytes that don't live in `src`.  We pre-
    /// parse the attribute list eagerly into owned pairs and stash
    /// them here; consumers that need entity-stream attrs (the DOM
    /// builder, for instance) read this first.  `None` for the
    /// common source-borrowed case.
    entity_attrs:    Option<Vec<(Vec<u8>, Vec<u8>)>>,
    attrs_start:     u32,
    attrs_end:       u32,
    entities:        &'r HashMap<String, EntityDecl>,
    expansion_bytes: &'r mut u64,
    /// Borrowed (not copied) — `ParseOptions` is ~32 bytes; for
    /// elements where `attrs()` is never called we'd otherwise pay the
    /// copy for nothing.  At `attrs()` time we deref + copy into the
    /// child Scanner.
    opts:            &'r ParseOptions,
    /// Mirror of [`XmlBytesReader::standalone_yes`] for the standalone
    /// WFC check on entity references inside attribute values.
    standalone_yes:  bool,
    /// Mirror of [`XmlBytesReader::is_xml_11`] threaded through to
    /// any text-decl parsing during attribute-entity expansion.
    is_xml_11:       bool,
}

impl<'r, 'src> BytesStartTag<'r, 'src> {
    /// Element name as bytes.  Borrowed from the source slice on the
    /// common path; tied to `&self` on the cold path where the start
    /// tag was read from inside an entity-replacement stream and the
    /// name had to be captured separately.  Use [`name_cow`] when you
    /// need a lifetime that outlives `self`.
    ///
    /// [`name_cow`]: BytesStartTag::name_cow
    #[inline]
    pub fn name(&self) -> &[u8] {
        match &self.owned_name {
            Some(b) => b,
            None    => &self.src[self.name_start as usize..self.name_end as usize],
        }
    }

    /// Element name with the `'src` lifetime preserved when possible.
    /// Source-borrowed names round-trip without copying; an
    /// entity-stream name is returned as `Cow::Owned` (heap copy).
    pub fn name_cow(&self) -> Cow<'src, [u8]> {
        match &self.owned_name {
            Some(b) => Cow::Owned(b.to_vec()),
            None    => Cow::Borrowed(
                &self.src[self.name_start as usize..self.name_end as usize]
            ),
        }
    }

    /// Byte offset of this element's name within the source buffer.
    /// Useful for line-number computation at parser-side
    /// (translate via `compute_line_col` only when the consumer
    /// actually asks).
    ///
    /// For start tags read from inside an entity-replacement stream,
    /// the source-offset is meaningless and returns `0`; check
    /// [`name_cow`](Self::name_cow) returning `Cow::Owned` to detect
    /// that case.
    #[inline]
    pub fn name_offset(&self) -> u32 {
        self.name_start
    }

    /// Raw byte range of the attrs region (between the name and the
    /// closing `>` / `/>`).  Useful for callers that want to do their
    /// own scanning rather than going through the iterator.
    #[inline]
    pub fn attrs_bytes(&self) -> &'src [u8] {
        &self.src[self.attrs_start as usize..self.attrs_end as usize]
    }

    /// Pre-parsed attribute pairs from an entity-replacement stream,
    /// or `None` for the common source-borrowed case.  When `Some`,
    /// callers should consume this list instead of [`attrs()`] — the
    /// lazy iterator can't surface attrs whose bytes don't live in
    /// `src`.
    #[inline]
    pub fn entity_attrs(&self) -> Option<&[(Vec<u8>, Vec<u8>)]> {
        self.entity_attrs.as_deref()
    }

    /// Iterate the attributes.  Consumes the tag — once you ask for
    /// attrs you've committed.  No-attr elements do effectively zero
    /// work here (the iterator's first `.next()` short-circuits on EOF).
    pub fn attrs(self) -> BytesAttrs<'r, 'src> {
        BytesAttrs {
            // Borrow the parent reader's options — no clone on the hot path.
            // The inner Scanner's `'opt` lifetime is `'r`, the same lifetime
            // the StartTag carries; both end when this BytesAttrs drops.
            scan:            Scanner::new(self.attrs_bytes(), Cow::Borrowed(self.opts)),
            entities:        self.entities,
            expansion_bytes: self.expansion_bytes,
            done:            false,
            standalone_yes:  self.standalone_yes,
            is_xml_11:       self.is_xml_11,
        }
    }
}

impl std::fmt::Debug for BytesStartTag<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesStartTag")
            .field("name",  &String::from_utf8_lossy(self.name()))
            .field("attrs", &String::from_utf8_lossy(self.attrs_bytes()))
            .finish()
    }
}

/// An end-tag event (`</element>` — or the synthetic close emitted
/// after every self-closing `<element/>`).  Holds the matched tag's
/// name as a source-borrowed slice.
pub struct BytesEndTag<'src> {
    src:         &'src [u8],
    name_start:  u32,
    name_end:    u32,
}

impl<'src> BytesEndTag<'src> {
    #[inline]
    pub fn name(&self) -> &'src [u8] {
        &self.src[self.name_start as usize..self.name_end as usize]
    }
}

impl std::fmt::Debug for BytesEndTag<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesEndTag")
            .field("name", &String::from_utf8_lossy(self.name()))
            .finish()
    }
}

/// Character-data text between elements.  `Cow::Borrowed` for the
/// common case (no entity refs in the literal); `Cow::Owned` only when
/// the parser had to expand `&entity;` references.
pub struct BytesText<'src> { inner: Cow<'src, [u8]> }
impl<'src> BytesText<'src> {
    #[inline] pub fn as_bytes(&self) -> &[u8] { &self.inner }
    #[inline] pub fn into_bytes(self) -> Cow<'src, [u8]> { self.inner }
}
impl std::fmt::Debug for BytesText<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BytesText({:?})", String::from_utf8_lossy(&self.inner))
    }
}

/// A `<![CDATA[…]]>` section.  Always source-borrowed in practice
/// (entity references inside CDATA aren't expanded per spec).
pub struct BytesCData<'src> { inner: Cow<'src, [u8]> }
impl<'src> BytesCData<'src> {
    #[inline] pub fn as_bytes(&self) -> &[u8] { &self.inner }
    #[inline] pub fn into_bytes(self) -> Cow<'src, [u8]> { self.inner }
}
impl std::fmt::Debug for BytesCData<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BytesCData({:?})", String::from_utf8_lossy(&self.inner))
    }
}

/// An XML comment (`<!-- ... -->`).  The payload is the text strictly
/// between the delimiters.
pub struct BytesComment<'src> { inner: Cow<'src, [u8]> }
impl<'src> BytesComment<'src> {
    #[inline] pub fn as_bytes(&self) -> &[u8] { &self.inner }
    #[inline] pub fn into_bytes(self) -> Cow<'src, [u8]> { self.inner }
}
impl std::fmt::Debug for BytesComment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BytesComment({:?})", String::from_utf8_lossy(&self.inner))
    }
}

/// A processing instruction (`<?target content?>`).
pub struct BytesPi<'src> {
    target_:  Cow<'src, [u8]>,
    content_: Cow<'src, [u8]>,
}
impl<'src> BytesPi<'src> {
    #[inline] pub fn target(&self)  -> &[u8] { &self.target_ }
    #[inline] pub fn content(&self) -> &[u8] { &self.content_ }
    #[inline] pub fn into_parts(self) -> (Cow<'src, [u8]>, Cow<'src, [u8]>) {
        (self.target_, self.content_)
    }
}
impl std::fmt::Debug for BytesPi<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesPi")
            .field("target",  &String::from_utf8_lossy(&self.target_))
            .field("content", &String::from_utf8_lossy(&self.content_))
            .finish()
    }
}

/// A streaming XML event with **lazy** access to its payload.
///
/// Returned by [`XmlBytesReader::next`].  Each variant wraps a tag
/// struct whose methods (`name()`, `attrs()`, etc.) extract data on
/// demand — discard the event without calling a method and you pay
/// almost nothing.  For an eager API that fills a caller-owned buffer
/// see [`XmlBytesReader::next_into`] which returns [`BytesEventInto`].
#[derive(Debug)]
pub enum BytesEvent<'r, 'src> {
    /// An opening (or empty-element) start tag.
    StartElement(BytesStartTag<'r, 'src>),
    /// A closing tag.  Emitted once for each `StartElement`, including
    /// for empty elements (`<br/>` emits `StartElement` then `EndElement`).
    EndElement(BytesEndTag<'src>),
    /// Character data between tags.
    Text(BytesText<'src>),
    /// A `<![CDATA[…]]>` section.
    CData(BytesCData<'src>),
    /// An XML comment.
    Comment(BytesComment<'src>),
    /// A processing instruction.
    Pi(BytesPi<'src>),
    /// An unresolved entity reference — `&foo;` left literal in the
    /// event stream.  Emitted only when
    /// [`ParseOptions::resolve_entities`] is `false` and the
    /// reference is a *user-declared* entity (predefined `&amp;`
    /// etc. and numeric `&#NN;` refs are always expanded into
    /// `Text` payloads).  Carries the entity name (without the
    /// leading `&` / trailing `;`).
    EntityRef(BytesEntityRef<'src>),
    /// The document has been fully consumed.
    Eof,
}

/// `BytesEvent::EntityRef` payload — the entity name only.  The
/// literal source form `&{name};` is reconstructable by callers.
pub struct BytesEntityRef<'src> {
    src:        &'src [u8],
    name_start: u32,
    name_end:   u32,
}

impl<'src> BytesEntityRef<'src> {
    /// Entity name as a borrowed source slice (e.g. `b"foo"` for
    /// the reference `&foo;`).
    #[inline]
    pub fn name(&self) -> &'src [u8] {
        &self.src[self.name_start as usize..self.name_end as usize]
    }
}

impl std::fmt::Debug for BytesEntityRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BytesEntityRef")
            .field("name", &String::from_utf8_lossy(self.name()))
            .finish()
    }
}

/// A streaming XML event with **attributes already parsed into a caller-owned
/// buffer**.
///
/// Returned by [`XmlBytesReader::next_into`].  The buffer the caller passes is
/// cleared on each call and filled with the start tag's attributes, allowing
/// buffer reuse across many events.  For lazy attribute access without a
/// buffer see [`XmlBytesReader::next`] which returns [`BytesEvent`].
#[derive(Debug)]
pub enum BytesEventInto<'src> {
    /// An opening (or empty-element) start tag.  Attributes are in the
    /// caller-owned buffer passed to [`XmlBytesReader::next_into`].
    StartElement { name: Cow<'src, [u8]> },
    /// A closing tag.  Emitted once for each `StartElement`, including for
    /// empty elements.
    EndElement { name: Cow<'src, [u8]> },
    Text(Cow<'src, [u8]>),
    CData(Cow<'src, [u8]>),
    Comment(Cow<'src, [u8]>),
    Pi { target: Cow<'src, [u8]>, content: Cow<'src, [u8]> },
    /// An unresolved entity reference — emitted only when
    /// [`ParseOptions::resolve_entities`] is `false`.
    EntityRef { name: Cow<'src, [u8]> },
    Eof,
}

// ── reader ────────────────────────────────────────────────────────────────────

/// Inter-call state of the reader's `next()` dispatcher.
///
/// Three states folded into one byte so the steady-state path is a
/// single `match` discriminant test instead of two booleans (`Option`
/// niche check + `prolog_done`).  The vast majority of `next()` calls
/// land in `Steady`; the other arms self-transition back to `Steady`
/// after handling their one-shot work.
enum NextState {
    /// Prolog has not been parsed yet.  Set on construction; flipped
    /// to `Steady` after the first `next()` call parses the prolog.
    NeedsProlog,
    /// Steady-state — the common case.  Read the next event off the
    /// scanner directly; no extra bookkeeping.
    Steady,
    /// An empty element (`<foo/>`) was just emitted as `StartElement`;
    /// the next `next()` call must synthesise the matching `EndElement`
    /// from the stored name range.  Transitions back to `Steady` once
    /// the synthetic close fires.
    PendingEnd(u32, u32),
    /// `resolve_entities=false` saw a user-defined `&foo;` inside
    /// text content.  The current call emits accumulated `Text`
    /// (possibly empty); this state carries the entity-name source
    /// offsets so the next call can emit the `EntityRef` event
    /// without re-scanning.
    PendingEntityRef(u32, u32),
}

/// Captured XML declaration fields (`<?xml version="…" encoding="…"
/// standalone="…"?>`).  Populated by [`XmlBytesReader`] during prolog
/// parsing; available via [`XmlBytesReader::xml_decl`] after the first
/// event has been read.
///
/// `encoding` is `None` when the declaration omitted it (XML 1.0 § 2.8
/// allows that — the file is then implicitly UTF-8 or UTF-16 as detected
/// from the BOM).  `standalone` is `None` when the declaration omitted
/// `standalone=…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlDeclInfo {
    pub version:    String,
    pub encoding:   Option<String>,
    pub standalone: Option<bool>,
}

/// One stack frame in [`XmlBytesReader::element_stack`].  See its
/// field doc for why this exists in two flavours.
enum ElementStackEntry {
    /// Hot path — start tag read from the original source.  The
    /// `(start, end)` half-open range indexes into `scan.src_bytes()`
    /// to recover the element name.  Zero allocation.
    SourceRange(u32, u32),
    /// Cold path — start tag read from inside an entity-replacement
    /// stream.  The entity bytes only exist while that frame is
    /// active on the scanner, and even then a source-offset wouldn't
    /// land on the right bytes; we own the name outright so end-tag
    /// matching and error messages work regardless of which stream
    /// owns the original bytes.
    Owned(String),
}

impl ElementStackEntry {
    /// Borrow the element-name bytes, indexing into `src` only when
    /// this entry is a `SourceRange`.
    fn name_bytes<'a>(&'a self, src: &'a [u8]) -> &'a [u8] {
        match self {
            ElementStackEntry::SourceRange(s, e) => &src[*s as usize..*e as usize],
            ElementStackEntry::Owned(s)          => s.as_bytes(),
        }
    }
}

pub struct XmlBytesReader<'src> {
    scan:            Scanner<'src, 'static>,
    /// General entities (`<!ENTITY name …>`) declared in the DTD.
    /// The value carries both the kind (internal / external) and
    /// any replacement text we have — see [`EntityKind`].  The
    /// kind is consulted by `&name;` expansion, by the text-decl
    /// skip on entity push, and by §4.4.4's
    /// "no external entity in attribute values" check.
    entities:        HashMap<String, EntityDecl>,
    /// Parameter entities (`<!ENTITY % name …>`) declared in the
    /// DTD.  Same kind/value layout as `entities`; lives in a
    /// separate map because PE references (`%name;`) and general
    /// references (`&name;`) are disjoint namespaces in
    /// XML 1.0 § 4.4.  On `%name;` expansion in the internal
    /// subset we push the replacement text as an entity stream
    /// and let the decl / PI / comment parsers run against the
    /// expanded bytes — naturally catching e.g. a PE that
    /// expands to `<?xml ...?>` in the wrong context
    /// (XML 1.0 § 2.8 [28a]).
    parameter_entities: HashMap<String, EntityDecl>,
    /// External *general* entities declared with a resolver configured
    /// but not yet loaded.  Populated at declaration time and drained
    /// by `load_deferred_entity` on first `&name;` reference, so an
    /// unreferenced external entity never triggers I/O (XML 1.0
    /// § 4.4.3; see [`DeferredExternal`]).
    deferred_general_entities: HashMap<String, DeferredExternal>,
    /// XML 1.0 errata E13: when the internal DTD subset contains at
    /// least one parameter-entity reference (`%pe;`), an undeclared
    /// *general* entity reference becomes a validity error rather
    /// than a well-formedness error — the PE could in principle have
    /// declared it.  Tracked here so `expand_reference_bytes` can
    /// tolerate the missing decl when this flag is set.
    pe_ref_in_internal_subset_seen: bool,
    depth:           u32,
    expansion_bytes: u64,
    /// Tracking stack for end-tag matching.  Hot path stores a byte
    /// range into the original source — zero allocation per start
    /// tag.  Cold path (start tag read from inside an entity-stream
    /// replacement text) stores the name as an owned String, since
    /// the entity bytes only exist while that frame is active and a
    /// source-offset wouldn't resolve to the right bytes anyway.
    element_stack:   Vec<ElementStackEntry>,
    /// `scan.stream_depth()` captured at each start tag.  At end-tag time
    /// the depth must match — XML 1.0 § 4.3.2 forbids start/end tag pairs
    /// from straddling an entity boundary.  Parallel to `element_stack`.
    element_streams: Vec<usize>,
    /// Whether each open element has had at least one child *element*
    /// (as opposed to only text / nothing).  Parallel to `element_stack`.
    /// Used by `skip_inter_element_whitespace` (libxml2 `remove_blank_text`)
    /// to keep a leaf element's sole whitespace content while dropping
    /// whitespace that sits between element siblings.
    frame_saw_child: Vec<bool>,
    /// Folded inter-call state — see [`NextState`].  Replaces the
    /// previous `pending_end: Option<(u32, u32)>` + `prolog_done: bool`
    /// pair so the steady-state hot path checks one tag, not two.
    state:           NextState,
    /// `true` once the document's root element has started.  XML 1.0
    /// § 2.1 [document]: the document body has *exactly one* root
    /// element, and only Misc (comment / PI / whitespace) is allowed
    /// at the document level outside it.  We track this so we can
    /// reject text events at depth 0, reject a second start tag at
    /// depth 0 after the root closes, and reject EOF while still
    /// nested.  Disabled when `skip_end_tag_check: true` (caller
    /// has opted out of structural well-formedness).
    root_seen:       bool,
    /// Source byte offset of the most recently emitted StartElement's
    /// `<` character.  Updated inside `dispatch_start_element` just
    /// before constructing the [`BytesStartTag`].  Returned by
    /// [`last_start_offset`](Self::last_start_offset).
    ///
    /// Used by higher-level validators (XSD) that need the offset of
    /// the element they just saw — the scanner's `src_offset()` is
    /// past the start tag's `>` by then, which puts diagnostics on
    /// the wrong line for multi-line start tags or root elements
    /// preceded by whitespace.
    ///
    /// `u32::MAX` before the first start tag is emitted, and for
    /// start tags read from inside an entity-replacement stream
    /// (where the source offset is meaningless).
    last_start_offset: u32,
    /// Non-fatal errors logged while
    /// `ParseOptions::recovery_mode` is enabled.  Empty in
    /// strict mode (errors are returned via `Err` instead).  See
    /// [`recovered_errors`](Self::recovered_errors).
    recovered_errors: Vec<XmlError>,
    /// XML declaration fields captured during prolog parsing.  `None`
    /// when the document had no `<?xml ... ?>` declaration; `Some`
    /// after it has been parsed.  Surfaced via
    /// [`xml_decl`](Self::xml_decl).
    xml_decl: Option<XmlDeclInfo>,
    /// Mirror of `xml_decl.standalone == Some(true)` cached for cheap
    /// access from the reference-expansion hot path.  XML 1.0 § 2.9 /
    /// § 4.1 WFC: Entity Declared — in a standalone document, refs to
    /// entities declared in the external subset are not-WF; this bool
    /// gates that check without re-reading `xml_decl` per reference.
    standalone_yes: bool,
    /// True when the document declared `version="1.1"`.  Cached for
    /// the character-class and line-ending hot paths so they can pick
    /// the right table without paying an Option<String> compare per
    /// byte.  Stays false for unspecified versions (XML 1.0 default).
    is_xml_11: bool,
    /// One-shot source-wide pre-scan result: `true` iff the original
    /// source bytes contain at least one byte that participates in
    /// §2.11 EOL normalization — a `\r`, a UTF-8 NEL (`0xC2 0x85`),
    /// or a UTF-8 LS (`0xE2 0x80 0xA8`).  Computed once at
    /// construction with a SIMD memchr3 sweep + a multi-byte
    /// lookahead on each candidate.
    ///
    /// When `false`, the per-text-event EOL scan is skipped
    /// entirely (a typical UTF-8 LF-only document hits this fast
    /// path).  When `true`, the per-segment scan still runs to
    /// locate the rewrite positions.
    source_has_eol_candidate: bool,
    /// Reusable scratch buffer for the text-event slow path (entity
    /// expansion, `]]>` rejection, etc.).  Cleared at the start of
    /// each slow-path entry so allocation cost amortises across all
    /// text chunks in the document instead of paying a fresh
    /// `Vec::new` + grow per chunk.
    ///
    /// Lives on the reader so its capacity grows monotonically to the
    /// largest single text chunk seen during parsing — typical
    /// documents end up with one allocation total across the entire
    /// parse.
    text_decode_buf: Vec<u8>,
    /// DTD declarations captured from the internal subset.  Populated
    /// by `parse_element_decl` / `parse_attlist_decl` as the prolog
    /// is consumed.  Empty when the document has no doctype, or its
    /// doctype contains no element/attlist decls.  Surfaced via
    /// [`take_dtd`](Self::take_dtd) once parsing finishes so that
    /// downstream validators can read it.
    dtd: crate::dtd::Dtd,
    /// Running count of document-level comments/PIs emitted in the
    /// prolog (`depth == 0`, before the root element opened).
    /// Snapshotted into `dtd.internal_subset_prolog_index` when the
    /// `<!DOCTYPE …>` is parsed, so the compat layer can place the
    /// internal-subset node at its true position among prolog siblings.
    prolog_misc_count: u32,
}

/// Maximum `(…)` nesting accepted in a DTD element content model
/// (XML 1.0 § 3.2 [47]).  The model is parsed by mutual recursion
/// (`parse_content_model_inner` ⇄ `parse_cp`), so a pathologically
/// nested declaration — `<!ELEMENT e (((((…)))))>` from an untrusted
/// internal or external subset — would otherwise overflow the call
/// stack.  256 levels sits far above any real grammar.
const MAX_CONTENT_MODEL_DEPTH: u32 = 256;

impl<'src> XmlBytesReader<'src> {
    /// Create a reader from a string slice.  The string must remain alive for
    /// the lifetime of the reader and all events it produces.
    #[allow(clippy::should_implement_trait)] // intentional: mirrors `FromStr` but `XmlBytesReader<'src>` borrows
    pub fn from_str(input: &'src str) -> Self {
        Self::new(input.as_bytes())
    }

    /// Create a reader from a byte slice.  Returns an error if the bytes are
    /// not valid UTF-8.
    pub fn from_bytes(src: &'src [u8]) -> Result<Self> {
        std::str::from_utf8(src).map_err(|e| {
            // `valid_up_to` is the offset of the first ill-formed
            // byte; attach it so callers get the same location info
            // they get from any other parse failure.
            let off = e.valid_up_to();
            let (line, col) = crate::scanner::compute_line_col(src, off);
            XmlError::new(ErrorDomain::Encoding, ErrorLevel::Fatal, format!("invalid UTF-8: {e}"))
                .at("<input>", line, col, off as u64)
        })?;
        Ok(Self::new(src))
    }

    /// Create a reader from a byte slice, **skipping** the upfront UTF-8
    /// validation that [`from_bytes`](Self::from_bytes) performs.
    ///
    /// # Why this is faster
    ///
    /// [`from_bytes`](Self::from_bytes) runs a single O(n)
    /// `std::str::from_utf8` over the entire input before any events are
    /// produced.  On large documents that pass is measurable.  This entry
    /// point removes it.
    ///
    /// # Just-in-time validation by the caller
    ///
    /// This constructor does not validate later either — the safety contract
    /// is that the bytes already are UTF-8 when you call it.  The contract
    /// lets the caller validate *however and whenever they want*, including
    /// not at all when the encoding is already guaranteed by the upstream
    /// source.  Common patterns:
    ///
    /// **Already a `&str`** — UTF-8 is guaranteed by Rust's type system:
    ///
    /// ```no_run
    /// # use sup_xml_core::XmlBytesReader;
    /// let xml: &str = "<r/>";
    /// let reader = unsafe { XmlBytesReader::from_bytes_unchecked(xml.as_bytes()) };
    /// ```
    ///
    /// **Validate up front yourself** — useful when you want a custom error
    /// type or want to avoid duplicate work later:
    ///
    /// ```no_run
    /// # use sup_xml_core::XmlBytesReader;
    /// let bytes: &[u8] = b"<r/>";
    /// std::str::from_utf8(bytes).expect("input must be UTF-8");
    /// let reader = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) };
    /// ```
    ///
    /// **Validate each chunk as it streams in** — the per-chunk passes total
    /// the same O(n) work as one big pass, but you can interleave validation
    /// with I/O instead of paying it all at the end:
    ///
    /// ```no_run
    /// # use sup_xml_core::XmlBytesReader;
    /// # fn next_chunk() -> Option<Vec<u8>> { None }
    /// let mut buf = Vec::new();
    /// while let Some(chunk) = next_chunk() {
    ///     std::str::from_utf8(&chunk).expect("chunk must be UTF-8");
    ///     buf.extend_from_slice(&chunk);
    /// }
    /// let reader = unsafe { XmlBytesReader::from_bytes_unchecked(&buf) };
    /// ```
    ///
    /// In all of these the upfront pass inside
    /// [`from_bytes`](Self::from_bytes) is duplicated work, and this
    /// constructor lets you elide it.
    ///
    /// # Safety
    ///
    /// `src` must be valid UTF-8.  Passing non-UTF-8 bytes is **undefined
    /// behaviour** — the reader hands out `&str` slices into the input that
    /// the rest of the program will treat as UTF-8.
    pub unsafe fn from_bytes_unchecked(src: &'src [u8]) -> Self {
        Self::new(src)
    }

    /// Construct a reader in **destructive in-place mode**.  The
    /// resulting reader is permitted to mutate `src` during parsing
    /// (entity decoding, normalization) — the caller transfers
    /// exclusive write access for the reader's lifetime.  Use this
    /// alongside `parse_bytes_in_place` in [`crate::parser`].
    ///
    /// # Safety
    ///
    /// `src` must be valid UTF-8.  Same contract as
    /// [`from_bytes_unchecked`](Self::from_bytes_unchecked).
    pub unsafe fn from_bytes_in_place_unchecked(src: &'src mut [u8]) -> Self {
        let source_has_eol_candidate = precompute_source_has_eol(src);
        Self {
            scan: Scanner::new_in_place(src, Cow::Owned(ParseOptions::default())),
            entities: HashMap::default(),
            parameter_entities: HashMap::default(),
            deferred_general_entities: HashMap::default(),
            pe_ref_in_internal_subset_seen: false,
            depth: 0,
            expansion_bytes: 0,
            element_stack: Vec::new(),
            element_streams: Vec::new(),
            frame_saw_child: Vec::new(),
            state: NextState::NeedsProlog,
            root_seen: false,
            last_start_offset: u32::MAX,
            recovered_errors: Vec::new(),
            xml_decl: None,
            standalone_yes: false,
            is_xml_11: false,
            source_has_eol_candidate,
            text_decode_buf: Vec::new(),
            dtd: crate::dtd::Dtd::new(),
            prolog_misc_count: 0,
        }
    }

    pub fn with_options(mut self, opts: ParseOptions) -> Self {
        self.scan.opts = Cow::Owned(opts);
        self
    }

    /// Re-point the inner scanner's source view at a fresh buffer
    /// location.  Used by the streaming reader wrapper after it has
    /// refilled / compacted / grown its rolling buffer.
    ///
    /// # Safety
    ///
    /// All of [`crate::scanner::Scanner::rebind`]'s safety contract
    /// applies, plus: this reader's `'src` lifetime parameter is a
    /// lie when the caller is the streaming wrapper (the wrapper
    /// constructs the reader with `'src = 'static` and rebinds the
    /// scanner to bytes that live in its own `Vec<u8>`).  The
    /// reader must not outlive the buffer it points into, and the
    /// caller must call this whenever the buffer might have moved.
    #[inline]
    pub(crate) unsafe fn rebind_scanner(&mut self, ptr: *const u8, len: usize, cur_pos: usize) {
        // SAFETY: forwarded; see `Scanner::rebind` and the docstring above.
        unsafe { self.scan.rebind(ptr, len, cur_pos) }
    }

    fn new(src: &'src [u8]) -> Self {
        let source_has_eol_candidate = precompute_source_has_eol(src);
        Self {
            scan: Scanner::new(src, Cow::Owned(ParseOptions::default())),
            entities: HashMap::default(),
            parameter_entities: HashMap::default(),
            deferred_general_entities: HashMap::default(),
            pe_ref_in_internal_subset_seen: false,
            depth: 0,
            expansion_bytes: 0,
            element_stack: Vec::new(),
            element_streams: Vec::new(),
            frame_saw_child: Vec::new(),
            state: NextState::NeedsProlog,
            root_seen: false,
            last_start_offset: u32::MAX,
            recovered_errors: Vec::new(),
            xml_decl: None,
            standalone_yes: false,
            is_xml_11: false,
            source_has_eol_candidate,
            text_decode_buf: Vec::new(),
            dtd: crate::dtd::Dtd::new(),
            prolog_misc_count: 0,
        }
    }

    /// XML declaration fields parsed from the prolog.  Returns `None`
    /// before the first event has been read, or if the document has no
    /// `<?xml ... ?>` declaration.  Calling this after at least one
    /// successful `next()` (or `read_event`) call is guaranteed to
    /// reflect the document's actual declaration state.
    pub fn xml_decl(&self) -> Option<&XmlDeclInfo> {
        self.xml_decl.as_ref()
    }

    /// Borrow the DTD declarations captured from the internal subset.
    ///
    /// Empty when the document had no `<!DOCTYPE … [ … ]>`, or had a
    /// doctype with no `<!ELEMENT>`/`<!ATTLIST>` declarations.  The
    /// returned [`crate::dtd::Dtd`] feeds
    /// [`crate::dtd::validate`].
    pub fn dtd(&self) -> &crate::dtd::Dtd {
        &self.dtd
    }

    /// The original source bytes the reader is parsing.  Used by
    /// callers that need byte-offset → line/column translation for
    /// diagnostics or for stamping `Node::line` at element creation.
    #[inline]
    pub fn src_bytes(&self) -> &'src [u8] {
        self.scan.src_bytes()
    }

    /// Current byte offset into the original source.  Pairs with
    /// [`src_bytes`](Self::src_bytes) and
    /// [`crate::scanner::compute_line_col`] for on-demand line/column
    /// translation by higher-level validators (XSD, custom checkers).
    /// Inside an entity-replacement stream this returns the position
    /// of the entity reference in the user-visible document — see
    /// [`crate::scanner::Scanner::src_offset`].
    #[inline]
    pub fn src_offset(&self) -> usize {
        self.scan.src_offset()
    }

    /// Source byte offset of the most recently emitted StartElement's
    /// `<` character.  `None` before the first start tag, or for start
    /// tags read from inside an entity-replacement stream (where source
    /// offsets are meaningless).
    ///
    /// Validators snapshot this immediately after `next()` /
    /// `next_into()` returns a StartElement to anchor downstream
    /// diagnostics at the right source position — `src_offset()` by
    /// that point has advanced past the start tag's closing `>`,
    /// which puts errors on the wrong line for multi-line start tags
    /// or root elements preceded by whitespace.
    #[inline]
    pub fn last_start_offset(&self) -> Option<usize> {
        match self.last_start_offset {
            u32::MAX => None,
            v        => Some(v as usize),
        }
    }

    /// Consume `self`'s DTD, leaving an empty one behind.  Used by
    /// `parser.rs::parse_bytes` to hand ownership over to the
    /// resulting [`Document`].
    pub fn take_dtd(&mut self) -> crate::dtd::Dtd {
        std::mem::take(&mut self.dtd)
    }

    /// Errors logged while `ParseOptions::recovery_mode` was
    /// enabled.  Empty in strict mode (the default) — errors there
    /// surface as `Err` from `next()` and abort the parse.
    ///
    /// In recover mode this list grows as the parser encounters
    /// non-fatal well-formedness violations and applies heuristic
    /// repair to keep going.  Inspect after parsing finishes to
    /// learn what the document had wrong; or poll periodically
    /// during streaming.
    ///
    /// Order is the order errors were encountered.  Each entry's
    /// `level` is `ErrorLevel::Error` (recoverable) or
    /// `ErrorLevel::Warning` (informational).  Fatal errors are
    /// never logged here — they always come back through `Err`.
    pub fn recovered_errors(&self) -> &[XmlError] {
        &self.recovered_errors
    }

    /// Decide whether to recover from a non-fatal error or surface
    /// it to the caller, based on `ParseOptions::recovery_mode`
    /// and the error's severity.
    ///
    /// - `Fatal` errors always return `Err`, regardless of the flag.
    /// - In recover mode, `Error` and `Warning` errors are pushed
    ///   to [`recovered_errors`](Self::recovered_errors) and the
    ///   caller continues parsing.
    /// - In strict mode, all errors return `Err`.
    #[inline]
    pub(crate) fn maybe_recover(&mut self, err: XmlError) -> Result<()> {
        if err.level == ErrorLevel::Fatal || !self.scan.opts.recovery_mode {
            return Err(err);
        }
        self.recovered_errors.push(err);
        Ok(())
    }

    /// Synthesise an `EndElement` event for the topmost still-open
    /// element.  Used by recovery for "unclosed element at EOF" and
    /// "mismatched end tag" — closes the inferred element so the
    /// caller's event stream reaches a consistent state.
    ///
    /// The synthetic close uses the start tag's name range so the
    /// event looks the same as a real close to consumers.  When
    /// `skip_end_tag_check: true` (no element_stack maintained) we
    /// can't recover the name, so we synthesise an `EndElement`
    /// with an empty name range — the caller can still see depth
    /// going to 0 even if the name is uninformative.
    fn synthesize_close(&mut self) -> BytesEvent<'_, 'src> {
        // Resolve to a source byte range when possible; for an
        // entity-stream-owned name, BytesEndTag can't borrow the
        // owned String, so return an empty range — synthesize_close
        // is only invoked from recovery paths where the consumer
        // mostly cares about depth bookkeeping, not the precise
        // name bytes.
        let (name_start, name_end) = match self.element_stack.pop() {
            Some(ElementStackEntry::SourceRange(s, e)) => (s, e),
            Some(ElementStackEntry::Owned(_)) | None   => (0, 0),
        };
        self.element_streams.pop();
        self.frame_saw_child.pop();
        self.depth = self.depth.saturating_sub(1);
        let src = self.scan.src_bytes();
        BytesEvent::EndElement(BytesEndTag { src, name_start, name_end })
    }

    // ── public API ────────────────────────────────────────────────────────────

    /// Read the next event with **lazy** attribute access.
    ///
    /// Returns an [`BytesEvent`] borrowing the reader for its lifetime.  Start tag
    /// events carry an [`BytesAttrs`] iterator — iterate it to read attributes,
    /// ignore it to skip attribute parsing entirely.
    ///
    /// For an eager API that fills a caller-owned buffer with parsed
    /// attributes, see [`next_into`](Self::next_into).
    #[allow(clippy::should_implement_trait)] // can't impl `Iterator`: events borrow the reader
    pub fn next(&mut self) -> Result<BytesEvent<'_, 'src>> {
        // Steady-state fast path is the first match arm.  The branch
        // predictor sees the same arm on >99% of calls in any non-
        // pathological document; the cold arms self-transition back to
        // `Steady` so the predictor stays correct.
        match self.state {
            NextState::Steady => {}
            NextState::PendingEnd(name_start, name_end) => {
                // Source-relative offsets — empty-element close was
                // queued by `read_start_element`, where the scanner was
                // on the original source.  Drop back to Steady before
                // returning so the next call hits the hot arm.
                self.state = NextState::Steady;
                let src = self.scan.src_bytes();
                return Ok(BytesEvent::EndElement(BytesEndTag {
                    src, name_start, name_end,
                }));
            }
            NextState::PendingEntityRef(name_start, name_end) => {
                // Text-content loop saw an unresolved `&name;` and
                // bailed; emit the queued EntityRef event now.
                self.state = NextState::Steady;
                let src = self.scan.src_bytes();
                return Ok(BytesEvent::EntityRef(BytesEntityRef {
                    src, name_start, name_end,
                }));
            }
            NextState::NeedsProlog => {
                self.parse_prolog()?;
                self.state = NextState::Steady;
            }
        }

        // ── hot dispatch: cursor in locals ──────────────────────────
        //
        // The hot dispatch holds the cursor in stack locals
        // (`bytes`, `end`, `p`) so LLVM can register-allocate them
        // across the per-event dispatch instead of reloading from the
        // Scanner on every method call.  This is correct ONLY when
        // the active stream is the original source — when an entity
        // expansion has pushed a replacement-text stream, `cur_pos`
        // is relative to the entity bytes, not `src_bytes()`, and
        // mixing the two reads from the wrong buffer.  Bail to the
        // method-based dispatch in that case.
        if !self.scan.on_original_source() {
            return self.next_in_entity();
        }
        let bytes = self.scan.src_bytes();
        let end   = bytes.len();
        let p_in  = self.scan.cur_pos();
        let mut p = p_in;

        // Skip whitespace.  Always at depth 0 (between top-level
        // constructs); deeper depths only when the caller opts in via
        // `ParseOptions::skip_inter_element_whitespace`.
        //
        // SAFETY (all the `get_unchecked` below): `p`/`q` are bounded by
        // `< end` and `end == bytes.len()`.  Hand-bounded to keep this
        // per-event loop branch-light; see CONTRIBUTING.md § "Unsafe policy".
        #[inline(always)]
        fn is_xml_ws(b: u8) -> bool {
            b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
        }
        if self.depth == 0 {
            // Top-level: whitespace between the prolog/root/misc is never
            // part of any element's content — always skip it.
            while p < end && is_xml_ws(unsafe { *bytes.get_unchecked(p) }) {
                p += 1;
            }
        } else if self.scan.opts.skip_inter_element_whitespace {
            // libxml2 `remove_blank_text` (areBlanks): a whitespace-only
            // run is "ignorable" — and dropped — only when it sits between
            // element siblings.  Peek the run, then decide from what
            // follows it, leaving `p` put (so the run is read as text)
            // when it should be kept.
            let mut q = p;
            while q < end && is_xml_ws(unsafe { *bytes.get_unchecked(q) }) {
                q += 1;
            }
            if q > p {
                let drop = match bytes.get(q).copied() {
                    // Followed by an end tag: ignorable only if this
                    // element already holds a child element; a leaf
                    // element's sole whitespace content is significant.
                    Some(b'<') if bytes.get(q + 1) == Some(&b'/') => {
                        self.frame_saw_child.last().copied().unwrap_or(false)
                    }
                    // Followed by another element (or comment / PI):
                    // inter-element whitespace, ignorable.
                    Some(b'<') => true,
                    // Followed by character data (or EOF): this is the
                    // leading whitespace of a non-blank text node — keep
                    // it, never strip prose.
                    _ => false,
                };
                if drop {
                    p = q;
                }
            }
        }

        // EOF — element content has no entity streams in this path,
        // so `is_eof()` reduces to a simple bounds check.
        if p >= end {
            self.scan.cur_set_pos(p);
            // XML 1.0 § 3.1: every start tag must have a matching
            // end tag before the document ends.  Reject if the
            // element stack is still open.  Gated on the same
            // skip_end_tag_check flag that disables paired-name
            // matching, since both checks are about structural
            // well-formedness and a caller streaming fragments will
            // want both relaxed together.
            if self.depth > 0 && !self.scan.opts.skip_end_tag_check {
                // Recovery: synthesise an EndElement event for the
                // topmost open element, log a per-element error, and
                // return.  Subsequent next() calls land here again
                // until depth == 0, then we fall through to Eof —
                // the caller sees a clean tree close-out and the
                // recovered_errors list itemises which elements
                // were unclosed.
                //
                // SAFETY: indexing element_stack — guarded by the
                // `depth > 0` precondition AND the fact that
                // !skip_end_tag_check means the stack is maintained
                // in lockstep with depth.  The element_stack pop
                // happens inside synthesize_close, so we read here
                // before mutating.
                let name_lossy = self.element_stack.last()
                    .map(|e| String::from_utf8_lossy(e.name_bytes(bytes)).into_owned())
                    .unwrap_or_else(|| "?".to_string());
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!(
                        "unclosed element '<{name_lossy}>' at end of document \
                         (XML 1.0 § 3.1 [STag/ETag])"
                    ),
                ).with_code(crate::error::ErrorCode::TagNotFinished);
                self.maybe_recover(err)?;
                return Ok(self.synthesize_close());
            }
            // XML 1.0 § 2.1 [document] = prolog element Misc*.
            // The single root [element] is REQUIRED; an empty
            // document (one with only whitespace / comments / a
            // DOCTYPE) is not well-formed.
            if !self.root_seen && !self.scan.opts.skip_end_tag_check {
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    "document has no root element (XML 1.0 § 2.1 [document])",
                ).with_code(crate::error::ErrorCode::DocumentEmpty);
                self.maybe_recover(err)?;
                // Nothing to synthesise — empty doc stays empty.
            }
            return Ok(BytesEvent::Eof);
        }

        // SAFETY: the `if p >= end { return Eof }` above proves `p <
        // end == bytes.len()` here, so `bytes.get_unchecked(p)` is
        // in bounds.
        // Why unsafe: dispatched on every event; bounds check would
        // run per call.  See CONTRIBUTING.md § "Unsafe policy".
        let b0 = unsafe { *bytes.get_unchecked(p) };

        if b0 != b'<' {
            // XML 1.0 § 2.1 [document]: text is forbidden at the
            // document level — only Misc (whitespace / comments /
            // PIs) is allowed outside the root element.  Whitespace
            // was already consumed by the depth-0 skip-ws above, so
            // any non-`<` byte at depth 0 here is real text content
            // appearing illegally outside a root.
            if self.depth == 0 && !self.scan.opts.skip_end_tag_check {
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    "text content not allowed at the document level \
                     (XML 1.0 § 2.1 [document])",
                );
                self.maybe_recover(err)?;
                // Recovery: emit the doc-level text as a Text event
                // so the user can see what was there.  Better than
                // libxml2 which sometimes silently loses the root
                // element OR the trailing text depending on
                // position.  read_text scans up to the next `<` or
                // EOF.
            }
            // Text-content path: write the cursor back and let the
            // existing slow path handle entity references and the
            // `]]>` check.  Skip the store when the local cursor
            // didn't actually advance (no whitespace consumed at
            // depth > 0 — the common in-element case).
            if p != p_in {
                self.scan.cur_set_pos(p);
            }
            return self.read_text();
        }

        // ── single-load `<x` dispatch ───────────────────────────────
        //
        // Replaces four serial `starts_with` calls (each loading
        // `cur_ptr`/`cur_len` and comparing 2-9 bytes) with one byte
        // load and a small jump table.  The `read_*` methods re-validate
        // the prefix via `expect_str`, so a mis-dispatch on a malformed
        // input still produces a fatal error — just from a slightly
        // different call site.
        let b1 = if p + 1 < end {
            // SAFETY: the `if p + 1 < end` guard proves `p + 1 <
            // bytes.len()`.
            // Why unsafe: dispatched on every `<` we encounter; this
            // is the per-tag dispatch byte read.  See CONTRIBUTING.md
            // § "Unsafe policy".
            unsafe { *bytes.get_unchecked(p + 1) }
        } else {
            0
        };
        // Skip the writeback when no whitespace was consumed — common
        // case inside an element (depth > 0) where p == p_in.
        if p != p_in {
            self.scan.cur_set_pos(p);
        }
        match b1 {
            b'/' => self.read_end_element(),
            b'?' => self.read_pi(),
            b'!' => {
                // `<!` either opens a comment (`<!--`), CDATA
                // (`<![CDATA[`), or — at the document level only —
                // a DOCTYPE declaration (`<!DOCTYPE …>`).  The old
                // prolog handler ate DOCTYPE before any user-visible
                // event, but with comments now emitted as events
                // (rather than silently skipped in the prolog), a
                // DOCTYPE that follows a comment lands back in this
                // dispatch loop — so it has to be handled here too.
                // Anything else is malformed and falls through to
                // `dispatch_start_element` so the existing
                // name-validation error fires (matches the
                // pre-refactor dispatch behaviour for inputs like
                // `<!X`).
                let b2 = if p + 2 < end {
                    // SAFETY: the `if p + 2 < end` guard proves `p +
                    // 2 < bytes.len()`.
                    // Why unsafe: same per-tag-dispatch hot path as
                    // `b1` above.  See CONTRIBUTING.md § "Unsafe
                    // policy".
                    unsafe { *bytes.get_unchecked(p + 2) }
                } else {
                    0
                };
                match b2 {
                    b'-' => self.read_comment(),
                    b'[' => {
                        // CDATA sections are part of [content], so they
                        // are only legal *inside* an element (depth > 0).
                        // At the document level they're a fatal error
                        // (XML 1.0 § 2.1 [document] / § 3.1 [content]).
                        if self.depth == 0
                            && !self.scan.opts.skip_end_tag_check
                        {
                            return Err(self.scan.err(
                                "CDATA sections are only allowed inside an element \
                                 (XML 1.0 § 3.1 [content])"
                            ));
                        }
                        self.read_cdata()
                    }
                    b'D' | b'd' if self.depth == 0
                        && (self.scan.starts_with(b"<!DOCTYPE")
                            || self.scan.starts_with(b"<!doctype")) =>
                    {
                        // Consume the DOCTYPE in-place and then
                        // recurse to pick up the next real event.
                        // `parse_doctype` already returns after the
                        // closing `]>` so the cursor is positioned
                        // at the post-DOCTYPE byte.
                        self.parse_doctype()?;
                        self.next()
                    }
                    _    => self.dispatch_start_element(),
                }
            }
            _ => {
                // Bare `<` recovery: a `<` followed by something
                // that isn't a NameStartChar (whitespace, digit,
                // EOF, etc.) can't open a real start tag.  In
                // recover mode, treat the `<` as literal text and
                // continue.  Preserves user data — unlike libxml2
                // which silently drops the `<` from the text
                // payload.
                let looks_like_name_start = matches!(
                    b1,
                    b'A'..=b'Z' | b'a'..=b'z' | b'_' | b':' | 0x80..=0xFF
                );
                if !looks_like_name_start
                    && self.scan.opts.recovery_mode
                    && self.depth > 0
                {
                    let err = self.scan.err_with_level(
                        ErrorLevel::Error,
                        "bare '<' in text content — kept literal \
                         (XML 1.0 § 2.4 [CharData])",
                    );
                    self.recovered_errors.push(err);
                    // Emit a Text("<") event and advance past the
                    // `<`; the next event will pick up at the
                    // following byte.  This produces an event
                    // stream like Text("1 "), Text("<"), Text(" 2")
                    // for input `<r>1 < 2</r>` — the caller can
                    // concatenate text events to recover the
                    // original bytes.
                    self.scan.cur_set_pos(p + 1);
                    // Manufacture a Text event by slicing the one
                    // `<` byte directly out of `src_bytes()` — no
                    // allocation.
                    let src = self.scan.src_bytes();
                    let lt_slice = &src[p..p + 1];
                    return Ok(BytesEvent::Text(BytesText {
                        inner: std::borrow::Cow::Borrowed(lt_slice),
                    }));
                }
                self.dispatch_start_element()
            }
        }
    }

    /// Slow-path dispatch used when an entity-replacement stream is
    /// active.  The local-cursor fast path in `next()` reads
    /// `bytes = src_bytes()` and `p = cur_pos()`, which is wrong
    /// when an entity is being expanded (cur_pos is relative to the
    /// entity bytes, not the original source).  This method uses
    /// the small-method scanner API throughout — slower, correct,
    /// and rare (only fires inside entity content).  Always called
    /// with depth > 0 (we entered the entity inside an element), so
    /// the document-level structural checks in the fast path don't
    /// apply here.
    fn next_in_entity(&mut self) -> Result<BytesEvent<'_, 'src>> {
        if self.scan.opts.skip_inter_element_whitespace {
            self.scan.skip_ws();
        }
        // The active entity stream may be fully consumed.  Pop it
        // so the next event is read from the parent stream below.
        // XML 1.0 § 4.3.2 WFC 'Logical Structure': the
        // element-stack depth at the entity's current position
        // must equal the depth captured when it was pushed —
        // otherwise the entity's replacement text contains
        // unbalanced markup and is rejected.
        while self.scan.cur_pos() >= self.scan.cur_len()
              && !self.scan.on_original_source()
        {
            let depth_now = self.element_stack.len() as u32;
            if let Some((name, depth_at_push)) = self.scan.top_entity_info() {
                if depth_at_push != depth_now {
                    return Err(self.scan.err(format!(
                        "entity '&{name};' contains unbalanced element markup — \
                         element-stack depth was {depth_at_push} when the entity \
                         was expanded but is {depth_now} at its end \
                         (XML 1.0 § 4.3.2 WFC 'Logical Structure')"
                    )));
                }
            }
            if !self.scan.try_pop_entity_stream() {
                break;
            }
        }
        // After popping we may now be on the original source —
        // re-enter `next()` to take the fast path.
        if self.scan.on_original_source() {
            return self.next();
        }
        if self.scan.is_eof() {
            // We're inside an entity stream; if we hit document EOF
            // here, the entity straddled a structural boundary.  The
            // depth check at fast-path EOF doesn't fire because we
            // never returned to the original source; surface the
            // error here instead.  In recover mode, synthesise a
            // close just as the fast-path EOF does.
            if self.depth > 0 && !self.scan.opts.skip_end_tag_check {
                let bytes = self.scan.src_bytes();
                let name_lossy = self.element_stack.last()
                    .map(|e| String::from_utf8_lossy(e.name_bytes(bytes)).into_owned())
                    .unwrap_or_else(|| "?".to_string());
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!(
                        "unclosed element '<{name_lossy}>' at end of document \
                         (XML 1.0 § 3.1 [STag/ETag])"
                    ),
                ).with_code(crate::error::ErrorCode::TagNotFinished);
                self.maybe_recover(err)?;
                return Ok(self.synthesize_close());
            }
            return Ok(BytesEvent::Eof);
        }
        if self.scan.peek() != Some(b'<') {
            return self.read_text();
        }
        if      self.scan.starts_with(b"</")        { self.read_end_element() }
        else if self.scan.starts_with(b"<!--")      { self.read_comment() }
        else if self.scan.starts_with(b"<![CDATA[") { self.read_cdata() }
        else if self.scan.starts_with(b"<?")        { self.read_pi() }
        else                                        { self.dispatch_start_element() }
    }

    /// Wrapper around `read_start_element` that enforces XML 1.0
    /// § 2.1 [document]: at the document level, exactly one root
    /// element is allowed.  A second start tag at depth 0 after
    /// the root element has closed is a fatal error.  Gated on
    /// `!skip_end_tag_check` (callers who relax end-tag pairing
    /// have opted out of structural checks).
    #[inline]
    fn dispatch_start_element(&mut self) -> Result<BytesEvent<'_, 'src>> {
        if self.depth == 0
            && self.root_seen
            && !self.scan.opts.skip_end_tag_check
        {
            // XML 1.0 § 2.1 [document]: exactly one root element.
            // In recover mode, log the violation and accept the
            // second root anyway — the caller can still walk the
            // events.  The resulting event stream is no longer a
            // single-rooted document, which the caller should be
            // aware of via `recovered_errors()`.
            let err = self.scan.err_with_level(
                ErrorLevel::Error,
                "only one root element allowed (XML 1.0 § 2.1 [document])",
            );
            self.maybe_recover(err)?;
        }
        self.read_start_element()
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
    pub fn next_into(&mut self, buf: &mut Vec<BytesAttr<'src>>) -> Result<BytesEventInto<'src>> {
        buf.clear();
        match self.next()? {
            BytesEvent::StartElement(tag) => {
                let name = tag.name_cow();
                for attr in tag.attrs() {
                    buf.push(attr?);
                }
                Ok(BytesEventInto::StartElement { name })
            }
            BytesEvent::EndElement(tag) => Ok(BytesEventInto::EndElement {
                name: Cow::Borrowed(tag.name()),
            }),
            BytesEvent::Text(t)    => Ok(BytesEventInto::Text(t.into_bytes())),
            BytesEvent::CData(s)   => Ok(BytesEventInto::CData(s.into_bytes())),
            BytesEvent::Comment(s) => Ok(BytesEventInto::Comment(s.into_bytes())),
            BytesEvent::Pi(pi)     => {
                let (target, content) = pi.into_parts();
                Ok(BytesEventInto::Pi { target, content })
            }
            BytesEvent::EntityRef(e) => Ok(BytesEventInto::EntityRef {
                name: Cow::Borrowed(e.name()),
            }),
            BytesEvent::Eof        => Ok(BytesEventInto::Eof),
        }
    }

    // ── prolog ────────────────────────────────────────────────────────────────

    fn parse_prolog(&mut self) -> Result<()> {
        // XML 1.0 § 2.2: validate every byte once before streaming begins.
        // One SWAR sweep here is faster than folding the check into every
        // byte-consuming hot path (the bulk pass amortizes SIMD setup over
        // the whole document; per-content-slice calls re-pay fixed overhead
        // and don't fit enough bytes in their SWAR loop on short slices).
        if !self.scan.opts.skip_xml_char_validation {
            validate_xml_chars(self.scan.cur_bytes())?;
        }
        if self.scan.starts_with(&[0xEF, 0xBB, 0xBF]) { self.scan.skip_n(3); }
        if self.scan.starts_with(b"<?xml")
            && matches!(self.scan.peek_at(5), Some(b' ' | b'\t' | b'\r' | b'\n' | b'?'))
        {
            // Recovery: a malformed XML declaration (missing
            // version, bad value, etc.) is logged; we then scan
            // forward to the next `?>` and continue with the
            // rest of the document.  Matches libxml2's behaviour
            // (silent skip past the bad decl).
            if let Err(e) = self.skip_xml_decl() {
                if e.level == ErrorLevel::Fatal || !self.scan.opts.recovery_mode {
                    return Err(e);
                }
                self.recovered_errors.push(e);
                // Resync to the closing `?>`.  If we don't find
                // one, give up — the input is structurally weird
                // beyond what our heuristic can repair.
                match memchr(b'?', self.scan.cur_tail()) {
                    Some(off) => {
                        self.scan.cur_advance_pos(off);
                        if self.scan.starts_with(b"?>") {
                            self.scan.skip_n(2);
                        } else {
                            self.scan.advance();
                        }
                    }
                    None => {
                        // No `?` at all in the rest of the input —
                        // can't safely resync.  Fall through to
                        // skip_misc which will end at the next
                        // structural token (or EOF).
                    }
                }
            }
        }
        self.skip_misc()
    }

    fn skip_xml_decl(&mut self) -> Result<()> {
        // XML 1.0 § 2.8 [XMLDecl]:
        //     '<?xml' VersionInfo EncodingDecl? SDDecl? S? '?>'
        //     VersionInfo  ::= S 'version'    Eq ("'" VersionNum "'" | '"' VersionNum '"')
        //     EncodingDecl ::= S 'encoding'   Eq ('"' EncName "'" | "'" EncName "'")
        //     SDDecl       ::= S 'standalone' Eq (("'" ('yes'|'no') "'") | ('"' ('yes'|'no') '"'))
        // The S between each attribute is REQUIRED, and each value
        // has its own production we must validate against.
        self.scan.expect_str(b"<?xml")?;
        self.scan.skip_ws();

        // ── required: VersionInfo ────────────────────────────────
        if !self.scan.starts_with(b"version") {
            return Err(self.scan.err_with_level(
                ErrorLevel::Error,
                "XML declaration is missing the required `version` attribute \
                 (XML 1.0 § 2.8 [XMLDecl])"
            ));
        }
        let version = self.consume_xmldecl_attr_value(b"version")?;
        // VersionNum = '1.' [0-9]+ — bytes only, no internal
        // whitespace.  Most documents say "1.0" or "1.1".
        if !is_valid_version(&version) {
            return Err(self.scan.err_with_level(
                ErrorLevel::Error,
                format!(
                    "invalid XML version '{}' (XML 1.0 § 2.8 [26] [VersionNum])",
                    String::from_utf8_lossy(&version)
                ),
            ));
        }

        let mut encoding_bytes: Option<Vec<u8>> = None;
        let mut standalone_bool: Option<bool>   = None;

        // ── optional: EncodingDecl ────────────────────────────────
        // S is required between attributes when both are present.
        // `saw_ws` records the whitespace between `version="..."`
        // and the next attribute (whichever it is).  When encoding
        // is omitted, this same flag carries over to the standalone
        // check — re-skipping below would consume zero bytes and
        // falsely report "expected whitespace before standalone"
        // for inputs like `<?xml version='1.0' standalone='yes'?>`.
        let mut saw_ws = self.scan_skip_ws_returning_count() > 0;
        if self.scan.starts_with(b"encoding") {
            if !saw_ws {
                return Err(self.scan.err_with_level(
                    ErrorLevel::Error,
                    "expected whitespace before `encoding` in XML declaration \
                     (XML 1.0 § 2.8 [XMLDecl])"
                ));
            }
            let enc = self.consume_xmldecl_attr_value(b"encoding")?;
            if !is_valid_encname(&enc) {
                return Err(self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!(
                        "invalid encoding name '{}' (XML 1.0 § 4.3.3 [81] [EncName])",
                        String::from_utf8_lossy(&enc)
                    ),
                ));
            }
            encoding_bytes = Some(enc);
            // Encoding consumed — refresh `saw_ws` for the standalone
            // check, which now needs its own preceding whitespace.
            saw_ws = self.scan_skip_ws_returning_count() > 0;
        }

        // ── optional: SDDecl ──────────────────────────────────────
        if self.scan.starts_with(b"standalone") {
            if !saw_ws {
                return Err(self.scan.err(
                    "expected whitespace before `standalone` in XML declaration \
                     (XML 1.0 § 2.8 [XMLDecl])"
                ));
            }
            let sd = self.consume_xmldecl_attr_value(b"standalone")?;
            standalone_bool = match &sd[..] {
                b"yes" => Some(true),
                b"no"  => Some(false),
                _ => return Err(self.scan.err(format!(
                    "invalid 'standalone' value '{}' — must be \"yes\" or \"no\" \
                     (XML 1.0 § 2.9 [32] [SDDecl])",
                    String::from_utf8_lossy(&sd)
                ))),
            };
        }

        self.scan.skip_ws();
        self.scan.expect_str(b"?>")?;

        // Capture for callers (arena Document, serializer round-trip).
        // All three fields are guaranteed-valid ASCII at this point —
        // is_valid_version / is_valid_encname / b"yes"|b"no" enforce that.
        // Stash a fast version-test flag so downstream parsing code
        // can branch on 1.0 vs 1.1 semantics (NEL/LS line-ending
        // normalization, C0 character-reference acceptance, expanded
        // name-character ranges) without an Option<String> compare on
        // every check.
        self.is_xml_11 = version.as_slice() == b"1.1";
        self.xml_decl = Some(XmlDeclInfo {
            version:    String::from_utf8(version).expect("validated ASCII"),
            encoding:   encoding_bytes.map(|b| String::from_utf8(b).expect("validated ASCII")),
            standalone: standalone_bool,
        });
        self.standalone_yes = standalone_bool == Some(true);
        Ok(())
    }

    /// Skip whitespace and report how many bytes were consumed.
    /// Used inside the XML declaration where some inter-attribute S
    /// is required and we need to know whether any was actually
    /// present to emit a precise error.
    fn scan_skip_ws_returning_count(&mut self) -> usize {
        let before = self.scan.cur_pos();
        self.scan.skip_ws();
        self.scan.cur_pos() - before
    }

    /// Consume one `name = "value"` attribute pair inside the XML
    /// declaration and return the (raw) value bytes.  Caller has
    /// already verified `starts_with(name)`; we advance past the
    /// name then parse `S? '=' S? AttValue`.
    fn consume_xmldecl_attr_value(&mut self, name: &[u8]) -> Result<Vec<u8>> {
        self.scan.skip_n(name.len());
        self.scan.skip_ws();
        self.scan.expect(b'=')?;
        self.scan.skip_ws();
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted XML-decl value")),
        };
        let val_start = self.scan.cur_pos();
        match memchr(q, self.scan.cur_tail()) {
            None => Err(self.scan.err("unterminated XML-decl value")),
            Some(off) => {
                let end = val_start + off;
                let bytes = self.scan.cur_slice(val_start, end).into_owned();
                self.scan.cur_set_pos(end + 1);
                Ok(bytes)
            }
        }
    }

    fn skip_misc(&mut self) -> Result<()> {
        // Only structurally-significant items still consume bytes
        // here (DOCTYPE, leading whitespace).  Comments and PIs are
        // left in the stream so the main `next()` dispatch can emit
        // them as `BytesEvent::Comment` / `BytesEvent::Pi` — this
        // is what lets consumers see prolog markup
        // (`<!--…--><root/>`) in document order.  Before this
        // change, `skip_comment_raw` ate the bytes silently and
        // the prolog comment was lost.
        loop {
            self.scan.skip_ws();
            if self.scan.starts_with(b"<!DOCTYPE") || self.scan.starts_with(b"<!doctype") {
                self.parse_doctype()?;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn skip_quoted(&mut self) -> Result<()> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted value")),
        };
        // SIMD-fast jump to the closing quote — beats the byte-by-byte
        // peek/advance loop on long literals (DOCTYPE PUBLIC / SYSTEM
        // URLs are typically 50-100 chars).
        match memchr(q, self.scan.cur_tail()) {
            None => Err(self.scan.err("unterminated quoted value")),
            Some(off) => {
                self.scan.cur_advance_pos(off + 1);
                Ok(())
            }
        }
    }

    /// Variant of [`skip_quoted`] that returns the literal contents
    /// instead of discarding them.  Used by `parse_doctype` to
    /// capture the SYSTEM identifier when external-subset loading
    /// is enabled.
    fn capture_quoted(&mut self) -> Result<String> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted value")),
        };
        match memchr(q, self.scan.cur_tail()) {
            None => Err(self.scan.err("unterminated quoted value")),
            Some(off) => {
                let bytes = self.scan.cur_slice(self.scan.cur_pos(), self.scan.cur_pos() + off);
                let s = String::from_utf8_lossy(&bytes).into_owned();
                self.scan.cur_advance_pos(off + 1);
                Ok(s)
            }
        }
    }

    /// Like [`skip_quoted`] but also validates the literal content
    /// against the rules for an XML SystemLiteral / URI:
    /// XML 1.0 § 4.2.2 [11] forbids `#` fragment identifiers in
    /// SystemLiterals (the spec says implementations may issue an
    /// error or warning if the SystemLiteral is not a properly
    /// formed URI reference; well-formed URI references in this
    /// context exclude `#fragment`).  This is what catches
    /// `<!ENTITY foo SYSTEM "foo#bar">`.
    /// Read a quoted SystemLiteral (XML 1.0 § 4.2.2 [11]) and
    /// return its bytes (without the surrounding quotes).  Used
    /// by entity-decl parsing when an `external_resolver` is
    /// configured — we need the URL to pass to the resolver.  Used by entity-decl
    /// parsing when an `external_resolver` is configured — we need
    /// the URL to pass to the resolver.
    fn read_system_literal(&mut self) -> Result<String> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted SystemLiteral")),
        };
        let start = self.scan.cur_pos();
        match memchr(q, self.scan.cur_tail()) {
            None => Err(self.scan.err("unterminated SystemLiteral")),
            Some(off) => {
                let end = start + off;
                let bytes = &self.scan.cur_bytes()[start..end];
                if memchr(b'#', bytes).is_some() {
                    return Err(self.scan.err(
                        "URI fragment ('#…') is not allowed in a SystemLiteral \
                         (XML 1.0 § 4.2.2 [11])"
                    ));
                }
                // SAFETY: bytes are sourced from a Scanner whose
                // input is guaranteed UTF-8.  Why unsafe: avoids
                // re-validating UTF-8 we already know is good.
                let s = unsafe { std::str::from_utf8_unchecked(bytes) }.to_string();
                self.scan.cur_advance_pos(off + 1);
                Ok(s)
            }
        }
    }

    /// Read a quoted PubidLiteral and return its bytes as a
    /// String.  Caller has already validated PubidChar via
    /// `skip_pubid_literal` semantics — we reuse that path then
    /// simply return the captured slice.
    fn read_pubid_literal(&mut self) -> Result<String> {
        // Save cursor, run the validating skip, then re-extract the
        // literal bytes from the source.  Cheap because pubid
        // literals are short.
        let q = match self.scan.peek() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted PubidLiteral")),
        };
        // Skip past opening quote.
        self.scan.advance();
        let start = self.scan.cur_pos();
        // Scan to closing quote, validating as in skip_pubid_literal.
        let off = memchr(q, self.scan.cur_tail())
            .ok_or_else(|| self.scan.err("unterminated PubidLiteral"))?;
        let end = start + off;
        let bytes = &self.scan.cur_bytes()[start..end];
        for &b in bytes {
            if !is_pubid_char(b) {
                return Err(self.scan.err(format!("invalid PubidChar 0x{b:02X}")));
            }
        }
        // SAFETY: PubidChar is a subset of ASCII (validated above);
        // ASCII is valid UTF-8.  Why unsafe: skip the redundant
        // from_utf8 pass.
        let s = unsafe { std::str::from_utf8_unchecked(bytes) }.to_string();
        self.scan.cur_advance_pos(off + 1);
        Ok(s)
    }

    fn skip_comment_raw(&mut self) -> Result<()> {
        self.scan.expect_str(b"<!--")?;
        loop {
            match memchr(b'-', self.scan.cur_tail()) {
                None => return Err(self.scan.err("unterminated comment")),
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                    if self.scan.starts_with(b"-->") { self.scan.skip_n(3); return Ok(()); }
                    if self.scan.starts_with(b"--") { return Err(self.scan.err("'--' inside comment not allowed")); }
                    self.scan.advance();
                }
            }
        }
    }

    fn skip_pi_raw(&mut self) -> Result<()> {
        self.scan.expect_str(b"<?")?;
        // XML 1.0 § 2.6 [16] [17]:
        //   PI       ::= '<?' PITarget (S (Char* - (Char* '?>' Char*)))? '?>'
        //   PITarget ::= Name - (('X'|'x')('M'|'m')('L'|'l'))
        // The literal name `xml` (any case) is reserved.  After the
        // target, the next char MUST be either `?>` or whitespace
        // followed by content.
        let target = self.scan.scan_name_bytes()?;
        if target.eq_ignore_ascii_case(b"xml") {
            return Err(self.scan.err(
                "PI target name 'xml' is reserved (XML 1.0 § 2.6 [17])"
            ));
        }
        match self.scan.peek() {
            Some(b'?') => {
                // Immediate close — `<?target?>` with no content.
                self.scan.expect_str(b"?>")?;
                return Ok(());
            }
            Some(b' ' | b'\t' | b'\r' | b'\n') => {} // OK, S follows
            Some(b) => return Err(self.scan.err(format!(
                "expected whitespace or `?>` after PI target, got '{}' (XML 1.0 § 2.6 [16])",
                b as char
            ))),
            None => return Err(self.scan.err("unterminated PI")),
        }
        loop {
            match memchr(b'?', self.scan.cur_tail()) {
                None => return Err(self.scan.err("unterminated PI")),
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                    if self.scan.starts_with(b"?>") { self.scan.skip_n(2); return Ok(()); }
                    self.scan.advance();
                }
            }
        }
    }

    fn parse_doctype(&mut self) -> Result<()> {
        // Record how many prolog comments/PIs preceded this DOCTYPE so
        // the internal-subset node can be spliced into the document
        // sibling chain at its true position (see
        // `Dtd::internal_subset_prolog_index`).
        self.dtd.internal_subset_prolog_index = self.prolog_misc_count;
        self.scan.skip_n(9); // "<!DOCTYPE"
        self.scan.expect_ws()?;
        self.scan.skip_ws();
        // Capture the root name so `docinfo.root_name` / the doctype
        // serialisation round-trip correctly.  Names are pure ASCII
        // identifiers — capture the bytes between scan-name's
        // before/after offsets.
        let name_start = self.scan.src_offset();
        self.scan.skip_name()?;
        let name_end = self.scan.src_offset();
        // SAFETY: scan.skip_name advanced over a valid XML Name in
        // the input buffer; those bytes are valid UTF-8.
        let root_name = unsafe {
            std::str::from_utf8_unchecked(&self.scan.src_bytes()[name_start..name_end])
        }.to_string();
        self.dtd.root_name = root_name;
        self.scan.skip_ws();

        // SYSTEM / PUBLIC identifier for the optional external subset.
        // We always capture both for `docinfo.public_id` /
        // `docinfo.system_url`; the captured system-id additionally
        // drives external-subset loading when `load_external_dtd` is
        // on.
        let mut external_system_id: Option<String> = None;
        if self.scan.starts_with(b"SYSTEM") || self.scan.starts_with(b"PUBLIC") {
            let is_public = self.scan.starts_with(b"PUBLIC");
            self.scan.skip_n(6);
            self.scan.expect_ws()?;
            self.scan.skip_ws();
            if is_public {
                let pub_id = self.capture_pubid_literal()?;
                self.dtd.public_id = Some(pub_id);
                // Per XML 1.0 § 4.2.2 [75]: PUBLIC PubidLiteral
                // SystemLiteral — whitespace between the two is
                // REQUIRED.  We diverge from strict spec only in
                // letting the SystemLiteral be omitted entirely
                // (the HTML-style `<!DOCTYPE html PUBLIC "...">`
                // shape libxml2/lxml accept).  When omitted, we
                // expect `>` or `[` next; mandatory whitespace is
                // still enforced before a SystemLiteral.
                let saw_ws = matches!(self.scan.peek(), Some(b' ' | b'\t' | b'\n' | b'\r'));
                self.scan.skip_ws();
                if matches!(self.scan.peek(), Some(b'"' | b'\'')) {
                    if !saw_ws {
                        return Err(self.scan.err(
                            "whitespace is required between PubidLiteral and SystemLiteral \
                             (XML 1.0 § 4.2.2 [75] ExternalID)"
                        ));
                    }
                    let sys_id = self.capture_quoted()?;
                    self.dtd.system_id = Some(sys_id.clone());
                    if self.scan.opts.load_external_dtd {
                        external_system_id = Some(sys_id);
                    }
                }
            } else {
                let sys_id = self.capture_quoted()?;
                self.dtd.system_id = Some(sys_id.clone());
                // The external DTD subset loads only under
                // `load_external_dtd` (libxml2's `XML_PARSE_DTDLOAD`).  A
                // configured resolver is the *mechanism* for loading it
                // (and for general-entity resolution), not the trigger —
                // lxml always registers one yet defaults to `load_dtd=False`.
                if self.scan.opts.load_external_dtd {
                    external_system_id = Some(sys_id);
                }
            }
            self.scan.skip_ws();
        }

        if self.scan.peek() == Some(b'[') {
            self.scan.advance();
            self.parse_internal_subset()?;
        }

        self.scan.skip_ws();
        self.scan.expect(b'>')?;

        // External subset is loaded AFTER the internal subset and
        // AFTER the closing `>`.  Load failures (file-not-found,
        // non-UTF-8, network URI) are silently downgraded to
        // warnings inside `load_external_subset` so we still parse
        // the document.  Parse failures (malformed declarations,
        // ill-formed conditional sections, etc.) propagate as real
        // well-formedness errors — except in `recovery_mode` where
        // we demote them to warnings too.  This requires the
        // external-subset parser to expand PE references inside
        // markup declarations (XML 1.0 § 4.4.8) — otherwise valid
        // documents using PE refs in their DTD would all fail.
        if let Some(system_id) = external_system_id {
            if let Err(e) = self.load_external_subset(&system_id) {
                if self.scan.opts.recovery_mode {
                    self.recovered_errors.push(e);
                } else {
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Attempt to load and parse the external DTD subset.
    ///
    /// Two error categories:
    ///   - **Load failures** (resolver refused, file not found,
    ///     non-UTF-8, network URI) → logged as a warning in
    ///     `recovered_errors`; returns `Ok(())`.  These match
    ///     libxml2's "load DTD if you can, ignore otherwise"
    ///     stance when running without strict validation.
    ///   - **Parse failures** (malformed declarations, ill-formed
    ///     conditional sections, etc.) → returned as `Err`.  These
    ///     are well-formedness violations the spec requires us to
    ///     surface; the caller decides whether to propagate or
    ///     swallow based on `recovery_mode`.
    ///
    /// Source-of-bytes precedence:
    ///   1. `external_resolver`, when configured — the resolver is
    ///      the unified entry point for external loading; its
    ///      allowlists / catalog logic apply.
    ///   2. Direct `std::fs::read`, gated by `load_external_dtd` —
    ///      historical fallback for the lean parse path; only fires
    ///      when no resolver is set.
    fn load_external_subset(&mut self, system_id: &str) -> Result<()> {
        // The DOCTYPE's external-subset SYSTEM is resolved against
        // the *document* URL — there's no enclosing external entity
        // at this point, so `current_base_uri()` is irrelevant.
        let base = self.scan.opts.base_url.clone();
        let absolute = resolve_uri(system_id, base.as_deref());
        let bytes_result: std::result::Result<Vec<u8>, String> =
            if let Some(resolver) = self.scan.opts.external_resolver.clone() {
                resolver.resolve(None, &absolute, base.as_deref())
                    .map_err(|e| e.to_string())
            } else if self.scan.opts.load_external_dtd {
                // Network URIs are out of scope for the lean path.
                if absolute.starts_with("http://") || absolute.starts_with("https://") {
                    return Ok(());
                }
                let raw_path: &str = absolute.strip_prefix("file://").unwrap_or(&absolute);
                std::fs::read(std::path::Path::new(raw_path)).map_err(|e| e.to_string())
            } else {
                return Ok(());
            };
        let bytes = match bytes_result {
            Ok(b)  => b,
            Err(msg) => {
                // Load failure — log as warning, don't fail the parse.
                self.recovered_errors.push(
                    XmlError::new(
                        ErrorDomain::Dtd,
                        ErrorLevel::Warning,
                        format!("external DTD '{system_id}' not loaded: {msg}"),
                    )
                );
                return Ok(());
            }
        };
        let text = match crate::encoding::transcode_to_utf8(&bytes)
            .map_err(|e| e.message)
            .and_then(|c| String::from_utf8(c.into_owned()).map_err(|e| e.to_string()))
        {
            Ok(s)  => s,
            Err(msg) => {
                self.recovered_errors.push(
                    XmlError::new(
                        ErrorDomain::Dtd,
                        ErrorLevel::Warning,
                        format!("external DTD '{system_id}' not valid UTF-8: {msg}"),
                    )
                );
                return Ok(());
            }
        };
        // Push the file bytes onto the scanner as an entity stream
        // named `__external_dtd__` (the name is for cycle-detection;
        // we won't recursively load).  After the push, the scanner
        // is positioned at byte 0 of the file.  Then loop through
        // declarations until the stream is empty.
        self.scan.push_entity_stream(
            "__external_dtd__".to_string(),
            text,
            self.depth,
            Some(absolute),
        )?;
        let parse_result = consume_text_decl_if_present(&mut self.scan, self.is_xml_11)
            .and_then(|()| self.parse_external_subset_loop());
        // Drain whatever's left on the pushed stream so subsequent
        // parsing sees the original source again — required on both
        // success and error paths, otherwise a parse error mid-decl
        // leaves the scanner pointing at the entity-stream tail.
        while !self.scan.on_original_source() {
            if !self.scan.try_pop_entity_stream() { break; }
        }
        parse_result
    }

    /// Parse a standalone external DTD subset — the markup
    /// declarations a `.dtd` file holds, with no surrounding
    /// `<!DOCTYPE>` wrapper or document body — capturing them into
    /// [`take_dtd`](Self::take_dtd).
    ///
    /// Unlike the internal subset, the external subset permits
    /// conditional sections (`<![INCLUDE[…]]>` / `<![IGNORE[…]]>`) and
    /// top-level parameter-entity references (XML 1.0 § 2.8); this
    /// drives [`parse_external_subset_loop`](Self::parse_external_subset_loop)
    /// directly over `text` so those constructs parse the same way they
    /// would when loaded via a SYSTEM identifier.  The reader is
    /// expected to have been constructed over an empty source —
    /// `text` is pushed as the sole entity-stream frame.
    pub(crate) fn parse_standalone_external_subset(&mut self, text: String) -> Result<()> {
        self.scan.push_entity_stream(
            "__external_dtd__".to_string(),
            text,
            self.depth,
            None,
        )?;
        let result = consume_text_decl_if_present(&mut self.scan, self.is_xml_11)
            .and_then(|()| self.parse_external_subset_loop());
        while !self.scan.on_original_source() {
            if !self.scan.try_pop_entity_stream() { break; }
        }
        result
    }

    /// Declaration-collection loop for the external DTD subset.
    /// Reads `<!ELEMENT>`, `<!ATTLIST>`, `<!ENTITY>`, `<!NOTATION>`,
    /// comments, PIs, and conditional `<![INCLUDE[...]]>` /
    /// `<![IGNORE[...]]>` sections until the pushed stream is
    /// exhausted.  Errors return the first issue but don't abort
    /// the outer parse (the caller logs and continues).
    fn parse_external_subset_loop(&mut self) -> Result<()> {
        loop {
            // The external subset is bounded by the entity-stream
            // frame that `load_external_subset` pushed; once the
            // scanner is back on the original source, we've drained
            // it (either we ran out of bytes inside the frame and
            // popped, or a PE expansion ended at the same frame
            // boundary).  Returning here is critical: without it,
            // the loop would happily continue reading the document
            // body's `<doc>` as if it were a DTD declaration.
            if self.scan.on_original_source() {
                return Ok(());
            }
            self.scan.skip_ws();
            if self.scan.peek().is_none() {
                // Top-of-stream empty — pop the frame and re-check
                // (either we exit via the on_original_source guard
                // above on the next iteration, or we land in a
                // deeper PE frame and keep going).
                if !self.scan.try_pop_entity_stream() {
                    return Ok(());
                }
                continue;
            }
            match self.scan.peek() {
                Some(b'<') => {
                    // XML 1.0 § 2.8 WFC: PE Between Declarations —
                    // a markup declaration's `<!` and `>` must come
                    // from the same entity frame.  Record the frame
                    // depth at start, verify at end.  This catches
                    // declarations split across PE boundaries like:
                    //   <!ENTITY % m "<!ELEMENT x ">
                    //   %m;ANY>
                    // where `<!ELEMENT x ` lives in m's expansion
                    // and the closing `>` in the outer source.
                    let start_depth = self.scan.entity_stream_depth();
                    if      self.scan.starts_with(b"<!--")       { self.skip_comment_raw()?; }
                    else if self.scan.starts_with(b"<!ENTITY")   { self.parse_entity_decl()?; }
                    else if self.scan.starts_with(b"<!ATTLIST")  { self.parse_attlist_decl()?; }
                    else if self.scan.starts_with(b"<!ELEMENT")  { self.parse_element_decl()?; }
                    else if self.scan.starts_with(b"<!NOTATION") { self.parse_notation_decl()?; }
                    else if self.scan.starts_with(b"<?")         { self.skip_pi_raw()?; }
                    else if self.scan.starts_with(b"<![") {
                        self.parse_conditional_section()?;
                    }
                    else {
                        return Err(self.scan.err(
                            "unexpected declaration in external DTD subset"
                        ));
                    }
                    if self.scan.entity_stream_depth() < start_depth {
                        return Err(self.scan.err(
                            "markup declaration is split across a parameter-entity \
                             boundary (XML 1.0 § 2.8 WFC: PE Between Declarations) — \
                             the start `<!` and end `>` of a declaration must come \
                             from the same entity"
                        ));
                    }
                }
                Some(b'%') => {
                    // Parameter-entity reference at the top level
                    // between declarations.  Per XML 1.0 § 4.4.8
                    // "Included", expand the PE so the next loop
                    // iteration sees its replacement text.
                    self.expand_pe_ref_at_cursor()?;
                }
                _ => return Err(self.scan.err("unexpected content in external DTD subset")),
            }
        }
    }

    /// Expand the parameter-entity reference at the current scanner
    /// position.  Called when `peek() == Some('%')` in a context
    /// where PE references are allowed (the external DTD subset,
    /// PE-replacement text).  Consumes `%name;` from the input,
    /// looks `name` up in [`parameter_entities`], and pushes the
    /// replacement text onto the scanner as a new entity stream
    /// — surrounded by spaces per § 4.4.8 "Included" so the PE
    /// can never silently merge adjacent tokens.
    ///
    /// Undefined PEs return an error (WFC: Entity Declared).
    /// External PEs whose resolver never loaded the bytes are
    /// silently skipped (no replacement text to inject).
    fn expand_pe_ref_at_cursor(&mut self) -> Result<()> {
        self.scan.expect(b'%')?;
        let name_bytes = self.scan.scan_name_bytes()?;
        self.scan.expect(b';')?;
        let name = unsafe { std::str::from_utf8_unchecked(&name_bytes) }.to_string();
        let kind = match self.parameter_entities.get(&name) {
            Some(d) => d.clone(),
            None => {
                // XML 1.0 § 4.1 WFC: Entity Declared has a carve-out:
                // refs that "do not occur within the external subset
                // or a parameter entity" are subject to WFC; refs
                // *inside* an external entity's replacement text
                // (i.e. `current_base_uri().is_some()`) are not — at
                // most a VC violation, which non-validating parsers
                // MUST tolerate.  Log a recoverable warning and
                // expand to empty (the entity might be declared
                // somewhere we haven't read yet).
                if self.scan.current_base_uri().is_some() {
                    self.recovered_errors.push(XmlError::new(
                        ErrorDomain::Parser,
                        ErrorLevel::Warning,
                        format!(
                            "undefined parameter entity '%{name};' inside an external \
                             entity — WFC: Entity Declared carve-out applies (XML 1.0 § 4.1); \
                             expansion skipped"
                        ),
                    ));
                    return Ok(());
                }
                return Err(self.scan.err(format!(
                    "undefined parameter entity '%{name};' (XML 1.0 § 4.1 WFC: Entity Declared)"
                )));
            }
        };
        self.pe_ref_in_internal_subset_seen = true;
        let is_external_value = kind.kind.is_external_value();
        let value = match kind.kind {
            EntityKind::InternalText(v) | EntityKind::ExternalLoaded(v) => v,
            EntityKind::ExternalUnloaded => return Ok(()),
        };
        // §4.4.8 "Included": the replacement text MUST be padded
        // with one leading and one trailing space so the PE can't
        // smudge adjacent tokens in the including context.
        let padded = format!(" {value} ");
        let depth = self.element_stack.len() as u32;
        // Propagate the entity's source URL into the new stream
        // frame so nested SYSTEM identifiers can be resolved
        // relative to where these bytes came from (XML 1.0 § 4.2.2
        // + errata E18).  `None` for internal PEs.
        let frame_base = kind.source_uri.clone();
        self.scan.push_entity_stream(name, padded, depth, frame_base)?;
        if is_external_value {
            consume_text_decl_if_present(&mut self.scan, self.is_xml_11)?;
        }
        Ok(())
    }

    /// Like [`Scanner::skip_ws`] but, in a context where PE
    /// references are allowed (the external DTD subset and
    /// PE-replacement text), also expands any `%name;` it
    /// encounters between whitespace runs.  Required by markup
    /// declaration parsers so their `skip_ws` between tokens
    /// doesn't trip over PE references the spec lets land there.
    fn skip_ws_and_pe_refs(&mut self) -> Result<()> {
        loop {
            self.scan.skip_ws();
            if self.scan.peek().is_none() {
                // Current stream exhausted.  If it's a PE-replacement
                // frame, pop and continue against the parent so the
                // caller's `expect('>')` etc. sees the bytes that
                // lived past the PE reference in the outer source.
                // Without this, e.g.
                //   <!ELEMENT x %ct;>
                // would EOF after consuming `%ct;`'s replacement
                // text and never reach the trailing `>`.
                if self.scan.on_original_source() { return Ok(()); }
                if !self.scan.try_pop_entity_stream() { return Ok(()); }
                continue;
            }
            if self.scan.peek() != Some(b'%') { return Ok(()); }
            // PE references aren't allowed inside markup declarations
            // in the *internal* subset (XML 1.0 § 2.8 WFC: PEs in
            // Internal Subset).  Only expand when we're on
            // PE-replaced or external-subset bytes.
            if self.scan.on_original_source() { return Ok(()); }
            self.expand_pe_ref_at_cursor()?;
        }
    }

    /// `expect_ws` for DTD contexts where a PE reference may stand
    /// in for required whitespace (XML 1.0 § 4.4.8 "Included":
    /// PE replacement text is space-padded, so an expansion at a
    /// whitespace-required boundary contributes its leading space).
    /// Either consumes one or more whitespace bytes OR expands a
    /// PE first and consumes its leading-space pad; on neither,
    /// errors with the underlying `expect_ws` diagnostic.
    fn expect_ws_with_pe(&mut self) -> Result<()> {
        if self.scan.peek() == Some(b'%') && !self.scan.on_original_source() {
            self.expand_pe_ref_at_cursor()?;
        }
        self.scan.expect_ws()?;
        self.skip_ws_and_pe_refs()
    }

    /// Compute the replacement text of an internal entity per
    /// XML 1.0 § 4.5.  The input `bytes` is the raw EntityValue
    /// literal (everything between the surrounding quotes).
    /// The output is the literal with:
    ///
    ///   * Character references (`&#…;`) decoded to their UTF-8
    ///     bytes.
    ///   * Parameter-entity references (`%name;`) replaced by the
    ///     referenced entity's already-computed replacement text
    ///     ("Included in Literal", § 4.4.5 — no space padding,
    ///     unlike "Included" which applies in markup-decl context).
    ///   * General-entity references (`&name;`) left LITERAL —
    ///     they're "Bypassed" per § 4.4.7 and expand only at
    ///     eventual reference time.
    ///
    /// XML 1.0 § 2.8 WFC "PEs in Internal Subset" forbids `%`
    /// references inside markup declarations of the internal
    /// subset; the caller has already enforced this at byte-scan
    /// time, so any `%` reaching here came from external-subset
    /// or PE-replacement text and is legal to expand.
    fn expand_entity_value(&self, bytes: &[u8]) -> std::result::Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'&' => {
                    let after = i + 1;
                    if after >= bytes.len() {
                        return Err("entity value ends with `&`".to_string());
                    }
                    if bytes[after] != b'#' {
                        // General entity reference — bypass per § 4.4.7.
                        let semi = bytes[after..].iter().position(|&c| c == b';')
                            .ok_or_else(|| "named entity reference in entity value missing `;`".to_string())?;
                        out.extend_from_slice(&bytes[i..after + semi + 1]);
                        i = after + semi + 1;
                    } else {
                        // Character reference — decode.
                        let body_start = after + 1;
                        let semi = bytes[body_start..].iter().position(|&c| c == b';')
                            .ok_or_else(|| "character reference missing `;`".to_string())?;
                        let body = &bytes[body_start..body_start + semi];
                        let cp: u32 = if body.first() == Some(&b'x') || body.first() == Some(&b'X') {
                            std::str::from_utf8(&body[1..]).ok()
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or_else(|| format!(
                                    "invalid hex character reference '&#{}'",
                                    String::from_utf8_lossy(body)
                                ))?
                        } else {
                            std::str::from_utf8(body).ok()
                                .and_then(|d| d.parse::<u32>().ok())
                                .ok_or_else(|| format!(
                                    "invalid decimal character reference '&#{}'",
                                    String::from_utf8_lossy(body)
                                ))?
                        };
                        let ch = char::from_u32(cp).ok_or_else(|| format!(
                            "character reference '&#{};' is not a valid Unicode scalar", cp
                        ))?;
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        i = body_start + semi + 1;
                    }
                }
                b'%' => {
                    // Parameter-entity reference.  Look up and
                    // splice replacement text inline (§ 4.4.5
                    // "Included in Literal" — no space padding).
                    let semi = bytes[i + 1..].iter().position(|&c| c == b';')
                        .ok_or_else(|| "PE reference in entity value missing `;`".to_string())?;
                    let name_bytes = &bytes[i + 1..i + 1 + semi];
                    let name = std::str::from_utf8(name_bytes)
                        .map_err(|e| format!("PE name not valid UTF-8: {e}"))?;
                    match self.parameter_entities.get(name).map(|d| &d.kind) {
                        Some(EntityKind::InternalText(v))
                        | Some(EntityKind::ExternalLoaded(v)) => {
                            out.extend_from_slice(v.as_bytes());
                        }
                        Some(EntityKind::ExternalUnloaded) => {
                            // No replacement text to splice — skip
                            // silently, matching how reference-time
                            // expansion handles unloaded externals.
                        }
                        None => {
                            // XML 1.0 § 4.1 WFC: Entity Declared
                            // carve-out for refs inside an external
                            // entity — non-validating parsers MUST
                            // tolerate.  Splice nothing and move on.
                            if self.scan.current_base_uri().is_some() {
                                // skip silently
                            } else {
                                return Err(format!(
                                    "undefined parameter entity '%{name};' \
                                     (XML 1.0 § 4.1 WFC: Entity Declared)"
                                ));
                            }
                        }
                    }
                    i = i + 1 + semi + 1;
                }
                _ => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        Ok(out)
    }

    /// Parse a `<![ … ]]>` conditional section per XML 1.0 § 3.4
    /// [62-65].  Validates that the keyword is `INCLUDE` or
    /// `IGNORE`, that a `[` follows, that the section terminates
    /// with `]]>`, and that nested conditional sections are
    /// balanced.  The body contents themselves are not deeply
    /// validated for now — once the opening / closing tokens
    /// check out we skip the body with nesting-aware scanning.
    fn parse_conditional_section(&mut self) -> Result<()> {
        self.scan.skip_n(3); // consume `<![`
        // The keyword (INCLUDE / IGNORE) is commonly supplied via a
        // PE in real DTDs — `<![ %active; [ … ]]>`.  Expand any PE
        // here so the next token is the actual keyword.
        self.skip_ws_and_pe_refs()?;
        let kw_start = self.scan.cur_pos();
        if self.scan.skip_name().is_err() {
            return Err(self.scan.err(
                "conditional section needs a keyword after '<![' \
                 (XML 1.0 § 3.4 [62])"
            ));
        }
        let kw_end = self.scan.cur_pos();
        let kw = self.scan.cur_slice(kw_start, kw_end).to_vec();
        let is_include = match kw.as_slice() {
            b"INCLUDE" => true,
            b"IGNORE"  => false,
            other => return Err(self.scan.err(format!(
                "conditional section keyword must be INCLUDE or IGNORE, got {:?} \
                 (XML 1.0 § 3.4 [62])",
                String::from_utf8_lossy(other),
            ))),
        };
        self.skip_ws_and_pe_refs()?;
        if !self.scan.starts_with(b"[") {
            return Err(self.scan.err(
                "expected '[' after INCLUDE/IGNORE in conditional section \
                 (XML 1.0 § 3.4 [62])"
            ));
        }
        self.scan.skip_n(1);
        if is_include {
            self.parse_include_section_body()
        } else {
            self.skip_ignore_section_body()
        }
    }

    /// INCLUDE-section body: process declarations until `]]>`.
    /// Mirrors [`parse_external_subset_loop`]'s decl dispatch but
    /// terminates on the conditional-section close-delimiter
    /// rather than on stream exhaustion / original-source return.
    fn parse_include_section_body(&mut self) -> Result<()> {
        loop {
            self.scan.skip_ws();
            if self.scan.starts_with(b"]]>") {
                self.scan.skip_n(3);
                return Ok(());
            }
            // Pop empty PE-replacement frames so the `]]>` in the
            // parent surfaces (PE expansion inside the INCLUDE body
            // is common: `<![INCLUDE[ %decls; ]]>`).
            if self.scan.peek().is_none() {
                if self.scan.on_original_source() {
                    return Err(self.scan.err(
                        "unterminated INCLUDE conditional section (expected ']]>')"
                    ));
                }
                if !self.scan.try_pop_entity_stream() {
                    return Err(self.scan.err(
                        "unterminated INCLUDE conditional section (expected ']]>')"
                    ));
                }
                continue;
            }
            match self.scan.peek() {
                Some(b'<') => {
                    if      self.scan.starts_with(b"<!--")       { self.skip_comment_raw()?; }
                    else if self.scan.starts_with(b"<!ENTITY")   { self.parse_entity_decl()?; }
                    else if self.scan.starts_with(b"<!ATTLIST")  { self.parse_attlist_decl()?; }
                    else if self.scan.starts_with(b"<!ELEMENT")  { self.parse_element_decl()?; }
                    else if self.scan.starts_with(b"<!NOTATION") { self.parse_notation_decl()?; }
                    else if self.scan.starts_with(b"<?")         { self.skip_pi_raw()?; }
                    else if self.scan.starts_with(b"<![")        { self.parse_conditional_section()?; }
                    else {
                        return Err(self.scan.err(
                            "unexpected declaration in INCLUDE conditional section"
                        ));
                    }
                }
                Some(b'%') => self.expand_pe_ref_at_cursor()?,
                _ => return Err(self.scan.err(
                    "unexpected content in INCLUDE conditional section"
                )),
            }
        }
    }

    /// IGNORE-section body: discard everything up to the matching
    /// `]]>`, respecting nesting via `<![ … ]]>` pairs.  Entity
    /// streams pop transparently so the terminator can live in the
    /// outer source when the body was supplied via a PE expansion.
    fn skip_ignore_section_body(&mut self) -> Result<()> {
        let mut depth = 1usize;
        loop {
            let tail = self.scan.cur_tail();
            if tail.is_empty() {
                if self.scan.on_original_source() {
                    return Err(self.scan.err(
                        "unterminated IGNORE conditional section (expected ']]>')"
                    ));
                }
                if !self.scan.try_pop_entity_stream() {
                    return Err(self.scan.err(
                        "unterminated IGNORE conditional section (expected ']]>')"
                    ));
                }
                continue;
            }
            if tail.starts_with(b"<![") {
                depth += 1;
                self.scan.skip_n(3);
            } else if tail.starts_with(b"]]>") {
                self.scan.skip_n(3);
                depth -= 1;
                if depth == 0 { return Ok(()); }
            } else {
                self.scan.advance();
            }
        }
    }

    fn skip_pubid_literal(&mut self) -> Result<()> {
        // capture-and-discard form; we keep the parsing logic in
        // capture_pubid_literal below and just throw the result away.
        self.capture_pubid_literal().map(|_| ())
    }

    /// Same as [`skip_pubid_literal`] but returns the body bytes.
    /// Used by `parse_doctype` to preserve the PUBLIC literal so
    /// consumers reading `docinfo.public_id` get the original string.
    fn capture_pubid_literal(&mut self) -> Result<String> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected PubidLiteral")),
        };
        let tail = self.scan.cur_tail();
        let off = memchr(q, tail).ok_or_else(|| self.scan.err("unterminated PubidLiteral"))?;
        let body = &tail[..off];
        for &b in body {
            if !is_pubid_char(b) {
                return Err(self.scan.err(format!("invalid PubidChar 0x{b:02X}")));
            }
        }
        // SAFETY: every byte just validated as ASCII (pubid chars
        // are a subset of ASCII), so the slice is valid UTF-8.
        let out = unsafe { std::str::from_utf8_unchecked(body) }.to_string();
        self.scan.cur_advance_pos(off + 1);
        Ok(out)
    }

    /// Record the raw source text of the internal-subset declaration
    /// just parsed (`decl_start` .. current position) into the DTD's
    /// ordered `internal_decls`.  Skipped unless the scanner read the
    /// whole declaration directly from the original source (a
    /// parameter-entity expansion has no stable source offset).
    fn capture_internal_decl(&mut self, decl_on_src: bool, decl_start: usize) {
        if !decl_on_src || !self.scan.on_original_source() {
            return;
        }
        let end = self.scan.cur_pos();
        if end > decl_start {
            let text = self.scan.original_slice(decl_start, end);
            if !text.is_empty() {
                self.dtd.internal_decls.push(text.to_string());
            }
        }
    }

    fn parse_internal_subset(&mut self) -> Result<()> {
        loop {
            self.scan.skip_ws();
            // If we're parsing inside a PE-replacement stream and
            // it's exhausted, pop back to the parent stream and
            // continue.  Spec (§ 4.4.8) requires PE expansions to
            // contain a sequence of complete declarations, so the
            // pop point should land us between declarations cleanly.
            while self.scan.peek().is_none() && !self.scan.on_original_source() {
                if !self.scan.try_pop_entity_stream() { break; }
                self.scan.skip_ws();
            }
            match self.scan.peek() {
                None    => return Err(self.scan.err("unterminated DOCTYPE internal subset")),
                Some(b']') => { self.scan.advance(); return Ok(()); }
                Some(b'<') => {
                    // Capture each declaration's raw source span (in
                    // document order) for round-trip serialization of
                    // the internal subset.  Only declarations read
                    // directly from the source are captured.
                    let decl_on_src = self.scan.on_original_source();
                    let decl_start  = self.scan.cur_pos();
                    if      self.scan.starts_with(b"<!--")      { self.skip_comment_raw()?; }
                    else if self.scan.starts_with(b"<!ENTITY")  { self.parse_entity_decl()?;  self.capture_internal_decl(decl_on_src, decl_start); }
                    else if self.scan.starts_with(b"<!ATTLIST") { self.parse_attlist_decl()?; self.capture_internal_decl(decl_on_src, decl_start); }
                    else if self.scan.starts_with(b"<!ELEMENT") { self.parse_element_decl()?; self.capture_internal_decl(decl_on_src, decl_start); }
                    else if self.scan.starts_with(b"<!NOTATION"){ self.parse_notation_decl()?; self.capture_internal_decl(decl_on_src, decl_start); }
                    else if self.scan.starts_with(b"<![")       {
                        // XML 1.0 § 3.4 [62] [conditionalSect]: only legal
                        // in the EXTERNAL subset.  Errata-2e clarification:
                        // when a parameter-entity reference inside the
                        // internal subset expands to markup containing a
                        // conditional section, that markup is processed
                        // as if external — so conditional sections ARE
                        // valid there.  We distinguish by whether the
                        // scanner is currently reading from the original
                        // source bytes (true internal subset, forbidden)
                        // or from an entity-replacement stream (PE
                        // expansion, allowed).
                        if self.scan.on_original_source() {
                            return Err(self.scan.err(
                                "conditional sections are only allowed in the external DTD subset \
                                 (XML 1.0 § 3.4 [62])"
                            ));
                        }
                        self.parse_conditional_section()?;
                    }
                    else if self.scan.starts_with(b"<?")        { self.skip_pi_raw()?; }
                    else {
                        return Err(self.scan.err(
                            "unexpected declaration in DOCTYPE internal subset"
                        ));
                    }
                }
                Some(b'%') => {
                    // XML 1.0 § 4.4.8 [Included as PE]: a parameter-
                    // entity reference inside the internal subset
                    // expands to its replacement text.  We push the
                    // text as an entity stream so the next loop
                    // iteration parses against it — the existing
                    // declaration / PI / comment parsers naturally
                    // catch violations like "PE expanded to an XML
                    // declaration in the wrong place" (the PI parser
                    // sees target `xml` and rejects).
                    self.scan.advance();
                    let name_bytes = self.scan.scan_name_bytes()?;
                    self.scan.expect(b';')?;
                    let name_str = unsafe {
                        std::str::from_utf8_unchecked(&name_bytes)
                    };
                    // XML 1.0 errata E13: noting that a PE reference
                    // appeared inside the internal subset relaxes
                    // undeclared-general-entity errors later (those
                    // become validity errors, not WF errors, since
                    // the PE expansion could in principle declare
                    // them).  Set the flag here whether or not the
                    // PE itself is declared — the rule fires on the
                    // mere appearance of the reference.
                    self.pe_ref_in_internal_subset_seen = true;
                    let decl = match self.parameter_entities.get(name_str) {
                        Some(d) => d.clone(),
                        None => {
                            // WFC: Entity Declared carve-out — refs
                            // *inside* external entity content are
                            // exempt from the WF rule.  Non-validating
                            // parser MUST tolerate; log a recoverable
                            // warning and skip the expansion.
                            if self.scan.current_base_uri().is_some() {
                                self.recovered_errors.push(XmlError::new(
                                    ErrorDomain::Parser,
                                    ErrorLevel::Warning,
                                    format!(
                                        "undefined parameter entity '%{name_str};' \
                                         inside an external entity — WFC: Entity \
                                         Declared carve-out applies (XML 1.0 § 4.1); \
                                         expansion skipped"
                                    ),
                                ));
                                continue;
                            }
                            return Err(self.scan.err(format!(
                                "undefined parameter entity '%{name_str};' \
                                 (XML 1.0 § 4.1 WFC: Entity Declared)"
                            )));
                        }
                    };
                    let is_external = decl.kind.is_external_value();
                    let frame_base = decl.source_uri.clone();
                    let value = match decl.kind {
                        EntityKind::InternalText(v) | EntityKind::ExternalLoaded(v) => v,
                        EntityKind::ExternalUnloaded => {
                            // Declared external but the resolver
                            // didn't load it.  Skip — no replacement
                            // text means no expansion.  Per XML 1.0
                            // §4.4.3, a non-validating parser MAY
                            // include but isn't required to.
                            continue;
                        }
                    };
                    let depth = self.element_stack.len() as u32;
                    self.scan.push_entity_stream(name_str.to_string(), value, depth, frame_base)?;
                    if is_external {
                        // XML 1.0 §4.3.1: only *external* parsed
                        // entities may begin with a text declaration.
                        // Internal PE content with `<?xml ...?>` at
                        // the start is not-wf and must surface as a
                        // reserved-PI error, not be swallowed.
                        consume_text_decl_if_present(&mut self.scan, self.is_xml_11)?;
                    }
                }
                _ => return Err(self.scan.err("unexpected content in DOCTYPE internal subset")),
            }
        }
    }

    fn parse_entity_decl(&mut self) -> Result<()> {
        // Capture decl origin BEFORE consuming the keyword:
        // anything not on the original source bytes is, by spec,
        // "external" for WFC purposes (the external subset itself
        // OR a parameter-entity's replacement text).
        let declared_external = !self.scan.on_original_source();
        self.scan.skip_n(8); // "<!ENTITY"
        self.scan.expect_ws()?;
        self.scan.skip_ws();

        let is_param = self.scan.peek() == Some(b'%');
        if is_param { self.scan.advance(); self.scan.expect_ws()?; self.scan.skip_ws(); }

        // The entity HashMap is keyed by `String` — same shape as in
        // XmlReader, since entity values come from DTD parsing which
        // produces text content.  Convert the byte name we just scanned
        // via from_utf8_unchecked (Scanner invariant: bytes are UTF-8).
        let name_bytes = self.scan.scan_name_bytes()?;
        // XML Namespaces 1.0 § 3 forbids colons in entity names —
        // they're not addressable via the namespace-prefix
        // machinery and would let a doc smuggle a name that looks
        // like a QName.  Gated on `namespace_aware`.
        if self.scan.opts.namespace_aware && name_bytes.contains(&b':') {
            return Err(self.scan.err(format!(
                "entity name '{}' must be an NCName (no colon) under \
                 XML Namespaces 1.0",
                String::from_utf8_lossy(&name_bytes)
            )));
        }
        let name = unsafe { std::str::from_utf8_unchecked(&name_bytes) }.to_string();
        self.scan.expect_ws()?;
        self.scan.skip_ws();

        // External entity: `SYSTEM SystemLiteral` or `PUBLIC PubidLiteral SystemLiteral`.
        // SystemLiteral must be quoted; PubidLiteral content must be
        // PubidChar.  When an `external_resolver` is configured we
        // capture the IDs and ask the resolver for the bytes; the
        // resolved replacement text gets inserted into the entity
        // map just like an internal entity would, so subsequent
        // `&name;` references expand normally.
        let mut is_external = false;
        let mut external_public_id: Option<String> = None;
        let mut external_system_id: Option<String> = None;
        // Captured for the DTD object model (lxml's `DTD.entities()`) and
        // the DTD serializer, pushed once at the end of the declaration.
        let model_name = name.clone();
        let mut ent_orig: Option<String> = None;
        let mut ent_content: Option<String> = None;
        let mut ent_ndata: Option<String> = None;
        if self.scan.starts_with(b"SYSTEM") {
            is_external = true;
            self.scan.skip_n(6);
            self.scan.expect_ws()?;
            self.scan.skip_ws();
            if !matches!(self.scan.peek(), Some(b'"' | b'\'')) {
                return Err(self.scan.err(
                    "SYSTEM identifier must be a quoted SystemLiteral (XML 1.0 § 4.2.2 [11])"
                ));
            }
            external_system_id = Some(self.read_system_literal()?);
        } else if self.scan.starts_with(b"PUBLIC") {
            is_external = true;
            self.scan.skip_n(6);
            self.scan.expect_ws()?;
            self.scan.skip_ws();
            external_public_id = Some(self.read_pubid_literal()?);
            self.scan.expect_ws()?;
            self.scan.skip_ws();
            if !matches!(self.scan.peek(), Some(b'"' | b'\'')) {
                return Err(self.scan.err(
                    "PUBLIC requires a SystemLiteral after the PubidLiteral (XML 1.0 § 4.2.2 [75])"
                ));
            }
            external_system_id = Some(self.read_system_literal()?);
        } else {
            // Internal entity: quoted EntityValue.  XML 1.0 § 2.3 [9]
            //   EntityValue ::= '"' ([^%&"] | PEReference | Reference)* '"'
            //                 | "'" ([^%&'] | PEReference | Reference)* "'"
            // Bare `&` (not a valid reference) and bare `%` are forbidden.
            let q = match self.scan.peek() {
                Some(b @ (b'"' | b'\'')) => { self.scan.advance(); b }
                _ => return Err(self.scan.err("expected quoted entity value")),
            };
            let val_start = self.scan.cur_pos();
            while !self.scan.is_eof() && self.scan.peek() != Some(q) {
                let b = self.scan.peek().unwrap();
                // XML 1.0 § 2.8 WFC "PEs in Internal Subset":
                //   In the internal DTD subset, parameter-entity
                //   references MUST NOT occur within markup
                //   declarations.
                // The entity value being parsed here is exactly
                // such a markup declaration's content, so a `%`
                // (parameter-entity reference start) is forbidden.
                if b == b'%' && self.scan.on_original_source() {
                    return Err(self.scan.err(
                        "parameter-entity reference '%…;' inside an entity value \
                         is forbidden in the internal DTD subset \
                         (XML 1.0 § 2.8 WFC: PEs in Internal Subset)"
                    ));
                }
                if b == b'&' || b == b'%' {
                    // Must be followed by valid Reference / PEReference.
                    self.scan.advance();
                    if self.scan.peek() == Some(b'#') && b == b'&' {
                        self.scan.advance();
                        if self.scan.peek() == Some(b'x') || self.scan.peek() == Some(b'X') {
                            self.scan.advance();
                            while matches!(self.scan.peek(),
                                Some(c) if (c as char).is_ascii_hexdigit())
                            {
                                self.scan.advance();
                            }
                        } else {
                            while matches!(self.scan.peek(),
                                Some(c) if (c as char).is_ascii_digit())
                            {
                                self.scan.advance();
                            }
                        }
                        if self.scan.peek() != Some(b';') {
                            return Err(self.scan.err(
                                "invalid character reference in entity value (missing ';')"
                            ));
                        }
                        self.scan.advance();
                    } else {
                        // Named reference / PE reference.
                        self.scan.skip_name()?;
                        if self.scan.peek() != Some(b';') {
                            return Err(self.scan.err(format!(
                                "bare '{}' in entity value — must be a valid reference (XML 1.0 § 2.3 [9])",
                                b as char
                            )));
                        }
                        self.scan.advance();
                    }
                } else {
                    self.scan.advance();
                }
            }
            let value_bytes = self.scan.cur_slice(val_start, self.scan.cur_pos());
            self.scan.expect(q)?;
            // Per § 4.5 "Construction of Internal Entity Replacement
            // Text", the replacement text is the literal value
            // after expansion of character references AND
            // parameter-entity references.  General-entity
            // references stay literal (they get expanded only at
            // reference time, in the eventual including context).
            //
            // The PE refs are "Included in Literal" (§ 4.4.5) — no
            // space padding here, unlike the "Included" rule that
            // applies when a PE expands within markup declarations.
            let replacement = self.expand_entity_value(&value_bytes)
                .map_err(|msg| self.scan.err(msg))?;
            // SAFETY: replacement bytes come from valid UTF-8 input
            // bytes plus char-ref decoding (which always emits valid
            // UTF-8 via `char::encode_utf8`), so the result is also
            // valid UTF-8.
            // Why unsafe: avoids a redundant `from_utf8` validation
            // pass on a buffer we already know is valid.
            let value = unsafe { String::from_utf8_unchecked(replacement) };
            ent_orig = Some(String::from_utf8_lossy(&value_bytes).into_owned());
            ent_content = Some(value.clone());
            // Internal entity:  store in the right map by kind.
            // General entities go into `entities` (referenced as
            // `&name;`); parameter entities into `parameter_entities`
            // (referenced as `%name;` only inside the DTD).
            //
            // XML 1.0 § 4.2: "If the same entity is declared more than
            // once, the first declaration encountered is binding."
            // Use `entry().or_insert` so a second decl of the same
            // name is silently ignored — matches valid-sa-086 and the
            // libxml2 behaviour.
            let decl = EntityDecl {
                kind: EntityKind::InternalText(value),
                declared_external,
                source_uri: None,
            };
            if is_param {
                self.parameter_entities.entry(name.clone()).or_insert(decl);
            } else {
                self.entities.entry(name.clone()).or_insert(decl);
            }
        }
        // Optional NDATA annotation on external general entities
        // (forbidden on parameter entities — § 4.2 [74]).
        // XML 1.0 § 4.2.2 [76]: NDataDecl ::= S 'NDATA' S Name —
        // whitespace is REQUIRED before `NDATA`, not optional.
        let saw_ws_before_ndata = {
            let before = self.scan.cur_pos();
            self.scan.skip_ws();
            self.scan.cur_pos() != before
        };
        let mut is_unparsed = false;
        if self.scan.starts_with(b"NDATA") {
            if !saw_ws_before_ndata {
                return Err(self.scan.err(
                    "whitespace is required before `NDATA` (XML 1.0 § 4.2.2 [76])"
                ));
            }
            if is_param {
                return Err(self.scan.err(
                    "NDATA annotation is not allowed on parameter entities (XML 1.0 § 4.2 [74])"
                ));
            }
            // XML 1.0 § 4.2.2 [73] [GEDecl]:
            //   GEDecl    ::= '<!ENTITY' S Name S EntityDef S? '>'
            //   EntityDef ::= EntityValue | (ExternalID NDataDecl?)
            // NDataDecl is only legal when EntityDef is an ExternalID
            // — `<!ENTITY ge "literal" NDATA n>` is a fatal error.
            if !is_external {
                return Err(self.scan.err(
                    "NDATA is only allowed on external (SYSTEM/PUBLIC) general \
                     entities, not on internal EntityValue declarations \
                     (XML 1.0 § 4.2.2 [73])"
                ));
            }
            self.scan.skip_n(5);
            self.scan.expect_ws()?;
            self.scan.skip_ws();
            let ndata_bytes = self.scan.scan_name_bytes()?;
            ent_ndata = Some(String::from_utf8_lossy(&ndata_bytes).into_owned());
            self.scan.skip_ws();
            // Record the unparsed-entity declaration for XSLT 1.0
            // §12.4 `unparsed-entity-uri()` / `-public-id()`.  Both the
            // SYSTEM identifier (the URI a non-XML processor fetches)
            // and the optional PUBLIC identifier are kept.  First decl
            // wins (XML 1.0 §4.2 — earliest binding).
            if let Some(sys) = &external_system_id {
                self.dtd.unparsed_entities
                    .entry(name.clone())
                    .or_insert_with(|| sup_xml_tree::UnparsedEntity {
                        system_id: sys.clone(),
                        public_id: external_public_id.clone(),
                    });
            }
            is_unparsed = true;
        }
        // Track external general-entity names for `libxml2_compat`
        // mode: references to these names should silently expand to
        // empty rather than erroring "undefined entity," matching
        // libxml2's behaviour when the external file isn't loaded.
        // Parameter entities aren't tracked here (PE references are
        // a separate beast handled in the internal-subset loop).
        if is_external && !is_unparsed {
            // If a resolver is configured, ask it for the entity's
            // bytes and install them as the replacement text.  The
            // resolver is the caller's opt-in to external loading;
            // its absence means we keep the historical no-load
            // behaviour and just record the name so libxml2_compat
            // mode can silently skip references.
            //
            // Unparsed (NDATA) entities are skipped here entirely:
            // XML 1.0 §4.4.4 forbids them from appearing as general
            // entity references in content (they're addressable only
            // through `unparsed-entity-uri()` and ENTITY-typed
            // attribute values), so there's nothing for the parser
            // to load.  Asking the resolver would also be wrong —
            // the SYSTEM id points at a binary (e.g. an image).
            if !is_param && self.scan.opts.external_resolver.is_some() {
                // External *general* entity with a resolver configured:
                // do NOT fetch it now.  XML 1.0 § 4.4.3 loads a parsed
                // external general entity only when it is referenced, so
                // an unreferenced declaration must not perform I/O (and
                // eagerly fetching would be an XXE/SSRF vector).  Record
                // the identifiers; `load_deferred_entity` resolves them
                // on the first `&name;` reference in content — but only
                // when `resolve_external_entities` is set.  When it is
                // not (lxml's `resolve_entities='internal'` default), the
                // entity is left unloaded so a reference reports it
                // undefined, never inlining external content.
                if self.scan.opts.resolve_external_entities {
                    let sys = external_system_id.as_deref().unwrap_or("");
                    self.deferred_general_entities.insert(
                        name.clone(),
                        DeferredExternal {
                            system_id: sys.to_string(),
                            public_id: external_public_id.clone(),
                        },
                    );
                }
                self.entities.insert(name, EntityDecl {
                    kind: EntityKind::ExternalUnloaded,
                    declared_external,
                    source_uri: None,
                });
            } else if let Some(resolver) = self.scan.opts.external_resolver.clone()
                .filter(|_| self.scan.opts.load_external_dtd || self.scan.opts.validating)
            {
                // Reached only for external *parameter* entities (external
                // general entities are handled above).  Loading one fetches
                // an attacker-controllable SYSTEM URI as part of DTD
                // processing, so it is gated behind the same opt-in as the
                // external DTD subset (`load_external_dtd`, or `validating`)
                // — not merely the presence of a resolver.  Without the
                // opt-in the PE is left unloaded (see the parameter arm
                // below), so a `%pe;` reference is skipped rather than
                // performing I/O (XXE/SSRF, incl. blind/out-of-band XXE).
                let sys = external_system_id.as_deref().unwrap_or("");
                let pid = external_public_id.as_deref();
                // XML 1.0 § 4.2.2 + errata E18: choose the base URI
                // by entity kind.  Parameter-entity declarations
                // resolve their SYSTEM URIs against the containing
                // entity (the most-deeply-nested PE we're currently
                // reading from).  General-entity declarations
                // resolve against the *document* URL — even when
                // declared inside a deeply-nested external PE —
                // which is the unusual rule E18 fixes.  Falling back
                // to `opts.base_url` covers the "not inside any
                // entity" case for both kinds.
                let base: Option<String> = if is_param {
                    self.scan.current_base_uri()
                        .map(str::to_string)
                        .or_else(|| self.scan.opts.base_url.clone())
                } else {
                    self.scan.opts.base_url.clone()
                };
                let absolute = resolve_uri(sys, base.as_deref());
                match resolver.resolve(pid, &absolute, base.as_deref()) {
                    Ok(bytes) => {
                        // XML 1.0 §4.3.3: external parsed entities
                        // may be in any encoding the
                        // EntityResolver returned bytes for —
                        // typically UTF-8, but UTF-16 / UTF-32 /
                        // any documented encoding is legal.  Detect
                        // from the BOM / first-bytes pattern and
                        // transcode to UTF-8 (validation falls out
                        // of the transcoder).  This mirrors what
                        // the main document goes through in
                        // `parse_bytes`.
                        // `transcode_to_utf8` short-circuits the
                        // UTF-8-detected path *without* validating
                        // — the bytes are returned as-is.  For
                        // resolver-supplied input we own the
                        // validation, so re-run `from_utf8` on the
                        // result.  Two distinct error modes:
                        //   1. transcode itself failed (malformed
                        //      UTF-16 / UTF-32 / unsupported encoding)
                        //   2. transcode succeeded but the bytes
                        //      weren't valid UTF-8 (the no-BOM,
                        //      no-text-decl path)
                        let transcoded: std::result::Result<Vec<u8>, String> =
                            match crate::encoding::transcode_to_utf8(&bytes) {
                                Ok(c)  => Ok(c.into_owned()),
                                Err(e) => Err(e.message.clone()),
                            };
                        let value = transcoded.and_then(|v| {
                            String::from_utf8(v).map_err(|e| e.to_string())
                        });
                        match value {
                            Ok(v) => {
                                let decl = EntityDecl {
                                    kind: EntityKind::ExternalLoaded(v),
                                    declared_external,
                                    source_uri: Some(absolute.clone()),
                                };
                                if is_param {
                                    self.parameter_entities.insert(name, decl);
                                } else {
                                    self.entities.insert(name, decl);
                                }
                            }
                            Err(msg) => {
                                let err = self.scan.err_with_level(
                                    ErrorLevel::Error,
                                    format!(
                                        "external entity '&{name};' is not valid UTF-8 \
                                         (system_id={sys:?}): {msg}"
                                    ),
                                );
                                self.maybe_recover(err)?;
                                if !is_param {
                                    self.entities.insert(name, EntityDecl {
                                        kind: EntityKind::ExternalUnloaded,
                                        declared_external,
                                        source_uri: None,
                                    });
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Resolver said no (or failed to load).  In
                        // recover mode, log + continue so the rest
                        // of the document still parses; in strict
                        // mode, return a fatal error.
                        let err = self.scan.err_with_level(
                            ErrorLevel::Error,
                            format!(
                                "external resolver failed to load entity '&{name};' \
                                 (system_id={sys:?}, public_id={pid:?}): {e}"
                            ),
                        );
                        self.maybe_recover(err)?;
                        // Recovery: still register the name so
                        // libxml2_compat can silently skip refs.
                        if !is_param {
                            self.entities.insert(name, EntityDecl {
                                kind: EntityKind::ExternalUnloaded,
                                declared_external,
                                source_uri: None,
                            });
                        }
                    }
                }
            } else if self.scan.opts.load_external_dtd && !is_param {
                // No resolver, but external loading is opted-in via
                // `load_external_dtd` (the same XXE-security gate
                // used for DTD subset loading).  Read the file
                // pointed at by SYSTEM, resolving relative paths
                // against `base_url` when set.  General entities
                // only — parameter entities aren't expanded here.
                let sys = external_system_id.as_deref().unwrap_or("");
                // E18 rule: general-entity decls resolve against
                // the document URL even when declared inside an
                // external PE.  `current_base_uri()` is therefore
                // deliberately *not* consulted here.
                let base = self.scan.opts.base_url.clone();
                let absolute = resolve_uri(sys, base.as_deref());
                match read_external_entity_bytes(&absolute, None)
                    .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
                {
                    Ok(value) => {
                        // Replacement text is held in the entities
                        // map and replayed via push_entity_stream
                        // when a `&name;` reference is encountered.
                        // Any XML well-formedness errors in the
                        // file surface at the reference site, not
                        // here at declaration time.
                        self.entities.insert(name, EntityDecl {
                            kind: EntityKind::ExternalLoaded(value),
                            declared_external,
                            source_uri: Some(absolute),
                        });
                    }
                    Err(msg) => {
                        // File not found, unreadable, or not UTF-8.
                        // Log a recovered warning and fall back to
                        // the historical "register name, refs skip"
                        // behaviour so the rest of the document
                        // still parses.
                        self.recovered_errors.push(XmlError::new(
                            ErrorDomain::Parser,
                            ErrorLevel::Warning,
                            format!("external entity '&{name};' SYSTEM {sys:?} not loaded: {msg}"),
                        ));
                        self.entities.insert(name, EntityDecl {
                            kind: EntityKind::ExternalUnloaded,
                            declared_external,
                            source_uri: None,
                        });
                    }
                }
            } else if !is_param {
                self.entities.insert(name, EntityDecl {
                    kind: EntityKind::ExternalUnloaded,
                    declared_external,
                    source_uri: None,
                });
            } else {
                // External parameter entity left unloaded (no resolver, or
                // external DTD loading not opted into).  Record it so a
                // later `%pe;` reference is silently skipped rather than
                // reported undefined — matching a libxml2 parser that does
                // not read external parameter entities by default.
                self.parameter_entities.insert(name, EntityDecl {
                    kind: EntityKind::ExternalUnloaded,
                    declared_external,
                    source_uri: None,
                });
            }
        }
        // Record the general/parameter entity for the DTD object model
        // and serializer, in source order; first declaration wins.
        if !self.dtd.entities.iter().any(|e| e.name == model_name && e.parameter == is_param) {
            let idx = self.dtd.entities.len();
            self.dtd.entities.push(crate::dtd::model::EntityDecl {
                name:      model_name,
                parameter: is_param,
                orig:      ent_orig,
                content:   ent_content,
                system_id: external_system_id.clone(),
                public_id: external_public_id.clone(),
                ndata:     ent_ndata,
            });
            self.dtd.decl_order.push(crate::dtd::model::DeclRef::Entity(idx));
        }
        self.scan.expect(b'>')
    }

    /// XML 1.0 § 3.3 [52] [AttlistDecl]:
    ///   '<!ATTLIST' S Name AttDef* S? '>'
    ///   AttDef     ::= S Name S AttType S DefaultDecl
    ///   AttType    ::= StringType | TokenizedType | EnumeratedType
    ///   StringType ::= 'CDATA'
    ///   TokenizedType ::= 'ID'|'IDREF'|'IDREFS'|'ENTITY'|'ENTITIES'|'NMTOKEN'|'NMTOKENS'
    ///   EnumeratedType ::= NotationType | Enumeration
    ///   NotationType ::= 'NOTATION' S '(' S? Name (S? '|' S? Name)* S? ')'
    ///   Enumeration  ::= '(' S? Nmtoken (S? '|' S? Nmtoken)* S? ')'
    ///   DefaultDecl  ::= '#REQUIRED' | '#IMPLIED' | (('#FIXED' S)? AttValue)
    fn parse_attlist_decl(&mut self) -> Result<()> {
        use crate::dtd::AttDecl;

        self.scan.skip_n(9); // "<!ATTLIST"
        self.expect_ws_with_pe()?;
        let elem_name = self.scan.scan_name()?.into_owned();
        // Attribute definitions — there may be zero or many.
        let mut decls: Vec<AttDecl> = Vec::new();
        loop {
            self.skip_ws_and_pe_refs()?;
            if self.scan.peek() == Some(b'>') {
                self.scan.advance();
                if !decls.is_empty() {
                    self.dtd.add_attlist(elem_name, decls);
                }
                return Ok(());
            }
            let attr_name = self.scan.scan_name()?.into_owned();
            self.expect_ws_with_pe()?;
            let att_type = self.parse_att_type()?;
            self.expect_ws_with_pe()?;
            let default = self.parse_att_default()?;
            decls.push(AttDecl { name: attr_name, att_type, default });
        }
    }

    /// One AttType per § 3.3.1.  Rejects the SGML-only types
    /// NUTOKEN / NUTOKENS / NUMBER / NUMBERS / NAME / NAMES that
    /// XML 1.0 explicitly forbids.
    fn parse_att_type(&mut self) -> Result<crate::dtd::AttType> {
        use crate::dtd::AttType;

        // Enumerated types start with `(` (or `NOTATION`).
        if self.scan.peek() == Some(b'(') {
            self.scan.advance();
            let mut values: Vec<String> = Vec::new();
            loop {
                self.scan.skip_ws();
                // Nmtoken = (NameChar)+ — XML 1.0 § 2.3 [7].  Unlike
                // Name it has no NameStartChar restriction, so
                // `(0|35a|...)` is valid in an attribute-enum decl.
                values.push(self.scan.scan_nmtoken()?.into_owned());
                self.scan.skip_ws();
                match self.scan.peek() {
                    Some(b')') => { self.scan.advance(); return Ok(AttType::Enumeration(values)); }
                    Some(b'|') => { self.scan.advance(); }
                    _ => return Err(self.scan.err(
                        "invalid enumerated attribute type — expected `|` or `)` (XML 1.0 § 3.3.1 [59])"
                    )),
                }
            }
        }
        // Otherwise it must be one of the named types.  Scan a Name
        // and check it against the allowed set.
        let kw = self.scan.scan_name_bytes()?;
        match &kw[..] {
            b"CDATA"    => Ok(AttType::CData),
            b"ID"       => Ok(AttType::Id),
            b"IDREF"    => Ok(AttType::IdRef),
            b"IDREFS"   => Ok(AttType::IdRefs),
            b"ENTITY"   => Ok(AttType::Entity),
            b"ENTITIES" => Ok(AttType::Entities),
            b"NMTOKEN"  => Ok(AttType::Nmtoken),
            b"NMTOKENS" => Ok(AttType::Nmtokens),
            b"NOTATION" => {
                self.scan.expect_ws()?;
                self.scan.skip_ws();
                self.scan.expect(b'(')?;
                let mut values: Vec<String> = Vec::new();
                loop {
                    self.scan.skip_ws();
                    values.push(self.scan.scan_name()?.into_owned());
                    self.scan.skip_ws();
                    match self.scan.peek() {
                        Some(b')') => { self.scan.advance(); return Ok(AttType::Notation(values)); }
                        Some(b'|') => { self.scan.advance(); }
                        _ => return Err(self.scan.err(
                            "invalid NOTATION enumeration — expected `|` or `)`"
                        )),
                    }
                }
            }
            other => Err(self.scan.err(format!(
                "invalid attribute type '{}' — not allowed in XML 1.0 (SGML-only or unknown); \
                 valid types are CDATA, ID, IDREF, IDREFS, ENTITY, ENTITIES, NMTOKEN, NMTOKENS, \
                 NOTATION, or `(enum)` (XML 1.0 § 3.3.1)",
                String::from_utf8_lossy(other)
            ))),
        }
    }

    /// One DefaultDecl per § 3.3.2.  Rejects SGML-only `#CURRENT`
    /// and `#CONREF`.  When the default is a literal value
    /// (`#FIXED "..."` or just `"..."`), validates the value as if
    /// it were a document-body attribute value: no bare `<`, no bare
    /// `&`, entity refs must be defined and non-recursive, and
    /// external entity refs are forbidden (§ 4.4.4).
    fn parse_att_default(&mut self) -> Result<crate::dtd::AttDefault> {
        use crate::dtd::AttDefault;

        if self.scan.peek() == Some(b'#') {
            self.scan.advance();
            let kw = self.scan.scan_name_bytes()?;
            match &kw[..] {
                b"REQUIRED" => Ok(AttDefault::Required),
                b"IMPLIED"  => Ok(AttDefault::Implied),
                b"FIXED" => {
                    self.scan.expect_ws()?;
                    self.scan.skip_ws();
                    let v = self.validate_att_default_value()?;
                    Ok(AttDefault::Fixed(v))
                }
                other => Err(self.scan.err(format!(
                    "invalid attribute default '#{}' — must be #REQUIRED, #IMPLIED, or #FIXED (XML 1.0 § 3.3.2 [60])",
                    String::from_utf8_lossy(other)
                ))),
            }
        } else {
            let v = self.validate_att_default_value()?;
            Ok(AttDefault::Default(v))
        }
    }

    /// Read a quoted ATTLIST default value and run the same syntactic
    /// validation as document-body attribute values:
    /// XML 1.0 § 3.1 / § 4.1 / § 4.4.4 — no `<`, no bare `&`, no
    /// external/cyclic/undefined entity references.
    fn validate_att_default_value(&mut self) -> Result<String> {
        let q = match self.scan.advance() {
            Some(b @ (b'"' | b'\'')) => b,
            _ => return Err(self.scan.err("expected quoted ATTLIST default value")),
        };
        let val_start = self.scan.cur_pos();
        match memchr(q, self.scan.cur_tail()) {
            None => Err(self.scan.err("unterminated ATTLIST default value")),
            Some(off) => {
                let val_end = val_start + off;
                // Synthesise an `attr="value"` slice so we can reuse
                // validate_attrs_syntax.  The leading name is a dummy.
                let value_bytes = self.scan.cur_slice(val_start, val_end).into_owned();
                let mut buf: Vec<u8> = b"_=".to_vec();
                buf.push(q);
                buf.extend_from_slice(&value_bytes);
                buf.push(q);
                let inside_external = self.scan.current_base_uri().is_some();
                validate_attrs_syntax(&buf, &self.scan.opts, &self.entities, inside_external)
                    .map_err(|msg| self.scan.err(msg))?;
                self.scan.cur_set_pos(val_end + 1);
                Ok(String::from_utf8_lossy(&value_bytes).into_owned())
            }
        }
    }

    /// XML 1.0 § 3.2 [45] [elementdecl]:
    ///   '<!ELEMENT' S Name S contentspec S? '>'
    ///   contentspec ::= 'EMPTY' | 'ANY' | Mixed | children
    /// We validate the keyword EMPTY/ANY directly; for Mixed and
    /// children we run a balanced-paren check that verifies the
    /// shape without building the full AST.
    fn parse_element_decl(&mut self) -> Result<()> {
        use crate::dtd::{ContentModel, ElementDecl};

        self.scan.skip_n(9); // "<!ELEMENT"
        self.expect_ws_with_pe()?;
        let name = self.scan.scan_name()?.into_owned();
        self.expect_ws_with_pe()?;
        // contentspec
        let content = match self.scan.peek() {
            Some(b'(') => self.parse_content_model()?,
            Some(_) => {
                let kw = self.scan.scan_name_bytes()?;
                match &kw[..] {
                    b"EMPTY" => ContentModel::Empty,
                    b"ANY"   => ContentModel::Any,
                    other => return Err(self.scan.err(format!(
                        "invalid content model '{}' — must be EMPTY, ANY, or `(...)` (XML 1.0 § 3.2 [46])",
                        String::from_utf8_lossy(other)
                    ))),
                }
            }
            None => return Err(self.scan.err("unterminated <!ELEMENT> declaration")),
        };
        self.skip_ws_and_pe_refs()?;
        self.scan.expect(b'>')?;
        self.dtd.add_element(ElementDecl { name, content });
        Ok(())
    }

    /// Parse a parenthesised content model: either Mixed (starts with
    /// `(#PCDATA`) or children (Names with `,`/`|` separators).
    fn parse_content_model(&mut self) -> Result<crate::dtd::ContentModel> {
        use crate::dtd::{ContentModel, Group, GroupKind, Occurrence};

        self.scan.expect(b'(')?;
        self.skip_ws_and_pe_refs()?;
        // Mixed: `(#PCDATA (| Name)* )*` or `(#PCDATA)`.
        if self.scan.starts_with(b"#PCDATA") {
            self.scan.skip_n(b"#PCDATA".len());
            let mut choices: Vec<String> = Vec::new();
            loop {
                self.skip_ws_and_pe_refs()?;
                match self.scan.peek() {
                    Some(b')') => {
                        self.scan.advance();
                        // XML 1.0 § 3.2.2 [51] Mixed has TWO shapes:
                        //   '(' S? '#PCDATA' (S? '|' S? Name)* S? ')*'
                        //   '(' S? '#PCDATA' S? ')'
                        // The first form (with alternatives) REQUIRES
                        // the trailing `*`; the bare `(#PCDATA)` form
                        // forbids any quantifier.
                        if choices.is_empty() {
                            // `(#PCDATA)` — no `*` permitted.
                            if self.scan.peek() == Some(b'*') {
                                self.scan.advance();
                            }
                        } else {
                            // `(#PCDATA | name | ...)` — `*` required.
                            if self.scan.peek() != Some(b'*') {
                                return Err(self.scan.err(
                                    "Mixed content model with alternatives must end \
                                     with `)*` (XML 1.0 § 3.2.2 [51])"
                                ));
                            }
                            self.scan.advance();
                        }
                        return Ok(ContentModel::Mixed { choices });
                    }
                    Some(b'|') => {
                        self.scan.advance();
                        self.skip_ws_and_pe_refs()?;
                        choices.push(self.scan.scan_name()?.into_owned());
                    }
                    _ => return Err(self.scan.err(
                        "invalid mixed content model — expected `|` or `)` (XML 1.0 § 3.2.2 [51])"
                    )),
                }
            }
        }
        // children: cp ((','|'|') cp)* — recursive paren structure.
        let mut items: Vec<crate::dtd::Particle> = Vec::new();
        items.push(self.parse_cp(0)?);
        // Determine separator on first occurrence (must stay consistent).
        let sep = {
            self.skip_ws_and_pe_refs()?;
            match self.scan.peek() {
                Some(b')') => 0u8,
                Some(b @ (b',' | b'|')) => b,
                _ => return Err(self.scan.err(
                    "invalid children content model — expected `,`, `|`, or `)` (XML 1.0 § 3.2.1 [49/50])"
                )),
            }
        };
        if sep != 0 {
            loop {
                self.scan.advance(); // consume separator
                self.skip_ws_and_pe_refs()?;
                items.push(self.parse_cp(0)?);
                self.skip_ws_and_pe_refs()?;
                match self.scan.peek() {
                    Some(b')') => break,
                    Some(b) if b == sep => continue,
                    Some(b) => return Err(self.scan.err(format!(
                        "inconsistent separator in children content model — \
                         got `{}`, expected `{}` or `)` (XML 1.0 § 3.2.1)",
                        b as char, sep as char
                    ))),
                    None => return Err(self.scan.err("unterminated content model")),
                }
            }
        }
        self.scan.expect(b')')?;
        // Trailing quantifier `?`/`*`/`+` allowed.
        let outer_occ = read_occurrence(&mut self.scan);
        let kind = match sep {
            b'|' => GroupKind::Choice,
            // 0 (single item) or `,` → sequence.
            _    => GroupKind::Sequence,
        };
        Ok(ContentModel::Children(Group {
            kind,
            items,
            occur: outer_occ.unwrap_or(Occurrence::One),
        }))
    }

    /// One [cp] — child production: Name | choice | seq, with an
    /// optional trailing `?` / `*` / `+`.  Recurses into nested
    /// parens.  Inside a nested group, `#PCDATA` is forbidden —
    /// mixed content is only legal at the outermost level
    /// (XML 1.0 § 3.2.2 [51]).  `parse_content_model_inner` enforces this.
    fn parse_cp(&mut self, depth: u32) -> Result<crate::dtd::Particle> {
        use crate::dtd::{Item, Occurrence, Particle};

        let item = if self.scan.peek() == Some(b'(') {
            Item::Group(Box::new(self.parse_content_model_inner(depth + 1)?))
        } else {
            Item::Name(self.scan.scan_name()?.into_owned())
        };
        let occur = read_occurrence(&mut self.scan).unwrap_or(Occurrence::One);
        Ok(Particle { item, occur })
    }

    /// Parse a parenthesised group nested inside a content model.
    /// This is the same as `parse_content_model` *except* that
    /// `#PCDATA` is forbidden — XML 1.0 § 3.2.2 [51] [Mixed]
    /// requires `#PCDATA` to appear only at the outermost level
    /// (`(#PCDATA | Name | …)*`), never inside a nested group.
    /// Catches `<!ELEMENT doc ((#PCDATA))>`.
    fn parse_content_model_inner(&mut self, depth: u32) -> Result<crate::dtd::Group> {
        use crate::dtd::{Group, GroupKind, Occurrence, Particle};

        if depth > MAX_CONTENT_MODEL_DEPTH {
            return Err(self.scan.err(format!(
                "content model nesting depth exceeds limit ({MAX_CONTENT_MODEL_DEPTH})"
            )));
        }
        self.scan.expect(b'(')?;
        self.skip_ws_and_pe_refs()?;
        if self.scan.starts_with(b"#PCDATA") {
            return Err(self.scan.err(
                "#PCDATA is only allowed at the top level of a content model, \
                 not inside a nested group (XML 1.0 § 3.2.2 [51])"
            ));
        }
        let mut items: Vec<Particle> = Vec::new();
        items.push(self.parse_cp(depth)?);
        let sep = {
            self.skip_ws_and_pe_refs()?;
            match self.scan.peek() {
                Some(b')') => 0u8,
                Some(b @ (b',' | b'|')) => b,
                _ => return Err(self.scan.err(
                    "invalid children content model — expected `,`, `|`, or `)`"
                )),
            }
        };
        if sep != 0 {
            loop {
                self.scan.advance();
                self.skip_ws_and_pe_refs()?;
                items.push(self.parse_cp(depth)?);
                self.skip_ws_and_pe_refs()?;
                match self.scan.peek() {
                    Some(b')') => break,
                    Some(b) if b == sep => continue,
                    Some(b) => return Err(self.scan.err(format!(
                        "inconsistent separator in content model — got `{}`, expected `{}` or `)`",
                        b as char, sep as char
                    ))),
                    None => return Err(self.scan.err("unterminated content model")),
                }
            }
        }
        self.scan.expect(b')')?;
        let occur = read_occurrence(&mut self.scan).unwrap_or(Occurrence::One);
        let kind = if sep == b'|' { GroupKind::Choice } else { GroupKind::Sequence };
        Ok(Group { kind, items, occur })
    }

    /// XML 1.0 § 4.7 [82] [NotationDecl]:
    ///   '<!NOTATION' S Name S (ExternalID | PublicID) S? '>'
    fn parse_notation_decl(&mut self) -> Result<()> {
        self.scan.skip_n(10); // "<!NOTATION"
        self.expect_ws_with_pe()?;
        // Capture the notation name so we can enforce the NCName
        // constraint (no colons) when namespace-aware.
        let notation_name = self.scan.scan_name_bytes()?;
        if self.scan.opts.namespace_aware && notation_name.contains(&b':') {
            return Err(self.scan.err(format!(
                "notation name '{}' must be an NCName (no colon) under \
                 XML Namespaces 1.0",
                String::from_utf8_lossy(&notation_name)
            )));
        }
        self.expect_ws_with_pe()?;
        // External / Public ID — let the existing pubid-validating
        // helper handle it loosely.  We just need to consume the
        // declaration.
        if self.scan.starts_with(b"PUBLIC") {
            self.scan.skip_n(6);
            self.expect_ws_with_pe()?;
            self.skip_pubid_literal()?;
            self.skip_ws_and_pe_refs()?;
            // Optional system literal (PublicID has no system; ExternalID has it).
            if matches!(self.scan.peek(), Some(b'"' | b'\'')) {
                self.skip_quoted()?;
                self.skip_ws_and_pe_refs()?;
            }
        } else if self.scan.starts_with(b"SYSTEM") {
            self.scan.skip_n(6);
            self.expect_ws_with_pe()?;
            self.skip_quoted()?;
            self.skip_ws_and_pe_refs()?;
        } else {
            return Err(self.scan.err(
                "<!NOTATION> requires PUBLIC or SYSTEM (XML 1.0 § 4.7 [82])"
            ));
        }
        self.scan.expect(b'>')
    }

    // ── event readers ─────────────────────────────────────────────────────────

    fn read_start_element(&mut self) -> Result<BytesEvent<'_, 'src>> {
        // Depth tracks unconditionally — `next()` consults it to decide
        // whether leading whitespace is content (depth > 0) or
        // ignorable inter-document space (depth == 0).  Tying depth to
        // `skip_end_tag_check` was a bug: turning that flag on used to
        // accidentally swallow every inter-element whitespace text
        // event because depth never left 0.
        self.depth += 1;
        if self.depth > self.scan.opts.max_element_depth {
            self.depth -= 1;
            return Err(self.scan.err(format!("element nesting too deep (limit {})", self.scan.opts.max_element_depth)));
        }
        // Note that we have entered a root element.  See `root_seen`
        // field docs and `dispatch_start_element` for what this
        // gates against (XML 1.0 § 2.1 [document] — exactly one
        // root).
        if self.depth == 1 {
            self.root_seen = true;
        }
        let track_stack = !self.scan.opts.skip_end_tag_check;

        // Skip the `<`.  Our dispatcher (`next()` → `dispatch_start_element()`)
        // is only entered when `bytes[cur_pos] == b'<'` was just verified,
        // so the byte is known to be present at `cur_pos`.  Replacing the
        // safe `expect(b'<')` with a direct advance saves the bounds check
        // + comparison + Result construction at every start tag (~5M times
        // on swiss_prot; expect was 5.7% of profile self time).
        self.scan.cur_advance_pos(1);
        // Lazy: record the name's source byte range instead of
        // extracting it.  scan_name_raw advances the scanner past the
        // name (same work as scan_name_bytes minus the slice copy).
        let (name_start_us, name_end_us) = self.scan.scan_name_raw()?;
        let name_start = name_start_us as u32;
        let name_end   = name_end_us   as u32;
        let start_stream_depth = self.scan.stream_depth();
        if track_stack {
            // Common case: scanner is on the original source — record
            // the byte range so end-tag matching can compare slices
            // without allocating.  When the start tag is being read
            // from inside an entity-replacement stream, those offsets
            // refer to entity-stream bytes that aren't reachable via
            // `src_bytes()` — eagerly own the name instead.
            // Three cases for which storage to use:
            //   1. `stream_owned_names` is set — the streaming
            //      wrapper rolls the source buffer between events,
            //      so any byte range we captured here would point
            //      at stale bytes by end-tag time.  Must own.
            //   2. Reading from an entity-replacement stream — its
            //      bytes don't live in `src_bytes()`, so byte
            //      offsets can't be resolved at end-tag time.
            //      Must own.
            //   3. Otherwise (the common slurped path): the source
            //      bytes are pinned for the reader's lifetime, so
            //      we can store a byte range and skip the alloc.
            let entry = if !self.scan.opts.stream_owned_names && self.scan.on_original_source() {
                ElementStackEntry::SourceRange(name_start, name_end)
            } else {
                // SAFETY: scan_name_raw advanced over a valid XML
                // Name; those bytes are valid UTF-8 by the same
                // invariant that lets `src_bytes()` be treated as
                // UTF-8 elsewhere in the parser.
                let bytes = &self.scan.cur_bytes()[name_start as usize..name_end as usize];
                ElementStackEntry::Owned(
                    unsafe { std::str::from_utf8_unchecked(bytes) }.to_string()
                )
            };
            // The element being opened is a child of the current top —
            // mark the parent as having an element child before pushing
            // this frame (which starts with none of its own).
            if let Some(parent) = self.frame_saw_child.last_mut() {
                *parent = true;
            }
            self.element_stack.push(entry);
            self.element_streams.push(start_stream_depth);
            self.frame_saw_child.push(false);
        }

        // Find the end of the start tag.  Two-tier strategy:
        //
        // **Optimistic fast path (the common case):**  XML allows `>`
        // inside attribute values literally (only `<` and `&` MUST be
        // escaped per XML 1.0 § 3.1) — but in practice nearly every
        // real-world document avoids it.  We `memchr` for the first
        // `>`, then verify both quote characters appear in balanced
        // pairs before it.  If yes, that `>` is the tag end.  Two
        // SIMD-fast `memchr_iter` counts cost much less than the
        // per-attribute `memchr3 + memchr` loop the slow path uses.
        //
        // **Conservative fallback:**  If a quote count comes out odd,
        // some `>` lives inside a quoted attribute value.  Reset to
        // the original position and run the quote-aware scan to be
        // safe.
        let attrs_start = self.scan.cur_pos();
        let (attrs_end, self_closing) = 'scan: {
            // Fast path.
            let tail = self.scan.cur_tail();
            match memchr(b'>', tail) {
                None => return Err(self.scan.err("unterminated start tag")),
                Some(off) => {
                    // Quote parity check.  We need `dq % 2 == 0 && sq % 2 == 0`
                    // — i.e. neither quote-kind has an unmatched occurrence
                    // before this `>`.  Counting both kinds in one
                    // memchr2_iter pass is ~half the SIMD work of two
                    // separate memchr_iter sweeps; for tags with no
                    // quoted-value `>` (the common case in real XML), the
                    // body runs only the attrs' quote bytes.
                    let prefix = &tail[..off];
                    let mut dq = 0u32;
                    let mut sq = 0u32;
                    for off2 in memchr::memchr2_iter(b'"', b'\'', prefix) {
                        // SAFETY: memchr2_iter only yields valid offsets.
                        match unsafe { *prefix.get_unchecked(off2) } {
                            b'"'  => dq += 1,
                            _     => sq += 1,
                        }
                    }
                    if (dq | sq) & 1 == 0 {
                        // No quoted region could span this `>`.
                        self.scan.cur_advance_pos(off);
                        let self_closing = self.scan.cur_pos() > attrs_start
                            && self.scan.cur_bytes()[self.scan.cur_pos() - 1] == b'/';
                        let attrs_end = if self_closing { self.scan.cur_pos() - 1 } else { self.scan.cur_pos() };
                        self.scan.cur_advance_pos(1);
                        break 'scan (attrs_end, self_closing);
                    }
                    // Fall through to the conservative scan.
                }
            }

            // Slow path (quote-aware).  Quote-balance check above
            // failed, so some `>` lives inside a quoted value.  Walk
            // attribute by attribute to find the real tag end.  Cursor
            // is still at `attrs_start` (we never advanced).
            loop {
                let tail = self.scan.cur_tail();
                match memchr3(b'>', b'\'', b'"', tail) {
                    None => return Err(self.scan.err("unterminated start tag")),
                    Some(off) => {
                        self.scan.cur_advance_pos(off);
                        match self.scan.cur_bytes()[self.scan.cur_pos()] {
                            b'>' => {
                                let self_closing = self.scan.cur_pos() > attrs_start
                                    && self.scan.cur_bytes()[self.scan.cur_pos() - 1] == b'/';
                                let attrs_end = if self_closing { self.scan.cur_pos() - 1 } else { self.scan.cur_pos() };
                                self.scan.cur_advance_pos(1);
                                break 'scan (attrs_end, self_closing);
                            }
                            quote @ (b'\'' | b'"') => {
                                self.scan.cur_advance_pos(1);
                                let inside = self.scan.cur_tail();
                                match memchr(quote, inside) {
                                    None => return Err(self.scan.err("unterminated attribute value")),
                                    Some(off2) => self.scan.cur_advance_pos(off2 + 1),
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                }
            }
        };

        // Eagerly validate attribute syntax for spec compliance.
        // The lazy `BytesAttrs` iterator only checks attributes when
        // the user actually iterates them — but XML 1.0 well-
        // formedness requires us to reject malformed attributes
        // regardless of whether the application reads them.  We do a
        // syntactic-only sweep here that errors on:
        //   - duplicate attribute names                 (§ 3.1 WFC: Unique Att Spec)
        //   - unquoted values                           (§ 3.1 [41] [AttValue])
        //   - bare `<` or bare `&` inside values        (§ 3.1 + § 4.1 WFC)
        //   - invalid name-start chars on attr names    (§ 2.3 [4])
        // The user can still iterate the attributes lazily
        // afterward — they'll re-walk the same bytes (now known
        // valid) without paying validation again.
        // Eager attribute validation.  Two-tier strategy keeps the
        // common case cheap:
        //
        //   1. Cheap global scan: a single `memchr2` over the whole
        //      attrs slice for `<` and `&`.  `<` is forbidden in
        //      attribute values regardless of position, so finding
        //      it before the closing quote → error here.  `&` means
        //      the slice contains entity references that need the
        //      deep walk; absent it, we can take the fast path.
        //
        //   2. Fast path (no `&`, no `<`): a smaller validator that
        //      only checks attribute *structure* — names, `=`,
        //      quotes, whitespace between attrs, duplicates — at
        //      a fraction of the per-attr cost of the full walk.
        //      Covers the vast majority of real-world documents:
        //      data-XML (OSM, RSS, SOAP, configs) almost never uses
        //      entity refs in attributes.
        //
        //   3. Full path (entity refs present): the existing
        //      `validate_attrs_syntax` walks each attribute and
        //      validates entity references against the document's
        //      entity table.
        //
        // Empty / whitespace-only attrs slices skip everything —
        // no-attribute start tags are extremely common.
        //
        // The attrs byte range lives in whichever buffer the
        // scanner is reading from — `src_bytes()` for the common
        // case, an entity replacement stream when we're inside one.
        // Use `cur_bytes()` so an entity-stream start tag's attrs
        // get validated against the right buffer (XML 1.0 § 4.4.8 —
        // entity replacement text may contain start tags).
        let cur_for_validate = self.scan.cur_bytes();
        let attrs_slice = &cur_for_validate[attrs_start..attrs_end];
        if !self.scan.opts.skip_attr_validation
            && !attrs_slice.is_empty()
            && !attrs_slice.iter().all(|&b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            // Tier 1: any `<` in the slice is fatal regardless of
            // position (it's forbidden in attribute values *and*
            // cannot appear between attributes either — the start
            // tag would have terminated at it).
            if let Some(off) = memchr(b'<', attrs_slice) {
                return Err(self.scan.err(format!(
                    "'<' not allowed inside start tag (XML 1.0 § 3.1) at attrs offset {off}"
                )));
            }
            // Tier 2 vs 3.  A clean (no-`&`) attrs slice goes
            // through the structure-only fast path; otherwise the
            // full validator handles entity refs.
            if memchr(b'&', attrs_slice).is_none() {
                validate_attrs_structure_fast(attrs_slice, &self.scan.opts)
                    .map_err(|msg| self.scan.err(msg))?;
            } else {
                let inside_external = self.scan.current_base_uri().is_some();
                validate_attrs_syntax(
                    attrs_slice,
                    &self.scan.opts,
                    &self.entities,
                    inside_external,
                ).map_err(|msg| self.scan.err(msg))?;
            }
        }

        if self_closing {
            // Depth always decrements (matching the unconditional ++
            // at function entry).  Element-stack maintenance is gated
            // on track_stack since it's only useful for end-tag
            // matching.
            self.depth -= 1;
            if track_stack {
                self.element_stack.pop();
                self.element_streams.pop();
                self.frame_saw_child.pop();
            }
            self.state = NextState::PendingEnd(name_start, name_end);
        }

        // BytesStartTag borrows from `src` for the common (hot) path.
        // When the start tag is parsed inside an entity-replacement
        // stream, `name_start` / `name_end` index into the entity
        // stream's bytes, not `src`, so we capture the name into
        // `owned_name` and zero out the source offsets (they're
        // meaningless in this branch).  Attrs are likewise unreachable
        // from `src`, so we eagerly parse them here against a copy of
        // the entity-stream slice and stash the pairs on the start
        // tag — see [`BytesStartTag::entity_attrs`].
        let src = self.scan.src_bytes();
        let (out_name_start, out_name_end, out_attrs_start, out_attrs_end, owned_name, entity_attrs) =
            if self.scan.on_original_source() {
                (name_start, name_end, attrs_start as u32, attrs_end as u32, None, None)
            } else {
                let name_bytes = &self.scan.cur_bytes()[name_start as usize..name_end as usize];
                let owned_name = Some(Box::<[u8]>::from(name_bytes));
                // Copy the entity-stream attrs slice into an owned
                // buffer, then run the regular attribute scanner over
                // it.  The pairs collected here outlive the entity
                // frame because we own them.
                let attrs_owned: Vec<u8> = self.scan.cur_bytes()
                    [attrs_start..attrs_end].to_vec();
                let parsed: Option<Vec<(Vec<u8>, Vec<u8>)>> = if attrs_owned.is_empty() {
                    None
                } else {
                    let mut tmp = BytesAttrs {
                        scan: Scanner::new(&attrs_owned, Cow::Borrowed(&self.scan.opts)),
                        entities:        &self.entities,
                        expansion_bytes: &mut self.expansion_bytes,
                        done:            false,
                        standalone_yes:  self.standalone_yes,
                        is_xml_11:       self.is_xml_11,
                    };
                    let mut acc: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                    while let Some(item) = tmp.next() {
                        let a = item?;
                        acc.push((a.name.to_vec(), a.value.into_owned()));
                    }
                    if acc.is_empty() { None } else { Some(acc) }
                };
                (0, 0, 0, 0, owned_name, parsed)
            };
        // Record the `<` offset for downstream diagnostics (XSD validator
        // pinning issues to the source line of the offending element).
        // Entity-stream start tags carry no meaningful source offset —
        // signal that case with u32::MAX so the validator can fall back.
        self.last_start_offset = if self.scan.on_original_source() {
            name_start.saturating_sub(1)
        } else {
            u32::MAX
        };

        // Eagerly drive the attribute scanner before emitting the event.
        //
        // Attribute iteration (`BytesStartTag::attrs`) is lazy by design —
        // a SAX-style consumer that only cares about element names should
        // pay no per-attr cost.  But laziness means a SAX consumer that
        // never asks for attrs can step right over a malformed attribute
        // value (a char ref to a non-`Char` codepoint, a standalone=yes
        // entity-declared violation, etc.) and emit `Eof` as if the
        // document were well-formed.  XML 1.0 § 4.1 makes those
        // violations not-WF, not "the consumer's problem to discover."
        //
        // Walking BytesAttrs to completion here forces the same checks
        // that the DOM `parse_bytes` path naturally runs.  Cost: a
        // memchr3 SIMD pass over the attribute byte range per element,
        // plus per-attribute decode work that's identical to what a DOM
        // consumer pays anyway.  Hot-path text scanning is untouched.
        if out_attrs_end > out_attrs_start {
            let attrs_slice = &src[out_attrs_start as usize..out_attrs_end as usize];
            let mut tmp_attrs = BytesAttrs {
                scan: Scanner::new(attrs_slice, Cow::Borrowed(&self.scan.opts)),
                entities:        &self.entities,
                expansion_bytes: &mut self.expansion_bytes,
                done:            false,
                standalone_yes:  self.standalone_yes,
                is_xml_11:       self.is_xml_11,
            };
            while let Some(item) = tmp_attrs.next() {
                item?;
            }
        }

        Ok(BytesEvent::StartElement(BytesStartTag {
            src,
            name_start:  out_name_start,
            name_end:    out_name_end,
            owned_name,
            entity_attrs,
            attrs_start: out_attrs_start,
            attrs_end:   out_attrs_end,
            entities:        &self.entities,
            expansion_bytes: &mut self.expansion_bytes,
            opts:            &self.scan.opts,
            standalone_yes:  self.standalone_yes,
            is_xml_11:       self.is_xml_11,
        }))
    }

    fn read_end_element(&mut self) -> Result<BytesEvent<'_, 'src>> {
        // Save the cursor before any consumption — used by recovery
        // for "mismatched end tag" to rewind so the unread `</name>`
        // is processed again after a synthetic close-out of the
        // intermediate elements.
        let rewind_pos = self.scan.cur_pos();

        // XML 1.0 § 4.3.2 WFC "Logical Structure": the end tag must be in
        // the same input stream as the start tag of the element it closes.
        if !self.scan.opts.skip_end_tag_check {
            if let Some(&start_depth) = self.element_streams.last() {
                let now = self.scan.stream_depth();
                if now != start_depth {
                    return Err(self.scan.err(
                        "end tag is in a different entity than its start tag — \
                         entity replacement must contain matched element pairs \
                         (XML 1.0 § 4.3.2 WFC 'Logical Structure')",
                    ));
                }
            }
        }
        // Skip `</`.  Our dispatcher entered `read_end_element` only after
        // verifying `bytes[cur_pos] == b'<'` and `bytes[cur_pos+1] == b'/'`,
        // so both bytes are known present at cur_pos and cur_pos+1.
        // Skips the two `expect(...)` calls and their bounds checks +
        // comparison + Result construction (~5M times on swiss_prot).
        self.scan.cur_advance_pos(2);
        let (name_start_us, name_end_us) = self.scan.scan_name_raw()?;
        let name_start = name_start_us as u32;
        let name_end   = name_end_us   as u32;
        self.scan.skip_ws();
        self.scan.expect(b'>')?;

        // Depth always decrements (matches the unconditional ++ in
        // read_start_element).  saturating_sub guards against the edge
        // case of a stray end tag with skip_end_tag_check on (rare;
        // would otherwise underflow).
        self.depth = self.depth.saturating_sub(1);

        if !self.scan.opts.skip_end_tag_check {
            // `scan_name_raw` returns offsets into whichever input
            // stream the scanner is currently reading from — that's
            // either the original source OR an active entity-stream
            // frame.  Slice from `cur_bytes()` so we read the right
            // buffer in both cases.  XML 1.0 § 4.3.2 already requires
            // matching pairs to share a stream (enforced by the
            // depth check above), so `got` and the matching stack
            // entry are guaranteed to come from the same source.
            let src = self.scan.src_bytes();
            let cur = self.scan.cur_bytes();
            let got = &cur[name_start as usize..name_end as usize];
            // Look at the top entry by reference so we can compare
            // its bytes — which may live in `src` (SourceRange) or
            // in the owned String for entity-stream entries.
            let top_matches = self.element_stack.last()
                .map(|e| e.name_bytes(src) == got)
                .unwrap_or(false);
            if top_matches {
                self.element_stack.pop();
                self.element_streams.pop();
                self.frame_saw_child.pop();
            } else if let Some(top) = self.element_stack.last() {
                // SAFETY: Scanner invariant — element-name bytes are
                // valid UTF-8.  Only used for the error message.
                let exp = unsafe { std::str::from_utf8_unchecked(top.name_bytes(src)) };
                let got_str = unsafe { std::str::from_utf8_unchecked(got) };
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!("mismatched end tag: expected '</{exp}>', got '</{got_str}>'"),
                ).with_code(crate::error::ErrorCode::TagNameMismatch);
                // Recovery (libxml2-style): if `got` matches an
                // element deeper in the stack, auto-close the
                // intermediates.  We do this one element at a
                // time — synthesise ONE close, rewind the
                // cursor to before `</name>`, and let the next
                // next() call re-process.  Each iteration peels
                // off one open element until either the names
                // match or the stack is empty.
                self.maybe_recover(err)?;
                // `maybe_recover` took `&mut self`, invalidating
                // the `src` / `cur` borrows we held above.  Re-
                // acquire from the same buffers (positions are
                // stable) so the stack-search closure can compare
                // bytes again.
                let src = self.scan.src_bytes();
                let cur = self.scan.cur_bytes();
                let got = &cur[name_start as usize..name_end as usize];
                let stack_has_match = self.element_stack.iter()
                    .any(|e| e.name_bytes(src) == got);
                if stack_has_match {
                    // Rewind so the </name> is re-read after
                    // the synth close.
                    self.scan.cur_set_pos(rewind_pos);
                    return Ok(self.synthesize_close());
                }
                // Name doesn't appear anywhere on the stack —
                // discard the spurious end tag and continue.
                // (We've already consumed `</name>` plus the
                // skip_ws + `>`; cursor is at the next event.)
                return Ok(self.synthesize_close());
            } else {
                // element_stack was empty — end tag with no
                // matching start.  Recover by logging and dropping.
                let got_str = unsafe { std::str::from_utf8_unchecked(got) };
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!("unexpected end tag '</{got_str}>' with no open element"),
                );
                self.maybe_recover(err)?;
                // Recovery: just drop the orphaned end tag and
                // emit nothing for it.  We're at depth 0; the
                // PendingEnd state isn't meaningful here, so
                // recurse into next() to fetch the next real
                // event.
                return self.next();
            }
        }
        // BytesEndTag borrows from `src` — if the end tag was parsed
        // inside an entity stream, `name_start`/`name_end` index into
        // that stream's bytes, NOT `src`.  Returning the source-offset
        // pair would point at unrelated source bytes.  Hand back an
        // empty name range in that case; structural bookkeeping
        // (depth, element_stack pop) is already correct.
        let src = self.scan.src_bytes();
        let (out_start, out_end) = if self.scan.on_original_source() {
            (name_start, name_end)
        } else {
            (0, 0)
        };
        Ok(BytesEvent::EndElement(BytesEndTag {
            src, name_start: out_start, name_end: out_end,
        }))
    }

    fn read_comment(&mut self) -> Result<BytesEvent<'_, 'src>> {
        self.scan.expect_str(b"<!--")?;
        // Count only miscs the builder will actually attach as a
        // document-level orphan, so the prolog index stays in step with
        // the sibling chain (`remove_comments` drops the node).
        let is_prolog_misc =
            self.depth == 0 && !self.root_seen && !self.scan.opts.remove_comments;
        let start = self.scan.cur_pos();
        loop {
            match memchr(b'-', self.scan.cur_tail()) {
                None => return Err(self.scan.err("unterminated comment")),
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                    if self.scan.starts_with(b"--") {
                        let content = self.scan.cur_slice(start, self.scan.cur_pos());
                        self.scan.skip_n(2);
                        if self.scan.peek() != Some(b'>') {
                            return Err(self.scan.err("'--' inside comment content is not allowed"));
                        }
                        self.scan.advance();
                        if is_prolog_misc { self.prolog_misc_count += 1; }
                        return Ok(BytesEvent::Comment(BytesComment { inner: content }));
                    }
                    self.scan.advance(); // lone `-`, keep going
                }
            }
        }
    }

    fn read_cdata(&mut self) -> Result<BytesEvent<'_, 'src>> {
        self.scan.expect_str(b"<![CDATA[")?;
        let start = self.scan.cur_pos();
        loop {
            match memchr(b']', self.scan.cur_tail()) {
                None => return Err(self.scan.err("unterminated CDATA")),
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                    if self.scan.starts_with(b"]]>") {
                        // §2.11 EOL normalization applies inside CDATA
                        // too — the spec frames it as happening before
                        // parsing.  The lazy wrapper keeps the common
                        // (no-EOL-byte) case zero-copy.
                        let content = self.scan.cur_slice(start, self.scan.cur_pos());
                        let content = maybe_normalize_eol(
                            content, self.is_xml_11, self.source_has_eol_candidate,
                        );
                        self.scan.skip_n(3);
                        return Ok(BytesEvent::CData(BytesCData { inner: content }));
                    }
                    self.scan.advance(); // lone `]`, keep going
                }
            }
        }
    }

    fn read_pi(&mut self) -> Result<BytesEvent<'_, 'src>> {
        self.scan.expect_str(b"<?")?;
        let is_prolog_misc =
            self.depth == 0 && !self.root_seen && !self.scan.opts.remove_pis;
        let target = self.scan.scan_name_bytes()?;
        if target.eq_ignore_ascii_case(b"xml") {
            return Err(self.scan.err("PI target 'xml' is reserved"));
        }
        // XML Namespaces 1.0 § 3 [NSPITarget]: PITarget must be an
        // NCName when namespace-aware — i.e. no colon anywhere.
        // Reject e.g. `<?a:b ?>`.
        if self.scan.opts.namespace_aware && target.contains(&b':') {
            return Err(self.scan.err(format!(
                "PI target '{}' must be an NCName (no colon) under XML \
                 Namespaces 1.0",
                String::from_utf8_lossy(&target)
            )));
        }
        let content = if matches!(self.scan.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.scan.skip_ws();
            let start = self.scan.cur_pos();
            loop {
                if self.scan.is_eof() { return Err(self.scan.err("unterminated PI")); }
                if self.scan.starts_with(b"?>") {
                    let s = self.scan.cur_slice(start, self.scan.cur_pos());
                    self.scan.skip_n(2);
                    break s;
                }
                self.scan.advance();
            }
        } else {
            self.scan.expect_str(b"?>")?;
            Cow::Borrowed(&b""[..])
        };
        if is_prolog_misc { self.prolog_misc_count += 1; }
        Ok(BytesEvent::Pi(BytesPi { target_: target, content_: content }))
    }

    /// Peek for an unresolved user-defined entity reference at the
    /// current scanner position (which must already be on `&`).  If
    /// the upcoming bytes are `&NAME;` where NAME is neither
    /// predefined (`amp`/`lt`/`gt`/`quot`/`apos`) nor a numeric
    /// `#...;`, queue a `NextState::PendingEntityRef` carrying NAME
    /// and consume `&NAME;` from the scanner.  Returns `Ok(true)`
    /// when the reference was queued, `Ok(false)` otherwise.
    ///
    /// Caller must have already verified `!opts.resolve_entities`
    /// and `scan.on_original_source()`.  Numeric character refs
    /// (`&#65;` / `&#x41;`) always expand inline regardless of
    /// `resolve_entities` — they're part of the character production,
    /// not the entity-reference machinery.
    fn try_queue_user_entity_ref(&mut self) -> Result<bool> {
        let bytes = self.scan.src_bytes();
        let amp_pos = self.scan.cur_pos();
        debug_assert_eq!(bytes.get(amp_pos), Some(&b'&'));
        let name_start = amp_pos + 1;
        if name_start >= bytes.len() { return Ok(false); }
        // Numeric refs always expand inline.
        if bytes[name_start] == b'#' { return Ok(false); }
        // Find the trailing `;` — name runs over NameChar bytes.
        let mut p = name_start;
        while p < bytes.len() {
            let b = bytes[p];
            // ASCII NameChar fast check.  Non-ASCII bytes (>= 0x80)
            // are accepted; the byte reader's name validation has
            // already passed at this point, so any non-`;` sequence
            // here is a valid name continuation.  Stop at the first
            // non-name byte and let the caller fall through to the
            // normal expand path, which will report a clean error
            // if the reference is malformed.
            if b == b';' { break; }
            let is_name_byte = matches!(b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                | b'_' | b'-' | b'.' | b':' | 0x80..=0xFF
            );
            if !is_name_byte { return Ok(false); }
            p += 1;
        }
        if p == name_start || p >= bytes.len() || bytes[p] != b';' {
            return Ok(false);
        }
        let name_end = p;
        // Predefined entities always expand.
        let name = &bytes[name_start..name_end];
        if matches!(name, b"amp" | b"lt" | b"gt" | b"quot" | b"apos") {
            return Ok(false);
        }
        // Queue + consume `&NAME;`.
        self.state = NextState::PendingEntityRef(name_start as u32, name_end as u32);
        self.scan.cur_set_pos(name_end + 1); // past the closing `;`
        Ok(true)
    }

    /// With the cursor parked on the `&` of a content reference, peek
    /// the entity name (without consuming) and, if it names a deferred
    /// external general entity, load it now — so the subsequent
    /// `expand_reference_bytes` sees `ExternalLoaded` replacement text.
    /// A no-op for builtins, numeric refs, and already-resolved names.
    fn maybe_load_deferred_entity_at_cursor(&mut self) -> Result<()> {
        if self.deferred_general_entities.is_empty() {
            return Ok(());
        }
        let bytes = self.scan.cur_bytes();
        let pos = self.scan.cur_pos();
        if bytes.get(pos) != Some(&b'&') {
            return Ok(());
        }
        let name_start = pos + 1;
        // Numeric character references never name an entity.
        if bytes.get(name_start) == Some(&b'#') {
            return Ok(());
        }
        let mut p = name_start;
        while p < bytes.len() && bytes[p] != b';' {
            p += 1;
        }
        if p >= bytes.len() || p == name_start {
            return Ok(());
        }
        let name = match std::str::from_utf8(&bytes[name_start..p]) {
            Ok(s) => s.to_string(),
            Err(_) => return Ok(()),
        };
        self.load_deferred_entity(&name)
    }

    /// Resolve a deferred external general entity through the configured
    /// `external_resolver`, upgrading its table entry from
    /// `ExternalUnloaded` to `ExternalLoaded`.  Idempotent: once the
    /// deferred record is consumed, later references reuse the loaded
    /// text.  A resolver failure on a *referenced* entity is a parse
    /// error in strict mode (recovered to `ExternalUnloaded` otherwise),
    /// mirroring how a referenced-but-unloadable entity behaved when
    /// loading was eager.
    fn load_deferred_entity(&mut self, name: &str) -> Result<()> {
        let Some(deferred) = self.deferred_general_entities.remove(name) else {
            return Ok(());
        };
        let Some(resolver) = self.scan.opts.external_resolver.clone() else {
            return Ok(());
        };
        let declared_external = self.entities.get(name)
            .map(|d| d.declared_external)
            .unwrap_or(false);
        // E18: general-entity SYSTEM ids resolve against the document URL.
        let base = self.scan.opts.base_url.clone();
        let absolute = resolve_uri(&deferred.system_id, base.as_deref());
        match resolver.resolve(deferred.public_id.as_deref(), &absolute, base.as_deref()) {
            Ok(raw) => {
                // XML 1.0 § 4.3.3: the external entity may be in any
                // documented encoding; detect + transcode to UTF-8, then
                // validate (transcode short-circuits the UTF-8 path
                // without validating).
                let value = match crate::encoding::transcode_to_utf8(&raw) {
                    Ok(c)  => String::from_utf8(c.into_owned()).map_err(|e| e.to_string()),
                    Err(e) => Err(e.message.clone()),
                };
                match value {
                    Ok(v) => {
                        self.entities.insert(name.to_string(), EntityDecl {
                            kind: EntityKind::ExternalLoaded(v),
                            declared_external,
                            source_uri: Some(absolute),
                        });
                    }
                    Err(msg) => {
                        let err = self.scan.err_with_level(
                            ErrorLevel::Error,
                            format!(
                                "external entity '&{name};' is not valid UTF-8 \
                                 (system_id={:?}): {msg}",
                                deferred.system_id
                            ),
                        );
                        self.maybe_recover(err)?;
                    }
                }
            }
            Err(e) => {
                let err = self.scan.err_with_level(
                    ErrorLevel::Error,
                    format!(
                        "external resolver failed to load entity '&{name};' \
                         (system_id={:?}, public_id={:?}): {e}",
                        deferred.system_id, deferred.public_id
                    ),
                );
                self.maybe_recover(err)?;
            }
        }
        Ok(())
    }

    fn read_text(&mut self) -> Result<BytesEvent<'_, 'src>> {
        let start = self.scan.cur_pos();

        // Raw-text mode: skip entity expansion and `]]>` checking, scan only
        // for the next `<`.  The Text payload is the source slice verbatim
        // with `&…;` references left in place; callers expand on demand via
        // `unescape`.  This is the apples-to-apples-with-quick-xml fast path
        // — single `memchr(b'<')` over the whole text body.  The opt-in skips
        // §2.11 EOL normalization too: callers using `skip_entity_expansion`
        // are responsible for any normalization they need downstream.
        if self.scan.opts.skip_entity_expansion {
            match memchr(b'<', self.scan.cur_tail()) {
                None => {
                    self.scan.cur_set_pos(self.scan.cur_len());
                }
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                }
            }
            return Ok(BytesEvent::Text(BytesText {
                inner: self.scan.cur_slice(start, self.scan.cur_pos()),
            }));
        }

        // SIMD fast path: scan for `<`, `&`, or `]` in one pass.  When we
        // stop at `<` (or EOF) without seeing any `&`, the result is
        // zero-copy (`cur_str` returns `Cow::Borrowed` for the source stream)
        // *unless* the segment contains EOL bytes that XML §2.11 requires
        // us to rewrite (`\r`, plus NEL/LS in XML 1.1) — in that case we
        // fall through to a normalized owned copy.  The `gate` flag is
        // the one-shot source pre-scan: when the entire document
        // contains no EOL candidate byte the per-segment scan and the
        // rewrite branch both become dead code.
        let is_xml_11 = self.is_xml_11;
        let gate = self.source_has_eol_candidate;
        loop {
            let tail = self.scan.cur_tail();
            match memchr3(b'<', b'&', b']', tail) {
                None => {
                    self.scan.cur_set_pos(self.scan.cur_len());
                    let raw = self.scan.cur_slice(start, self.scan.cur_pos());
                    return Ok(BytesEvent::Text(BytesText {
                        inner: maybe_normalize_eol(raw, is_xml_11, gate),
                    }));
                }
                Some(off) => {
                    self.scan.cur_advance_pos(off);
                    match self.scan.cur_bytes()[self.scan.cur_pos()] {
                        b'<' => {
                            let raw = self.scan.cur_slice(start, self.scan.cur_pos());
                            return Ok(BytesEvent::Text(BytesText {
                                inner: maybe_normalize_eol(raw, is_xml_11, gate),
                            }));
                        }
                        b'&' => break, // fall through to expansion path
                        b']' => {
                            if self.scan.starts_with(b"]]>") {
                                if self.scan.opts.recovery_mode {
                                    // Drop into the slow path with
                                    // the cursor still parked on
                                    // `]]>`; the slow path's `]`
                                    // arm handles the recovery
                                    // (push 3 bytes literal +
                                    // skip).
                                    break;
                                }
                                return Err(self.scan.err("']]>' not allowed in text content"));
                            }
                            self.scan.advance(); // lone `]` is fine
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }

        // Slow path: entity found.  Reuse the reader's text-decode scratch
        // buffer (grows monotonically over the parse) instead of allocating
        // a fresh `Vec::new()` per slow-path entry.  We swap the buffer out
        // via `mem::take` to satisfy the borrow checker — `expand_reference_bytes`
        // wants `&mut self.scan` and `&mut buf` at the same time, which is
        // unsound with `&mut self.text_decode_buf`.  The swap puts an empty
        // sentinel in place during the loop and restores the (possibly
        // larger-capacity) buffer at the end.
        let mut buf = std::mem::take(&mut self.text_decode_buf);
        buf.clear();
        // In-place + original-stream mode: skip copying the clean prefix
        // (bytes from `start` to current `cur_pos`) into `buf` — those
        // bytes are already at the right position in source.  `buf` then
        // collects only the *decoded suffix* (entity expansions +
        // post-entity clean runs), and the final `compact_at` writes
        // that suffix starting at `prefix_end`, leaving the prefix
        // untouched.  Saves one memcpy of `cur_pos - start` bytes per
        // slow-path entry — significant when the entity-bearing region
        // is preceded by a long clean run (typical real-world docs).
        let prefix_end = self.scan.cur_pos();
        let in_place_orig = self.scan.is_in_place() && self.scan.on_original_source();
        if !in_place_orig {
            append_text_segment(&self.scan, start, prefix_end, &mut buf, self.is_xml_11, self.source_has_eol_candidate);
        }
        let budget = self.scan.opts.max_entity_expansion_bytes;
        let depth = self.element_stack.len() as u32;
        loop {
            let tail = self.scan.cur_tail();
            match memchr3(b'<', b'&', b']', tail) {
                None => {
                    // Consume rest of the current stream into `buf`.
                    let end = self.scan.cur_len();
                    let from = self.scan.cur_pos();
                    append_text_segment(&self.scan, from, end, &mut buf, self.is_xml_11, self.source_has_eol_candidate);
                    self.scan.cur_set_pos(end);
                    // If the current stream was an entity replacement, pop it
                    // and keep accumulating text from the next stream below —
                    // but only if the element-stack depth still matches the
                    // depth at push (XML 1.0 § 4.3.2 WFC 'Logical Structure').
                    if let Some((name, depth_at_push)) = self.scan.top_entity_info() {
                        if depth_at_push != depth {
                            return Err(self.scan.err(format!(
                                "entity '&{name};' contains unbalanced element markup — \
                                 element-stack depth was {depth_at_push} when the entity \
                                 was expanded but is {depth} at its end \
                                 (XML 1.0 § 4.3.2 WFC 'Logical Structure')"
                            )));
                        }
                    }
                    if self.scan.try_pop_entity_stream() {
                        continue;
                    }
                    break;
                }
                Some(off) => {
                    let from = self.scan.cur_pos();
                    append_text_segment(&self.scan, from, from + off, &mut buf, self.is_xml_11, self.source_has_eol_candidate);
                    self.scan.cur_advance_pos(off);
                    match self.scan.cur_bytes()[self.scan.cur_pos()] {
                        b'<' => break,
                        b'&' => {
                            // Bare `&` recovery: a `&` followed by
                            // something that isn't `#` or a name-
                            // start character can't be a valid
                            // reference.  In recover mode, keep the
                            // `&` as a literal byte (preserving
                            // user data, unlike libxml2 which
                            // silently DROPS the `&`).
                            let next = self.scan.peek_at(1);
                            let is_ref_start = matches!(
                                next,
                                Some(b'#')
                                | Some(b'A'..=b'Z')
                                | Some(b'a'..=b'z')
                                | Some(b'_')
                                | Some(b':')
                                | Some(0x80..=0xFF)   // non-ASCII NameStart
                            );
                            if !is_ref_start && self.scan.opts.recovery_mode {
                                let err = self.scan.err_with_level(
                                    ErrorLevel::Error,
                                    "bare '&' in text content — kept literal \
                                     (XML 1.0 § 4.1 [Reference])",
                                );
                                self.recovered_errors.push(err);
                                buf.push(b'&');
                                self.scan.advance();
                            } else if !self.scan.opts.resolve_entities
                                && self.scan.on_original_source()
                                && self.try_queue_user_entity_ref()?
                            {
                                // EntityRef queued; flush the
                                // pre-reference Text content now and
                                // let the next next() call emit the
                                // ref.
                                break;
                            } else {
                                // Resolve a deferred external general
                                // entity (loaded lazily on first
                                // reference) before expanding it.
                                self.maybe_load_deferred_entity_at_cursor()?;
                                expand_reference_bytes(
                                    &mut self.scan, &mut buf,
                                    &self.entities,
                                    &mut self.expansion_bytes, budget, depth,
                                    Some(&mut self.recovered_errors),
                                    self.pe_ref_in_internal_subset_seen,
                                    self.standalone_yes,
                                    self.is_xml_11,
                                )?;
                            }
                        }
                        b']' => {
                            if self.scan.starts_with(b"]]>") {
                                // Recovery: keep `]]>` literal in
                                // the text content rather than
                                // mangling surrounding bytes the
                                // way libxml2 does.
                                let err = self.scan.err_with_level(
                                    ErrorLevel::Error,
                                    "']]>' not allowed in text content — kept literal \
                                     (XML 1.0 § 2.4 [CharData])",
                                );
                                self.maybe_recover(err)?;
                                buf.extend_from_slice(b"]]>");
                                self.scan.skip_n(3);
                            } else {
                                buf.push(b']');
                                self.scan.advance();
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        // In-place mode: write the decoded bytes back into the source
        // buffer at `start..start + buf.len()` and emit a Cow::Borrowed
        // slice into that span.  The "garbage tail" at
        // start + buf.len() .. cur_pos remains in the buffer but is
        // never re-read (the scanner has already advanced past it).
        //
        // The expansion fits only if `buf.len() <= cur_pos - start` —
        // i.e. the decoded form is no bigger than the source span it
        // came from.  For XML 1.0 builtins (`&amp;` → `&`, etc.) and
        // numeric char refs (`&#xC9;` → `É`) the rule is satisfied by
        // construction.  For user-defined `<!ENTITY>` references whose
        // replacement text is bigger than `&name;`, this is the
        // rejection site documented in the plan — we return Err.
        //
        // Only safe when we're still reading from the original source
        // (no active entity stream) — entity-stream bytes don't live in
        // the source buffer and `compact_at` would write to the wrong
        // place.
        if in_place_orig {
            // `buf` holds only the decoded *suffix* (the part after
            // the clean prefix that ends at `prefix_end`).  The
            // suffix's original source span is `prefix_end..end_pos`
            // — we require the decoded suffix to fit in it.  When
            // the prefix is short or absent this matches the old
            // "buf.len() <= end_pos - start" check; when the prefix
            // is long the new check is tighter (good — we'd corrupt
            // source by writing past `end_pos`).
            let end_pos = self.scan.cur_pos();
            let suffix_orig_len = end_pos.saturating_sub(prefix_end);
            if buf.len() > suffix_orig_len {
                let err = self.scan.err(format!(
                    "entity expansion ({} bytes) exceeds source span ({} bytes); \
                     parse_bytes_in_place requires expansion ≤ reference length. \
                     Use parse_bytes for documents with expanding user-defined entities.",
                    buf.len(), suffix_orig_len,
                ));
                self.text_decode_buf = buf;
                return Err(err);
            }
            // If the clean prefix carries any EOL-significant byte the
            // in-place compact would emit raw `\r` / NEL / LS to the
            // caller, violating §2.11.  Rather than introduce a
            // shifting in-place rewrite (EOL never lengthens, but
            // representing the shrunk prefix would need a second
            // compact), fall back to a fresh owned Vec for the rare
            // "in-place + entity + EOL-in-prefix" case.
            let prefix_bytes = self.scan.src_slice(start, prefix_end);
            if self.source_has_eol_candidate
                && first_eol_byte(prefix_bytes, self.is_xml_11).is_some()
            {
                let mut out = Vec::with_capacity(prefix_bytes.len() + buf.len());
                append_eol_normalized(prefix_bytes, &mut out, self.is_xml_11);
                out.extend_from_slice(&buf);
                self.text_decode_buf = buf;
                return Ok(BytesEvent::Text(BytesText { inner: Cow::Owned(out) }));
            }
            // Write decoded suffix into source[prefix_end..prefix_end+buf.len()].
            // The clean prefix at source[start..prefix_end] is unchanged.
            let suffix_written = buf.len();
            if suffix_written > 0 {
                self.scan.compact_at(prefix_end, end_pos, &buf);
            }
            // Restore the scratch buffer for the next slow-path entry —
            // its capacity grows monotonically across the parse.
            self.text_decode_buf = buf;
            let total_len = (prefix_end - start) + suffix_written;
            return Ok(BytesEvent::Text(BytesText {
                inner: Cow::Borrowed(self.scan.src_slice(start, start + total_len)),
            }));
        }
        // Non-in-place path consumes `buf` into the event payload — the
        // caller (`alloc_cow_bytes_as_str` in arena_parser) copies it into
        // the bump arena and drops the Vec.  We pay a fresh Vec next slow-
        // path entry; the in-place fast path is the one we optimise.
        Ok(BytesEvent::Text(BytesText { inner: Cow::Owned(buf) }))
    }
}

/// Walk an entity reference chain from `name`, checking that every
/// reference resolves (XML 1.0 § 4.1 WFC: Entity Declared) and that
/// no cycle exists (§ 4.1 WFC: No Recursion).  `chain` accumulates
/// the names currently being expanded; if we encounter a name
/// already on it, we have a cycle.
///
/// `inside_external` enables the XML 1.0 § 4.1 WFC: Entity Declared
/// carve-out: refs that appear within the external subset or a
/// parameter entity's replacement text are exempt from the WF rule
/// (at most a VC violation, which non-validating parsers tolerate).
/// When `true`, an undeclared `name` returns `Ok(())` instead of
/// erroring; the cycle check still applies if the entity is found.
fn check_entity_chain<'a>(
    name: &'a str,
    entities: &'a HashMap<String, EntityDecl>,
    chain: &mut Vec<&'a str>,
    inside_external: bool,
) -> std::result::Result<(), String> {
    if chain.contains(&name) {
        return Err(format!(
            "entity '&{name};' references itself (XML 1.0 § 4.1 WFC: No Recursion); \
             chain: {} -> {name}",
            chain.join(" -> ")
        ));
    }
    let predef = matches!(name, "amp" | "lt" | "gt" | "quot" | "apos");
    if predef { return Ok(()); }
    let value = match entities.get(name).and_then(|k| k.replacement()) {
        Some(v) => v,
        None => {
            if inside_external {
                // WFC: Entity Declared carve-out — refs inside an
                // external entity / external subset don't trigger
                // the WF rule.  Accept silently.
                return Ok(());
            }
            return Err(format!(
                "undefined entity '&{name};' (XML 1.0 § 4.1 WFC: Entity Declared)"
            ));
        }
    };
    chain.push(name);
    // Scan the value for nested references AND for stray `&` / `<`
    // bytes that the replacement text would inject into the
    // including context.  XML 1.0 § 4.5 says replacement text must
    // be well-formed CharData when included in attr values / element
    // content — a literal `&` (e.g. produced by `&#38;` pre-expansion)
    // would create a bare ampersand in the output, which the spec
    // forbids.  A literal `<` is forbidden in attribute values
    // (§ 3.1 WFC: No < in Attribute Values).
    let vb = value.as_bytes();
    let mut j = 0;
    while j < vb.len() {
        let b = vb[j];
        if b == b'<' {
            return Err(format!(
                "entity '&{name};' replacement text contains a literal `<` — \
                 inclusion in an attribute value would violate XML 1.0 § 3.1 \
                 WFC: No < in Attribute Values"
            ));
        }
        if b != b'&' { j += 1; continue; }
        let after = j + 1;
        let semi = vb[after..].iter().position(|&c| c == b';');
        let Some(off) = semi else {
            // Bare `&` not followed by a name — replacement text is
            // ill-formed when included.  XML 1.0 § 4.1.
            return Err(format!(
                "entity '&{name};' replacement text contains a bare `&` — \
                 inclusion would violate XML 1.0 § 4.1 [Reference]"
            ));
        };
        let body = &vb[after..after + off];
        if body.first() != Some(&b'#') {
            if let Ok(child) = std::str::from_utf8(body) {
                // SAFETY: `entities` keys outlive this call; recursing
                // with a borrowed `&'a str` into the same map is fine.
                let key: &'a str = entities
                    .get_key_value(child)
                    .map(|(k, _)| k.as_str())
                    .unwrap_or(child);
                check_entity_chain(key, entities, chain, inside_external)?;
            }
        }
        j = after + off + 1;
    }
    chain.pop();
    Ok(())
}

/// XML 1.0 § 4.5 [Entity Replacement Text]: expand character
/// references (`&#NN;` / `&#xNN;`) but leave general-entity
/// references unexpanded.  This matches the spec's distinction
/// between LITERAL ENTITY VALUE (raw bytes between the quotes in
/// the `<!ENTITY>` declaration) and REPLACEMENT TEXT (what gets
/// substituted at expansion time).
///
/// Pre-expanding char refs at declaration time is what makes
/// `<!ENTITY e "&#38;"> <doc>&e;</doc>` correctly fail: the
/// replacement text is a literal `&` byte; expanding `&e;` pushes
/// `&` into the parser stream; the parser sees a bare `&` not
/// Consume an XML 1.0 §4.3.1 text declaration if one is present
/// at the scanner's current position, and validate its structure.
///
/// ```text
/// TextDecl     ::= '<?xml' VersionInfo? EncodingDecl S? '?>'
/// VersionInfo  ::= S 'version' Eq ("'" VersionNum "'" | '"' VersionNum '"')
/// EncodingDecl ::= S 'encoding' Eq ('"' EncName '"' | "'" EncName "'")
/// ```
///
/// Distinct from the document-level XML declaration in two ways:
/// `version` is optional, and `standalone` is **forbidden**.  We
/// enforce the latter by rejecting any pseudo-attribute name other
/// than `version` or `encoding`.
///
/// Returns `Ok(())` either way (no decl present, or one present and
/// well-formed).  Returns `Err` when a decl is present but malformed
/// (e.g. carries `standalone=`, missing `?>`, encoding not a string).
///
/// Called right after [`push_entity_stream`] for *external* parsed
/// entities only — internal entity content has no text-decl and any
/// `<?xml...?>` at its start is a reserved-target PI (not-wf).
pub(crate) fn consume_text_decl_if_present(
    scan:             &mut Scanner,
    is_outer_xml_11:  bool,
) -> Result<()> {
    if !scan.starts_with(b"<?xml") { return Ok(()); }
    // The byte after `<?xml` must be whitespace so we don't
    // confuse a PI named e.g. `xml-stylesheet` for a text-decl.
    if !matches!(scan.peek_at(5), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        return Ok(());
    }
    scan.skip_n(5);                                                      // past `<?xml`
    // Pseudo-attributes: optional `version=…` MUST come first,
    // then a required `encoding=…`.  Anything else (`standalone`,
    // an unknown name, or version-after-encoding) is malformed.
    scan.skip_ws();
    let mut seen_version = false;
    let mut seen_encoding = false;
    while !scan.starts_with(b"?>") {
        if scan.peek().is_none() {
            return Err(scan.err("unterminated text declaration (expected '?>')"));
        }
        let key_start = scan.cur_pos();
        scan.skip_name()?;
        let key_end = scan.cur_pos();
        let key = scan.cur_slice(key_start, key_end).to_vec();
        match key.as_slice() {
            b"version" => {
                if seen_version {
                    return Err(scan.err("text declaration has duplicate 'version' pseudo-attribute"));
                }
                if seen_encoding {
                    return Err(scan.err(
                        "text declaration must list 'version' before 'encoding' \
                         (XML 1.0 § 4.3.1 [77])",
                    ));
                }
                seen_version = true;
                scan.skip_ws();
                scan.expect(b'=')?;
                scan.skip_ws();
                let q = match scan.peek() {
                    Some(b @ (b'"' | b'\'')) => { scan.advance(); b }
                    _ => return Err(scan.err("expected quoted value in text declaration")),
                };
                let val_start = scan.cur_pos();
                while let Some(b) = scan.peek() {
                    if b == q { break; }
                    scan.advance();
                }
                let val_end = scan.cur_pos();
                let ver = scan.cur_slice(val_start, val_end).to_vec();
                scan.expect(q)?;
                // XML 1.0 §4.3.4 / XML 1.1 §4.3.4 — an external
                // parsed entity referenced from an XML 1.0 document
                // must itself be XML 1.0; from an XML 1.1 document
                // it may be either 1.0 or 1.1.  `version="1.1"` in
                // an entity included by an XML 1.0 doc is not-wf.
                let entity_ver_ok = ver == b"1.0"
                    || (is_outer_xml_11 && ver == b"1.1");
                if !entity_ver_ok {
                    return Err(scan.err(format!(
                        "text declaration version {:?} is not compatible with the \
                         host document's version (XML §4.3.4)",
                        String::from_utf8_lossy(&ver),
                    )));
                }
                scan.skip_ws();
            }
            b"encoding" => {
                if seen_encoding {
                    return Err(scan.err("text declaration has duplicate 'encoding' pseudo-attribute"));
                }
                seen_encoding = true;
                scan.skip_ws();
                scan.expect(b'=')?;
                scan.skip_ws();
                let q = match scan.peek() {
                    Some(b @ (b'"' | b'\'')) => { scan.advance(); b }
                    _ => return Err(scan.err("expected quoted value in text declaration")),
                };
                while let Some(b) = scan.peek() {
                    if b == q { scan.advance(); break; }
                    scan.advance();
                }
                scan.skip_ws();
            }
            other => return Err(scan.err(format!(
                "{:?} is not a valid pseudo-attribute in a text declaration \
                 (XML 1.0 § 4.3.1 forbids anything besides version and encoding — \
                 note standalone= is document-only, see § 2.9)",
                String::from_utf8_lossy(other),
            ))),
        }
    }
    scan.skip_n(2); // past `?>`
    if !seen_encoding {
        return Err(scan.err(
            "text declaration is missing the required encoding= pseudo-attribute \
             (XML 1.0 § 4.3.1)",
        ));
    }
    Ok(())
}

/// Superseded by `XmlBytesReader::expand_entity_value`, which also
/// handles parameter-entity references per XML 1.0 § 4.5.  Kept
/// here as a free function only briefly as the new path's
/// implementation reference; if you need the char-ref-only
/// behavior, call `expand_entity_value` with an empty
/// `parameter_entities` map.
#[allow(dead_code)]
fn expand_entity_replacement_text(bytes: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'&' {
            out.push(b);
            i += 1;
            continue;
        }
        // `&` — char ref or named entity ref?  Look at next byte.
        let after = i + 1;
        if after >= bytes.len() {
            return Err("entity value ends with `&`".to_string());
        }
        if bytes[after] != b'#' {
            // Named GE reference — keep literal per § 4.5.
            // Just emit through to the matching `;`.
            let semi = bytes[after..].iter().position(|&c| c == b';')
                .ok_or_else(|| "named entity reference in entity value missing `;`".to_string())?;
            out.extend_from_slice(&bytes[i..after + semi + 1]);
            i = after + semi + 1;
            continue;
        }
        // `&#…;` — character reference.  Decode and emit the char.
        let body_start = after + 1;
        let semi = bytes[body_start..].iter().position(|&c| c == b';')
            .ok_or_else(|| "character reference missing `;`".to_string())?;
        let body = &bytes[body_start..body_start + semi];
        let cp: u32 = if body.first() == Some(&b'x') || body.first() == Some(&b'X') {
            std::str::from_utf8(&body[1..]).ok()
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!(
                    "invalid hex character reference '&#{}'", String::from_utf8_lossy(body)
                ))?
        } else {
            std::str::from_utf8(body).ok()
                .and_then(|d| d.parse::<u32>().ok())
                .ok_or_else(|| format!(
                    "invalid decimal character reference '&#{}'", String::from_utf8_lossy(body)
                ))?
        };
        let ch = char::from_u32(cp).ok_or_else(|| format!(
            "character reference '&#{};' is not a valid Unicode scalar", cp
        ))?;
        let mut tmp = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        i = body_start + semi + 1;
    }
    Ok(out)
}

/// XML 1.0 § 2.8 [26] [VersionNum]: `'1.' [0-9]+`.  No whitespace,
/// no leading sign, must start with the literal `1.`.  XML 1.1 also
/// allows `'1.' [0-9]+` so the same shape works for both.
fn is_valid_version(v: &[u8]) -> bool {
    v.len() >= 3
        && &v[..2] == b"1."
        && v[2..].iter().all(|c| c.is_ascii_digit())
}

/// XML 1.0 § 4.3.3 [81] [EncName]: `[A-Za-z] ([A-Za-z0-9._] | '-')*`.
/// First char must be a letter; remaining chars are letters, digits,
/// `.`, `_`, or `-`.  Catches `" utf-8"` (leading space), `"a/b"`
/// (`/` not allowed), `"utf:8"` (`:` not allowed), etc.
fn is_valid_encname(v: &[u8]) -> bool {
    let Some(&first) = v.first() else { return false; };
    if !first.is_ascii_alphabetic() { return false; }
    v[1..].iter().all(|&c| {
        c.is_ascii_alphanumeric() || c == b'.' || c == b'_' || c == b'-'
    })
}

/// Fast structural-only validator for attribute slices that contain
/// no `&` (no entity references) — the vast majority of real-world
/// documents.  Skips entity-reference handling entirely; everything
/// else is the same as `validate_attrs_syntax`.
///
/// Caller has already checked there's no `<` in the slice.  This
/// Read a single trailing occurrence indicator (`?`, `*`, `+`) from
/// the scanner, advancing past it.  Returns `None` if the next byte
/// is something else (caller should treat as [`Occurrence::One`]).
fn read_occurrence<'src>(
    scan: &mut crate::scanner::Scanner<'src, 'static>,
) -> Option<crate::dtd::Occurrence> {
    use crate::dtd::Occurrence;
    match scan.peek() {
        Some(b'?') => { scan.advance(); Some(Occurrence::ZeroOrOne)  }
        Some(b'*') => { scan.advance(); Some(Occurrence::ZeroOrMore) }
        Some(b'+') => { scan.advance(); Some(Occurrence::OneOrMore)  }
        _ => None,
    }
}


/// fast path uses memchr to jump to the closing quote of each
/// value rather than walking byte-by-byte, which dominated the
/// per-attribute cost on attribute-heavy fixtures.
fn validate_attrs_structure_fast(
    bytes: &[u8],
    opts: &ParseOptions,
) -> std::result::Result<(), String> {
    use crate::charsets::{ASCII_XML_NAME, NS};
    let mut i = 0;
    let n = bytes.len();
    let mut seen_inline: [(u32, u32); 8] = [(0, 0); 8];
    let mut seen_inline_len: usize = 0;
    let mut seen_overflow: Vec<(u32, u32)> = Vec::new();
    // Adversarial inputs may have thousands of attributes on a single
    // element.  Once the overflow Vec gets large, swap to a hash set so
    // duplicate detection stays linear in the attribute count instead
    // of quadratic.  Threshold picked so the common 9–31-attr case still
    // takes the cache-friendly linear path.
    let mut seen_set: Option<FxHashSet<&[u8]>> = None;
    const SEEN_PROMOTE_THRESHOLD: usize = 32;

    fn is_ws(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r') }

    let mut first_attr = true;
    while i < n {
        // Inter-attribute whitespace is required (except before
        // the first attribute) — XML 1.0 § 3.1 [40].
        let ws_start = i;
        while i < n && is_ws(bytes[i]) { i += 1; }
        if i >= n { break; }
        if !first_attr && i == ws_start {
            return Err(
                "expected whitespace between attributes (XML 1.0 § 3.1 [40] [STag])".to_string()
            );
        }
        first_attr = false;

        // Name: validate the leading char then skip to next `=`,
        // whitespace, or end via memchr.
        let name_start = i;
        if !opts.skip_name_validation {
            let b = bytes[i];
            if b < 0x80 && ASCII_XML_NAME[b as usize] & NS == 0 {
                return Err(format!(
                    "invalid attribute name-start character {:?}",
                    b as char
                ));
            }
        }
        let stop = memchr3(b'=', b' ', b'\t', &bytes[i..n])
            .map(|o| i + o)
            .unwrap_or(n);
        // Manual sweep for `\n` / `\r` (rare in attr-name positions).
        let stop = bytes[i..stop].iter().position(|&c| c == b'\n' || c == b'\r')
            .map(|o| i + o)
            .unwrap_or(stop);
        i = stop;
        let name_end = i;
        if name_end == name_start {
            return Err("expected attribute name".to_string());
        }

        // Eq: optional whitespace, `=`, optional whitespace.
        while i < n && is_ws(bytes[i]) { i += 1; }
        if i >= n || bytes[i] != b'=' {
            return Err("expected '=' after attribute name".to_string());
        }
        i += 1;
        while i < n && is_ws(bytes[i]) { i += 1; }

        // Quoted value: jump straight to the closing quote.  `<`
        // can't appear (caller checked); `&` can't appear (that's
        // the precondition for being on this fast path).
        if i >= n {
            return Err("expected quoted attribute value".to_string());
        }
        let q = bytes[i];
        if q != b'"' && q != b'\'' {
            return Err(format!(
                "attribute value must be quoted (got '{}'); XML 1.0 § 3.1 [41] [AttValue]",
                q as char
            ));
        }
        i += 1;
        let value_start = i;
        let val_off = memchr(q, &bytes[value_start..n])
            .ok_or_else(|| "unterminated attribute value".to_string())?;
        i = value_start + val_off + 1;

        // Duplicate-name check (§ 3.1 WFC: Unique Att Spec).
        let new_name = &bytes[name_start..name_end];
        for &(s, e) in &seen_inline[..seen_inline_len] {
            if &bytes[s as usize..e as usize] == new_name {
                return Err(format!(
                    "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                    String::from_utf8_lossy(new_name)
                ));
            }
        }
        if let Some(set) = seen_set.as_mut() {
            if !set.insert(new_name) {
                return Err(format!(
                    "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                    String::from_utf8_lossy(new_name)
                ));
            }
        } else {
            for &(s, e) in &seen_overflow {
                if &bytes[s as usize..e as usize] == new_name {
                    return Err(format!(
                        "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                        String::from_utf8_lossy(new_name)
                    ));
                }
            }
            if seen_inline_len < seen_inline.len() {
                seen_inline[seen_inline_len] = (name_start as u32, name_end as u32);
                seen_inline_len += 1;
            } else {
                seen_overflow.push((name_start as u32, name_end as u32));
                if seen_overflow.len() >= SEEN_PROMOTE_THRESHOLD {
                    let mut set: FxHashSet<&[u8]> = FxHashSet::default();
                    set.reserve(seen_overflow.len() * 2);
                    for &(s, e) in &seen_overflow {
                        set.insert(&bytes[s as usize..e as usize]);
                    }
                    seen_set = Some(set);
                }
            }
        }
    }
    Ok(())
}

/// Eagerly validate the syntax of one start tag's attribute slice.
/// Called from `read_start_element` so well-formedness errors fire
/// even when the application never iterates `BytesAttrs`.  This is
/// a syntactic check only — no entity expansion, no decoding, no
/// allocation; just a single pass over the bytes verifying:
///
/// - each attr starts with a valid Name (XML 1.0 § 2.3 [4–5])
/// - the next char after the name is `=` (modulo whitespace)
/// - the value is quoted with `"` or `'`                  (§ 3.1 [41])
/// - the value doesn't contain a literal `<`              (§ 3.1 WFC)
/// - any `&` inside the value is followed by a valid
///   `Name;` (entity ref) or `#[0-9]+;` / `#x[0-9a-fA-F]+;`
///   (char ref)                                           (§ 4.1 WFC)
/// - no attribute name appears more than once             (§ 3.1 WFC)
///
/// Returns `Err(message)` describing the violation; the caller
/// wraps it in a `Scanner::err` with location info.
fn validate_attrs_syntax(
    bytes: &[u8],
    opts: &ParseOptions,
    entities: &HashMap<String, EntityDecl>,
    inside_external: bool,
) -> std::result::Result<(), String> {
    use crate::charsets::{ASCII_XML_NAME, NS, NC};
    let mut i = 0;
    let n = bytes.len();
    // Most start tags have < 8 attrs.  Use a stack array up to that
    // cap to avoid the per-element heap allocation that dominates
    // attribute-heavy fixtures (bargains_he_5, OSM, gazali — every
    // element has 5–10 attrs and triggers a Vec allocation otherwise).
    // Fall back to a Vec for the rare element with > 8 attrs.
    let mut seen_inline: [(u32, u32); 8] = [(0, 0); 8];
    let mut seen_inline_len: usize = 0;
    let mut seen_overflow: Vec<(u32, u32)> = Vec::new();
    // Adversarial inputs may have thousands of attributes on a single
    // element.  Once the overflow Vec gets large, swap to a hash set so
    // duplicate detection stays linear in the attribute count instead
    // of quadratic.  Threshold picked so the common 9–31-attr case still
    // takes the cache-friendly linear path.
    let mut seen_set: Option<FxHashSet<&[u8]>> = None;
    const SEEN_PROMOTE_THRESHOLD: usize = 32;

    fn is_ws(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r') }

    let mut first_attr = true;
    while i < n {
        // XML 1.0 § 3.1 [40] [STag]: each attribute MUST be preceded
        // by whitespace.  The leading run before the first attr can
        // be empty (the tag name is followed directly by `(S Attribute)*`
        // — but only if there are no attributes).  Since we're past
        // the tag name when we get here, any non-leading attribute
        // requires at least one whitespace byte before it.
        let ws_start = i;
        while i < n && is_ws(bytes[i]) { i += 1; }
        if i >= n { break; }
        if !first_attr && i == ws_start {
            return Err(
                "expected whitespace between attributes (XML 1.0 § 3.1 [40] [STag])".to_string()
            );
        }
        first_attr = false;

        // ── Name ────────────────────────────────────────────
        // Validate the name-start character (one ASCII table
        // lookup), then skip remaining name chars cheaply by
        // memchring for the first byte that's NOT a name char —
        // i.e. `=`, whitespace, or end of input.  This avoids the
        // per-byte ASCII-table check that dominated start-tag time
        // for attribute-heavy fixtures (bargains_he_5, gazali, osm).
        let name_start = i;
        if !opts.skip_name_validation {
            let b = bytes[i];
            if b < 0x80 && ASCII_XML_NAME[b as usize] & NS == 0 {
                return Err(format!(
                    "invalid attribute name-start character {:?}",
                    b as char
                ));
            }
            // Non-ASCII name-start chars are accepted conservatively
            // (the full Unicode NameStart table lives in Scanner;
            // duplicating it here would be wasteful and the lazy
            // iterator's full validation will fire if the user
            // reads attrs).
        }
        // Skip past the name to the next `=`, whitespace, or `/`.
        // Single SIMD scan beats the per-byte name-char loop.  Any
        // weird byte inside the name (e.g. `!`, `?`) is caught by
        // the lazy attribute iterator's full validation if/when the
        // caller actually reads the attribute.
        let stop = memchr3(b'=', b' ', b'\t', &bytes[i..n])
            .map(|o| i + o)
            .unwrap_or(n);
        // `\n` and `\r` also count as whitespace per XML 1.0 § 2.3
        // [3] [S], but those are rare in attribute-name positions in
        // real-world documents — fall back to a manual sweep if we
        // hit one before `=`.
        let stop = {
            let s = bytes[i..stop].iter().position(|&c| c == b'\n' || c == b'\r')
                .map(|o| i + o)
                .unwrap_or(stop);
            s
        };
        i = stop;
        let name_end = i;
        if name_end == name_start {
            return Err("expected attribute name".to_string());
        }

        // ── Eq ──────────────────────────────────────────────
        while i < n && is_ws(bytes[i]) { i += 1; }
        if i >= n || bytes[i] != b'=' {
            return Err("expected '=' after attribute name".to_string());
        }
        i += 1;
        while i < n && is_ws(bytes[i]) { i += 1; }

        // ── AttValue ────────────────────────────────────────
        if i >= n {
            return Err("expected quoted attribute value".to_string());
        }
        let q = bytes[i];
        if q != b'"' && q != b'\'' {
            return Err(format!(
                "attribute value must be quoted (got '{}'); XML 1.0 § 3.1 [41] [AttValue]",
                q as char
            ));
        }
        i += 1;
        // Scan to closing quote checking for forbidden chars.
        // Use memchr3 to SIMD-jump to the next interesting byte
        // (`<` / `&` / quote) rather than walking byte-by-byte —
        // gives the same throughput as the lazy iterator's value
        // scan for clean values that contain none of the three.
        loop {
            let tail = &bytes[i..n];
            let off = match memchr3(b'<', b'&', q, tail) {
                Some(o) => o,
                None    => return Err("unterminated attribute value".to_string()),
            };
            i += off;
            let b = bytes[i];
            if b == q { i += 1; break; }
            if b == b'<' {
                return Err(
                    "'<' not allowed in attribute value (XML 1.0 § 3.1 WFC: No < in Attribute Values)".to_string()
                );
            }
            if b == b'&' {
                // Must be either `&Name;` or `&#NNN;` / `&#xNN;`.
                let after = i + 1;
                if after >= n {
                    return Err("unterminated entity reference in attribute value".to_string());
                }
                let semi = bytes[after..].iter().position(|&c| c == b';' || c == b'<' || c == q);
                let semi_off = match semi {
                    Some(off) if bytes[after + off] == b';' => off,
                    _ => return Err(
                        "bare '&' in attribute value (must be an entity / character reference; XML 1.0 § 4.1)".to_string()
                    ),
                };
                let body = &bytes[after..after + semi_off];
                if body.is_empty() {
                    return Err("empty reference '&;' in attribute value".to_string());
                }
                if body[0] == b'#' {
                    // Numeric character reference.
                    let digits: &[u8] = if body.len() >= 2 && (body[1] == b'x' || body[1] == b'X') {
                        &body[2..]
                    } else { &body[1..] };
                    if digits.is_empty() {
                        return Err("empty numeric character reference".to_string());
                    }
                    let valid = if body.len() >= 2 && (body[1] == b'x' || body[1] == b'X') {
                        digits.iter().all(|&c| c.is_ascii_hexdigit())
                    } else {
                        digits.iter().all(|&c| c.is_ascii_digit())
                    };
                    if !valid {
                        return Err(format!("invalid character reference '&{};'",
                            String::from_utf8_lossy(body)));
                    }
                } else {
                    // Named entity reference.
                    if !opts.skip_name_validation {
                        let b0 = body[0];
                        if b0 < 0x80 && ASCII_XML_NAME[b0 as usize] & NS == 0 {
                            return Err(format!(
                                "invalid entity-reference name '&{};' in attribute value",
                                String::from_utf8_lossy(body)
                            ));
                        }
                        for &b in &body[1..] {
                            if b < 0x80 && ASCII_XML_NAME[b as usize] & (NS | NC) == 0 {
                                return Err(format!(
                                    "invalid entity-reference name '&{};' in attribute value",
                                    String::from_utf8_lossy(body)
                                ));
                            }
                        }
                    }
                    // XML 1.0 § 4.1 WFC: Entity Declared + § 4.1 WFC:
                    // No Recursion.  Any named entity must be declared,
                    // and recursion through nested entity references
                    // must not form a cycle.  Walk the chain and check
                    // both.
                    let name = std::str::from_utf8(body).unwrap_or("?");
                    let predef = matches!(name, "amp" | "lt" | "gt" | "quot" | "apos");
                    if !predef {
                        // XML 1.0 § 4.4.4 [Forbidden]: external
                        // entity references inside an attribute
                        // value are a fatal error (WFC: No External
                        // Entity References).  Catches `<doc a="&extE;">`.
                        // This rule is independent of libxml2_compat —
                        // libxml2 itself enforces it.
                        if entities.get(name).is_some_and(|d| d.kind.is_external_value()) {
                            return Err(format!(
                                "external entity '&{name};' is not allowed in an attribute value \
                                 (XML 1.0 § 4.4.4 WFC: No External Entity References)"
                            ));
                        }
                        // The cycle / replacement-text check requires
                        // a DTD entity table.  Documents with no DTD
                        // (the common case) reach here only with
                        // predefined entities, which are handled
                        // above; any other name is an error.  Skip
                        // the recursive walk when the table is empty.
                        if !entities.is_empty() {
                            let mut chain: Vec<&str> = Vec::new();
                            check_entity_chain(name, entities, &mut chain, inside_external)?;
                        } else if !inside_external {
                            return Err(format!(
                                "undefined entity '&{name};' (XML 1.0 § 4.1 WFC: Entity Declared)"
                            ));
                        }
                    }
                }
                i = after + semi_off + 1;
                continue;
            }
            i += 1;
        }

        // ── duplicate-name check (§ 3.1 WFC: Unique Att Spec) ─
        // Walk the inline-array prefix first, then the overflow Vec
        // (empty in the common case).  No heap traffic for tags
        // with <= 8 attributes.  Past SEEN_PROMOTE_THRESHOLD overflow
        // entries we flip to a hash set to avoid quadratic blowup on
        // adversarial inputs.
        let new_name = &bytes[name_start..name_end];
        let ns_u32 = name_start as u32;
        let ne_u32 = name_end as u32;
        for &(s, e) in &seen_inline[..seen_inline_len] {
            if &bytes[s as usize..e as usize] == new_name {
                return Err(format!(
                    "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                    String::from_utf8_lossy(new_name)
                ));
            }
        }
        if let Some(set) = seen_set.as_mut() {
            if !set.insert(new_name) {
                return Err(format!(
                    "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                    String::from_utf8_lossy(new_name)
                ));
            }
        } else {
            for &(s, e) in &seen_overflow {
                if &bytes[s as usize..e as usize] == new_name {
                    return Err(format!(
                        "duplicate attribute '{}' in start tag (XML 1.0 § 3.1 WFC: Unique Att Spec)",
                        String::from_utf8_lossy(new_name)
                    ));
                }
            }
            if seen_inline_len < seen_inline.len() {
                seen_inline[seen_inline_len] = (ns_u32, ne_u32);
                seen_inline_len += 1;
            } else {
                seen_overflow.push((ns_u32, ne_u32));
                if seen_overflow.len() >= SEEN_PROMOTE_THRESHOLD {
                    let mut set: FxHashSet<&[u8]> = FxHashSet::default();
                    set.reserve(seen_overflow.len() * 2);
                    for &(s, e) in &seen_overflow {
                        set.insert(&bytes[s as usize..e as usize]);
                    }
                    seen_set = Some(set);
                }
            }
        }
    }

    Ok(())
}

/// XML §3.3.3 attribute-value normalization (CDATA default).
///
/// `\t`, `\n`, `\r` are rewritten to a single `#x20` space; in XML 1.1
/// the same applies to NEL (`0xC2 0x85`) and LS (`0xE2 0x80 0xA8`).
/// All other bytes are copied through verbatim — entity references
/// and char references have already been resolved by the caller.
///
/// This is the *CDATA-default* form: it does not collapse runs or
/// trim leading/trailing spaces.  The non-CDATA pass (used by DTD-
/// or schema-typed attributes) layers on top of this in
/// `dtd_normalize_attr_value` and friends.
fn append_attr_normalized(src: &[u8], dst: &mut Vec<u8>, is_xml_11: bool) {
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let off = if is_xml_11 {
            // Five candidates need to be located in one pass.  memchr3
            // is a tight SIMD loop; a separate memchr2 for the 1.1
            // EOL leads picks up NEL / LS, and we take whichever match
            // comes first in the rest slice.
            let a = memchr3(b'\t', b'\n', b'\r', rest);
            let b = memchr::memchr2(0xC2, 0xE2, rest);
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (a, b) => a.or(b),
            }
        } else {
            memchr3(b'\t', b'\n', b'\r', rest)
        };
        let stop = match off {
            Some(o) => i + o,
            None => src.len(),
        };
        if stop > i {
            dst.extend_from_slice(&src[i..stop]);
        }
        if stop >= src.len() { break; }
        let b = src[stop];
        let consumed = match b {
            b'\t' | b'\n' | b'\r' => {
                dst.push(b' ');
                1
            }
            0xC2 if is_xml_11 && src.get(stop + 1) == Some(&0x85) => {
                dst.push(b' ');
                2
            }
            0xE2 if is_xml_11
                && src.get(stop + 1) == Some(&0x80)
                && src.get(stop + 2) == Some(&0xA8) =>
            {
                dst.push(b' ');
                3
            }
            _ => {
                // 0xC2 / 0xE2 lead that didn't continue into NEL/LS —
                // emit the byte verbatim.
                dst.push(b);
                1
            }
        };
        i = stop + consumed;
    }
}

/// Scan `bytes` for the first byte that triggers §3.3.3 attribute-
/// value normalization: `\t`, `\n`, `\r`, plus (XML 1.1) the UTF-8
/// lead of NEL or LS.  Returns `None` when the attribute is already
/// in normalized form — the lazy hot-path zero-copy check.
#[inline(always)]
fn first_attr_norm_byte(bytes: &[u8], is_xml_11: bool) -> Option<usize> {
    let in_1_0 = memchr3(b'\t', b'\n', b'\r', bytes);
    if !is_xml_11 || in_1_0.is_some() {
        return in_1_0;
    }
    memchr::memchr2(0xC2, 0xE2, bytes)
}

/// Apply §3.3.3 attribute-value normalization lazily to `raw`.
///
/// Returns `raw` unchanged when the value contains none of the
/// normalization-significant bytes — the common case for hand-
/// authored documents where attributes are written with literal
/// spaces only.
///
/// This is the **fast-path** wrapper, used when the attribute value
/// contained no `&` reference — every byte in `raw` is literal
/// source content and so §3.3.3 applies uniformly.  The slow path
/// (entity / char references present) builds its buffer incrementally
/// with [`append_attr_segment`] so that *character* references can
/// inject literal `\t` / `\n` / `\r` without those bytes being
/// rewritten away.
#[inline(always)]
fn maybe_normalize_attr_value<'a>(raw: Cow<'a, [u8]>, is_xml_11: bool) -> Cow<'a, [u8]> {
    if first_attr_norm_byte(&raw, is_xml_11).is_none() {
        return raw;
    }
    let mut out = Vec::with_capacity(raw.len());
    append_attr_normalized(&raw, &mut out, is_xml_11);
    Cow::Owned(out)
}

/// Copy `scan.cur_bytes()[start..end]` into `buf`, applying §3.3.3
/// CDATA-default normalization to *only the source-borrowed bytes*.
///
/// The slow path of `scan_att_value_cow` builds the attribute value
/// by alternating segments of source bytes with the results of
/// character-reference / entity-reference expansion.  XML §3.3.3
/// rewrites literal `\t` / `\n` / `\r` (and in XML 1.1 NEL / LS) to
/// space, but **does not** rewrite the same characters when they
/// arrive via a character reference — that's the whole reason
/// authors write `&#xA;` to inject a literal LF.  Confining the
/// rewrite to source segments preserves that contract.
#[inline(always)]
fn append_attr_segment(
    scan: &Scanner<'_, '_>,
    start: usize,
    end: usize,
    buf: &mut Vec<u8>,
    is_xml_11: bool,
) {
    let bytes = &scan.cur_bytes()[start..end];
    if first_attr_norm_byte(bytes, is_xml_11).is_none() {
        buf.extend_from_slice(bytes);
    } else {
        append_attr_normalized(bytes, buf, is_xml_11);
    }
}

/// One-shot pre-scan over the document source to decide whether any
/// §2.11 EOL rewriting is needed at all.  Returns `true` when the
/// source contains at least one `\r`, NEL (`0xC2 0x85`), or LS
/// (`0xE2 0x80 0xA8`).  The cheap memchr3 lead-byte sweep is paired
/// with a lookahead so that ordinary 2-/3-byte UTF-8 sequences whose
/// lead happens to be `0xC2` or `0xE2` don't pin the per-segment
/// scan flag — important for ASCII-with-accents documents where
/// `0xC2 …` shows up on every Latin-1-supplement character.
fn precompute_source_has_eol(src: &[u8]) -> bool {
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let off = match memchr3(b'\r', 0xC2, 0xE2, rest) {
            Some(o) => o,
            None => return false,
        };
        let pos = i + off;
        match src[pos] {
            b'\r' => return true,
            0xC2 if src.get(pos + 1) == Some(&0x85) => return true,
            0xE2 if src.get(pos + 1) == Some(&0x80)
                && src.get(pos + 2) == Some(&0xA8) =>
            {
                return true;
            }
            _ => i = pos + 1,
        }
    }
    false
}

/// Scan `bytes` for the first byte that *could* participate in an
/// XML §2.11 end-of-line normalization rewrite.
///
/// Under XML 1.0 the only trigger is `#xD` (`\r`).  Under XML 1.1
/// the trigger set widens to include the UTF-8 lead bytes of NEL
/// (`U+0085` = `0xC2 0x85`) and LS (`U+2028` = `0xE2 0x80 0xA8`).
///
/// Returns `None` when `bytes` can be emitted verbatim — the
/// common case in modern documents — letting the text hot-path
/// stay zero-copy.
#[inline(always)]
pub(crate) fn first_eol_byte(bytes: &[u8], is_xml_11: bool) -> Option<usize> {
    if is_xml_11 {
        memchr3(b'\r', 0xC2, 0xE2, bytes)
    } else {
        memchr(b'\r', bytes)
    }
}

/// Append `src` to `dst`, applying XML §2.11 end-of-line normalization:
///
/// * `\r\n` → `\n`
/// * `\r`   → `\n`
/// * (XML 1.1 only) `\r\x85` → `\n`,
///   `\xc2\x85` (NEL) → `\n`,
///   `\xe2\x80\xa8` (LS) → `\n`
///
/// Every other byte is appended verbatim.  Callers should consult
/// [`first_eol_byte`] first; when it returns `None`, this function
/// is a more-expensive `extend_from_slice` and should be skipped.
///
/// Internally walks `src` in runs: a SIMD memchr finds the next
/// EOL-candidate byte, the bytes before it are bulk-copied with
/// `extend_from_slice` (memcpy under the hood), then a single
/// EOL sequence is processed.  On documents with CRLF line endings
/// this drops the per-byte loop overhead of the naive version —
/// the dominant cost becomes the rewrite ratio itself.
pub(crate) fn append_eol_normalized(src: &[u8], dst: &mut Vec<u8>, is_xml_11: bool) {
    let mut i = 0;
    while i < src.len() {
        let rest = &src[i..];
        let stop = match first_eol_byte(rest, is_xml_11) {
            Some(off) => i + off,
            None => src.len(),
        };
        if stop > i {
            dst.extend_from_slice(&src[i..stop]);
        }
        if stop >= src.len() { break; }
        let b = src[stop];
        let consumed = if b == b'\r' {
            dst.push(b'\n');
            if src.get(stop + 1) == Some(&b'\n') {
                2
            } else if is_xml_11
                && src.get(stop + 1) == Some(&0xC2)
                && src.get(stop + 2) == Some(&0x85)
            {
                3
            } else {
                1
            }
        } else if is_xml_11
            && b == 0xC2
            && src.get(stop + 1) == Some(&0x85)
        {
            dst.push(b'\n');
            2
        } else if is_xml_11
            && b == 0xE2
            && src.get(stop + 1) == Some(&0x80)
            && src.get(stop + 2) == Some(&0xA8)
        {
            dst.push(b'\n');
            3
        } else {
            // 0xC2 / 0xE2 lead that didn't continue into NEL/LS —
            // emit the candidate byte verbatim and advance.
            dst.push(b);
            1
        };
        i = stop + consumed;
    }
}

/// Copy `scan.cur_bytes()[start..end]` into `buf`, applying XML §2.11
/// end-of-line normalization on the fly.  Falls back to a plain
/// `extend_from_slice` when the segment contains no normalization-
/// significant bytes (the common case).
///
/// `gate` is the reader's `source_has_eol_candidate` flag — when
/// false we know the document carries no `\r`, NEL, or LS anywhere
/// in source and can skip the per-segment SIMD scan altogether.
#[inline(always)]
pub(crate) fn append_text_segment(
    scan: &Scanner<'_, '_>,
    start: usize,
    end: usize,
    buf: &mut Vec<u8>,
    is_xml_11: bool,
    gate: bool,
) {
    let bytes = &scan.cur_bytes()[start..end];
    if !gate {
        buf.extend_from_slice(bytes);
        return;
    }
    if first_eol_byte(bytes, is_xml_11).is_none() {
        buf.extend_from_slice(bytes);
    } else {
        append_eol_normalized(bytes, buf, is_xml_11);
    }
}

/// Apply §2.11 end-of-line normalization to `raw` lazily: return the
/// input untouched when it contains no normalization-significant
/// byte, otherwise allocate an owned Vec and rewrite.
///
/// This is the hot-path wrapper used by the text and CDATA emitters
/// — modern documents take the borrowed path, paying only a SIMD
/// memchr scan over the segment.  Pass the reader's
/// `source_has_eol_candidate` flag as `gate`: when `false` the
/// per-segment scan is skipped (no EOL bytes are reachable in the
/// document).
#[inline(always)]
pub(crate) fn maybe_normalize_eol<'a>(
    raw: Cow<'a, [u8]>,
    is_xml_11: bool,
    gate: bool,
) -> Cow<'a, [u8]> {
    if !gate {
        return raw;
    }
    if first_eol_byte(&raw, is_xml_11).is_none() {
        return raw;
    }
    let mut out = Vec::with_capacity(raw.len());
    append_eol_normalized(&raw, &mut out, is_xml_11);
    Cow::Owned(out)
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
/// Byte-output sibling of [`reader::unescape`](crate::reader::unescape).
/// Expands the five XML predefined entity references and numeric character
/// references inside `bytes` (which must be valid UTF-8), returning the
/// decoded form.  Returns `Cow::Borrowed(bytes)` when no `&` appears.
///
/// Intended for callers using
/// [`ParseOptions::skip_entity_expansion`](crate::ParseOptions::skip_entity_expansion)
/// with `XmlBytesReader` who want to decode a specific text payload on
/// demand.  General entities declared in a DTD are *not* expanded — the
/// helper has no access to the document's entity table.
pub fn unescape_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    if memchr(b'&', bytes).is_none() {
        return Cow::Borrowed(bytes);
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let rest = &bytes[i + 1..];
        let semi = match memchr(b';', rest) {
            Some(n) if n <= 16 => n,
            _ => { out.push(b'&'); i += 1; continue; }
        };
        let name = &rest[..semi];
        match name {
            b"amp"  => out.push(b'&'),
            b"lt"   => out.push(b'<'),
            b"gt"   => out.push(b'>'),
            b"quot" => out.push(b'"'),
            b"apos" => out.push(b'\''),
            _ if name.starts_with(b"#") => {
                let cp: Option<u32> = if name.len() >= 2 && (name[1] == b'x' || name[1] == b'X') {
                    std::str::from_utf8(&name[2..]).ok()
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                } else {
                    std::str::from_utf8(&name[1..]).ok()
                        .and_then(|d| d.parse::<u32>().ok())
                };
                match cp.and_then(char::from_u32) {
                    Some(c) => {
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                    }
                    None => {
                        // Leave invalid char-ref as literal so a downstream
                        // strict parser can flag it.
                        out.push(b'&');
                        out.extend_from_slice(name);
                        out.push(b';');
                    }
                }
            }
            _ => {
                // Unknown entity — leave verbatim.
                out.push(b'&');
                out.extend_from_slice(name);
                out.push(b';');
            }
        }
        i += 1 + semi + 1;
    }
    Cow::Owned(out)
}

/// Byte-output entity-reference expansion used by `XmlBytesReader`'s slow-path.
/// Writes the expanded entity content into a `Vec<u8>` buffer (as UTF-8 bytes).
pub(crate) fn expand_reference_bytes(
    scan: &mut Scanner<'_, '_>,
    buf: &mut Vec<u8>,
    entities: &HashMap<String, EntityDecl>,
    used: &mut u64,
    budget: u64,
    element_depth: u32,
    // Recovery sink — callers in recover mode pass `Some(&mut vec)`;
    // strict-mode callers pass `None`.  When `Some`, we log
    // recoverable errors to it and continue with a best-effort
    // representation of the malformed reference (literal `&name;`
    // bytes left in `buf`).
    recovered: Option<&mut Vec<crate::error::XmlError>>,
    // XML 1.0 errata E13: when the internal DTD subset contained at
    // least one parameter-entity reference, an undeclared general
    // entity is a validity error, not a well-formedness error.
    // Caller passes `true` to silence the WF-level rejection so the
    // doc still parses; the missing decl gets dropped on the floor
    // (the reference expands to nothing).
    pe_relax_undefined: bool,
    // XML 1.0 § 2.9 + § 4.1 WFC: Entity Declared.  `true` when the
    // document declared `standalone='yes'`; references to entities
    // whose declaration came from the external subset are then
    // not-well-formed.
    standalone_yes: bool,
    // True when the host document declared `version="1.1"`; passed
    // through to any text-decl in external entities so 1.1 entities
    // are accepted when the host is 1.1.
    is_xml_11: bool,
) -> Result<()> {
    scan.expect(b'&')?;
    if scan.peek() == Some(b'#') {
        scan.advance();
        let cp: u32 = if scan.peek() == Some(b'x') {
            scan.advance();
            let start = scan.cur_pos();
            while scan.peek().map(|b| b.is_ascii_hexdigit()).unwrap_or(false) { scan.advance(); }
            if scan.cur_pos() == start { return Err(scan.err("empty hex character reference")); }
            let hex = scan.cur_str(start, scan.cur_pos());
            u32::from_str_radix(&hex, 16)
                .map_err(|_| scan.err(format!("invalid hex char ref: {hex}")))?
        } else {
            let start = scan.cur_pos();
            while scan.peek().map(|b| b.is_ascii_digit()).unwrap_or(false) { scan.advance(); }
            if scan.cur_pos() == start { return Err(scan.err("empty decimal character reference")); }
            let dec = scan.cur_str(start, scan.cur_pos());
            dec.parse::<u32>()
                .map_err(|_| scan.err(format!("invalid decimal char ref: {dec}")))?
        };
        scan.expect(b';')?;
        let c = char::from_u32(cp).ok_or_else(|| {
            scan.err(format!("U+{cp:04X} is not a valid Unicode scalar value"))
        })?;
        // XML 1.1 § 2.2 broadens the Char production to include the
        // C0 controls (#x1-#x1F) and #x7F-#x9F.  Char refs (which
        // unlike raw bytes are explicitly the legal way to spell
        // these in a document) MUST be accepted in a 1.1 document
        // even though 1.0 forbids them.
        let valid = if is_xml_11 { is_xml_11_char(c) } else { is_xml_char(c) };
        if !valid {
            let spec = if is_xml_11 { "XML 1.1 § 2.2" } else { "XML 1.0 § 2.2" };
            return Err(scan.err(format!("U+{cp:04X} is not a valid XML character ({spec})")));
        }
        // UTF-8-encode the char into the byte buffer.  For ASCII chars this
        // is one byte; for non-ASCII char-refs (e.g. `&#x4E2D;`) we emit
        // the same UTF-8 sequence `String::push(c)` would produce.
        let mut tmp = [0u8; 4];
        buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
    } else {
        let (ns, ne) = scan.scan_name_raw()?;
        scan.expect(b';')?;
        let name = scan.cur_str(ns, ne);
        match name.as_ref() {
            "amp"  => buf.push(b'&'),
            "lt"   => buf.push(b'<'),
            "gt"   => buf.push(b'>'),
            "quot" => buf.push(b'"'),
            "apos" => buf.push(b'\''),
            other  => {
                // Look the entity up.  Three categories:
                //   * `InternalText` / `ExternalLoaded` — we have
                //     replacement text; push it as a stream below.
                //   * `ExternalUnloaded` — declared external but the
                //     resolver wasn't configured or refused.  In
                //     libxml2-compat mode silently expand to empty;
                //     otherwise treated like an undefined name (the
                //     downstream pe_relax / recovery / strict error
                //     handling applies).
                //   * Not in the map — genuinely undefined.
                let decl = entities.get(other);
                let kind = decl.map(|d| &d.kind);
                // XML 1.0 § 2.9 + § 4.1 WFC: Entity Declared.  In a
                // `standalone="yes"` document, references to entities
                // whose declaration lived in the external subset (or
                // in an external PE's replacement text) are not-WF.
                // Predefined names (amp/lt/gt/quot/apos) and entities
                // we declared internally are fine.
                if standalone_yes
                    && decl.is_some_and(|d| d.declared_external)
                {
                    return Err(scan.err(format!(
                        "reference to entity '&{other};' declared in the external \
                         subset is not allowed in a standalone='yes' document \
                         (XML 1.0 § 2.9 / § 4.1 WFC: Entity Declared)"
                    )));
                }
                let frame_base = decl.and_then(|d| d.source_uri.clone());
                let is_external = matches!(kind,
                    Some(EntityKind::ExternalLoaded(_)) | Some(EntityKind::ExternalUnloaded));
                let value = match kind {
                    Some(EntityKind::InternalText(v))
                    | Some(EntityKind::ExternalLoaded(v)) => v.clone(),
                    Some(EntityKind::ExternalUnloaded) | None => {
                        // libxml2-compat treats unloaded externals as
                        // silently expanding to empty.
                        if scan.opts.libxml2_compat
                            && matches!(kind, Some(EntityKind::ExternalUnloaded))
                        {
                            return Ok(());
                        }
                        // XML 1.0 § 4.1 WFC: Entity Declared carve-out:
                        // refs that appear *inside* an external
                        // entity's replacement text are exempt — at
                        // most a VC violation, which non-validating
                        // parsers MUST tolerate.  Log a recoverable
                        // warning and expand to empty so the parse
                        // continues.
                        if scan.current_base_uri().is_some() {
                            if let Some(sink) = recovered {
                                sink.push(scan.err_with_level(
                                    crate::error::ErrorLevel::Warning,
                                    format!(
                                        "undefined entity '&{other};' inside an external \
                                         entity — WFC: Entity Declared carve-out applies \
                                         (XML 1.0 § 4.1); expansion skipped"
                                    ),
                                ));
                            }
                            return Ok(());
                        }
                        // XML 1.0 errata E13: if any PE reference
                        // appeared in the internal DTD subset, an
                        // undeclared general entity is a validity
                        // error, not WF.  Accept the doc and let
                        // the reference expand to nothing — the
                        // PE could have declared it, we can't tell.
                        if pe_relax_undefined {
                            return Ok(());
                        }
                        // Recovery (libxml2 XML_PARSE_RECOVER style):
                        // leave the reference literal in the buffer
                        // and continue.  The caller's text/attr
                        // value will contain `&name;` verbatim — a
                        // best-effort representation of input the
                        // parser couldn't resolve.
                        if scan.opts.recovery_mode {
                            if let Some(sink) = recovered {
                                sink.push(scan.err_with_level(
                                    crate::error::ErrorLevel::Error,
                                    format!(
                                        "undefined entity '&{other};' — left as literal text \
                                         (XML 1.0 § 4.1 WFC: Entity Declared)"
                                    ),
                                ));
                            }
                            buf.push(b'&');
                            buf.extend_from_slice(other.as_bytes());
                            buf.push(b';');
                            return Ok(());
                        }
                        return Err(scan.err(format!(
                            "undefined entity '&{other};' (XML 1.0 § 4.1 WFC: Entity Declared)"
                        )));
                    }
                };
                *used = used.saturating_add(value.len() as u64);
                if *used > budget {
                    return Err(scan.err(format!(
                        "entity expansion limit exceeded ({budget} bytes) — possible \
                         entity expansion attack (CVE-2003-1564)"
                    )));
                }
                scan.push_entity_stream(other.to_string(), value, element_depth, frame_base)?;
                if is_external {
                    // XML 1.0 §4.3.1: only external parsed entities
                    // may begin with a text declaration.  Validating
                    // it on internal entities would silently accept
                    // not-wf inputs like `<!ENTITY e "<?xml ?>">`.
                    consume_text_decl_if_present(scan, is_xml_11)?;
                }
            }
        }
    }
    Ok(())
}

/// Resolve a SYSTEM literal to a filesystem path (joining against
/// `base_url`'s parent directory for relative literals), read the
/// file, and return its bytes.  Used by external general-entity
/// loading; mirrors the path-resolution rule in
/// [`XmlBytesReader::load_external_subset`].
///
/// `http://` and `https://` URIs are rejected — v0.1 doesn't fetch
/// over the network.  `file://` prefixes are stripped.
/// Resolve a (possibly relative) SYSTEM identifier against a base URI
/// into an absolute URL string.
///
/// The parser is the authority on base URI semantics — see XML 1.0
/// § 4.2.2 + errata E18.  Doing the join here means every
/// [`EntityResolver`] implementation can stay a simple
/// "open-this-URL" function rather than re-implementing URI math.
///
/// Rules:
/// * If `system_id` already has a URI scheme (contains `"://"`) or
///   is an absolute filesystem path (starts with `/`), it's returned
///   verbatim — already absolute.
/// * If `base` is `None`, `system_id` is returned verbatim (best
///   effort — the resolver will handle relative paths as it sees fit).
/// * Otherwise the parent directory of `base` is joined with
///   `system_id` (preserving any `file://` scheme on `base`).
///
/// The result may still contain `..` segments — they're left for the
/// resolver's own canonicalisation step (e.g.
/// `FilesystemResolver::validate_path` calls `canonicalize`, which
/// resolves them and follows the security check on the result).
///
/// [`EntityResolver`]: crate::entity_resolver::EntityResolver
pub fn resolve_uri(system_id: &str, base: Option<&str>) -> String {
    // Already-absolute forms pass through.
    if system_id.contains("://") || system_id.starts_with('/') {
        return system_id.to_string();
    }
    let Some(base) = base else { return system_id.to_string(); };
    // Strip and re-attach `file://` so path joining works on raw
    // path components.  Non-file schemes already short-circuited above.
    let (scheme, base_path) = match base.strip_prefix("file://") {
        Some(rest) => ("file://", rest),
        None       => ("",         base),
    };
    let parent = std::path::Path::new(base_path).parent()
        .unwrap_or_else(|| std::path::Path::new(""));
    let joined = parent.join(system_id);
    // Display gives a `/`-separated string on Unix; on Windows it
    // would use `\` — acceptable for now since the resolvers operate
    // on `Path` and don't mind the separator.
    format!("{scheme}{}", joined.display())
}

fn read_external_entity_bytes(
    system_id: &str,
    base_url:  Option<&str>,
) -> std::result::Result<Vec<u8>, String> {
    if system_id.starts_with("http://") || system_id.starts_with("https://") {
        return Err("network URIs not supported".to_string());
    }
    let raw: &str = system_id.strip_prefix("file://").unwrap_or(system_id);
    let resolved: std::path::PathBuf = {
        let pb = std::path::Path::new(raw);
        if pb.is_absolute() {
            pb.to_path_buf()
        } else if let Some(base) = base_url {
            let base_path = std::path::Path::new(base.strip_prefix("file://").unwrap_or(base));
            match base_path.parent() {
                Some(dir) => dir.join(pb),
                None      => pb.to_path_buf(),
            }
        } else {
            pb.to_path_buf()
        }
    };
    std::fs::read(&resolved).map_err(|e| e.to_string())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // SAFETY in this test module: every byte slice handed to `as_utf8`
    // came from a `&'static str` literal we passed into the reader, so
    // it's valid UTF-8 by construction.  Used only for readable error
    // messages in test asserts.
    fn as_utf8(b: &[u8]) -> &str { std::str::from_utf8(b).unwrap() }

    fn events(src: &str) -> Vec<String> {
        let mut r = XmlBytesReader::from_str(src);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        loop {
            match r.next_into(&mut buf).unwrap() {
                BytesEventInto::StartElement { name } => {
                    let a: Vec<_> = buf.iter()
                        .map(|a| format!("{}={}", as_utf8(&a.name), as_utf8(&a.value)))
                        .collect();
                    let name = as_utf8(&name);
                    if a.is_empty() { out.push(format!("<{name}>")); }
                    else            { out.push(format!("<{name} {}>", a.join(" "))); }
                }
                BytesEventInto::EndElement { name }  => out.push(format!("</{}>", as_utf8(&name))),
                BytesEventInto::Text(t)              => out.push(format!("T:{}", as_utf8(&t))),
                BytesEventInto::CData(s)             => out.push(format!("CD:{}", as_utf8(&s))),
                BytesEventInto::Comment(s)           => out.push(format!("C:{}", as_utf8(&s))),
                BytesEventInto::Pi { target, .. }    => out.push(format!("PI:{}", as_utf8(&target))),
                BytesEventInto::EntityRef { name }   => out.push(format!("E:{}", as_utf8(&name))),
                BytesEventInto::Eof                  => break,
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
        let mut r = XmlBytesReader::from_str(src);
        let mut buf = Vec::new();
        let ev = r.next_into(&mut buf).unwrap();
        assert!(matches!(&ev, BytesEventInto::StartElement { name } if &name[..] == b"el"));
        assert_eq!(buf.len(), 2);
        assert!(matches!(buf[0].value, Cow::Borrowed(_)), "no entity → should borrow");
    }

    #[test]
    fn attribute_entity_owned() {
        let src = r#"<el v="a&amp;b"/>"#;
        let mut r = XmlBytesReader::from_str(src);
        let mut buf = Vec::new();
        r.next_into(&mut buf).unwrap();
        assert_eq!(&*buf[0].value(), b"a&b");
        assert!(matches!(buf[0].value, Cow::Owned(_)), "entity → must allocate");
    }

    #[test]
    fn text_borrowed() {
        let src = "<r>hello world</r>";
        let mut r = XmlBytesReader::from_str(src);
        let mut buf = Vec::new();
        r.next_into(&mut buf).unwrap(); // StartElement
        let ev = r.next_into(&mut buf).unwrap();
        match ev {
            BytesEventInto::Text(Cow::Borrowed(b)) => assert_eq!(b, b"hello world"),
            other => panic!("expected borrowed Text, got {other:?}"),
        }
    }

    /// Sanity-check the lazy contract end-to-end: pattern-match on the
    /// new wrapper variants and call methods to extract data.  Pure
    /// behavioural test — covered by other tests too, but kept here as
    /// the explicit "the new shape works" smoke test.
    #[test]
    fn lazy_event_methods_smoke_test() {
        let mut r = XmlBytesReader::from_str(
            "<root><!-- c --><![CDATA[cd]]><?p y?><a x=\"1\">hi</a></root>"
        );
        let mut got: Vec<String> = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::StartElement(tag) => {
                    let attrs: Vec<_> = tag.attrs()
                        .map(|a| {
                            let a = a.unwrap();
                            format!("{}={}", as_utf8(a.name), as_utf8(&a.value))
                        })
                        .collect();
                    let name_part = format!("<{}", as_utf8(
                        // tag was consumed by attrs(); use the saved attrs vec for the assertion shape
                        b"_"
                    ));
                    // Using the attrs vec is the pattern-match-safe way
                    // to keep both name and attrs; the lazy API forces
                    // a choice via attrs() consuming the tag.  Real
                    // users would call `let name = tag.name(); let _ = tag.attrs();`.
                    let _ = name_part;
                    if attrs.is_empty() { got.push("<a>".into()); }
                    else                { got.push(format!("<a {}>", attrs.join(" "))); }
                }
                BytesEvent::EndElement(tag) => got.push(format!("</{}>", as_utf8(tag.name()))),
                BytesEvent::Text(t)         => got.push(format!("T:{}", as_utf8(t.as_bytes()))),
                BytesEvent::CData(s)        => got.push(format!("CD:{}", as_utf8(s.as_bytes()))),
                BytesEvent::Comment(s)      => got.push(format!("C:{}", as_utf8(s.as_bytes()))),
                BytesEvent::Pi(p)           => got.push(format!("PI:{}", as_utf8(p.target()))),
                BytesEvent::EntityRef(e)    => got.push(format!("E:{}", as_utf8(e.name()))),
                BytesEvent::Eof             => break,
            }
        }
        // The first StartElement we discarded the name (because attrs()
        // consumed `tag`); we just check shape and content of the rest.
        assert!(got.iter().any(|s| s == "C: c "));
        assert!(got.iter().any(|s| s == "CD:cd"));
        assert!(got.iter().any(|s| s == "PI:p"));
        assert!(got.iter().any(|s| s == "<a x=1>"));
        assert!(got.iter().any(|s| s == "T:hi"));
        assert!(got.iter().any(|s| s == "</a>"));
        assert!(got.iter().any(|s| s == "</root>"));
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
        let mut r = XmlBytesReader::from_str(&src);
        let mut buf: Vec<BytesAttr> = Vec::new();
        let cap_before;
        loop {
            match r.next_into(&mut buf).unwrap() {
                BytesEventInto::StartElement { name } if &name[..] == b"a" => {
                    cap_before = buf.capacity();
                    break;
                }
                BytesEventInto::Eof => panic!("unexpected EOF"),
                _ => {}
            }
        }
        loop {
            match r.next_into(&mut buf).unwrap() {
                BytesEventInto::StartElement { name } if &name[..] == b"b" => {
                    assert_eq!(buf.capacity(), cap_before, "capacity should not grow for same-size attrs");
                    break;
                }
                BytesEventInto::Eof => panic!("unexpected EOF"),
                _ => {}
            }
        }
    }

    #[test]
    fn lazy_attrs_iter() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlBytesReader::from_str(src);
        match r.next().unwrap() {
            BytesEvent::StartElement(tag) => {
                assert_eq!(tag.name(), b"el");
                let pairs: Vec<(Vec<u8>, Vec<u8>)> = tag.attrs()
                    .map(|a| a.map(|a| (a.name.to_vec(), a.value.into_owned())).unwrap())
                    .collect();
                assert_eq!(pairs, vec![
                    (b"id".to_vec(),    b"1".to_vec()),
                    (b"class".to_vec(), b"x".to_vec()),
                ]);
            }
            _ => panic!("expected StartElement"),
        }
    }

    #[test]
    fn lazy_attrs_skipped_costs_nothing() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlBytesReader::from_str(src);
        match r.next().unwrap() {
            BytesEvent::StartElement(tag) => assert_eq!(tag.name(), b"el"),
            _ => panic!(),
        }
        match r.next().unwrap() {
            BytesEvent::EndElement(tag) => assert_eq!(tag.name(), b"el"),
            _ => panic!(),
        }
        match r.next().unwrap() {
            BytesEvent::Eof => {}
            _ => panic!(),
        }
    }

    // ── regression: skip_end_tag_check must NOT swallow whitespace ──
    //
    // Earlier, depth incremented only when `track_stack` was true (which
    // is `!skip_end_tag_check`), so turning skip_end_tag_check on left
    // depth at 0 forever, and the `if depth == 0 { skip_ws() }` path in
    // `next()` ran on every call — silently eating every inter-element
    // whitespace text event.  These tests pin the correct behaviour:
    // depth tracking is independent of end-tag enforcement, and
    // whitespace text events are emitted in both modes.

    fn count_text_events(src: &str, opts: ParseOptions) -> (u32, u32) {
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src.as_bytes()) }
            .with_options(opts);
        let (mut total, mut ws) = (0u32, 0u32);
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => return (total, ws),
                BytesEvent::Text(t) => {
                    total += 1;
                    if !t.as_bytes().is_empty()
                        && t.as_bytes().iter().all(|b| b.is_ascii_whitespace())
                    {
                        ws += 1;
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn whitespace_preserved_with_default_options() {
        // Baseline: depth tracking on, whitespace text events emitted.
        let src = "<root>\n  <a/>\n  <b/>\n</root>";
        let (total, ws) = count_text_events(src, ParseOptions::default());
        assert_eq!(ws, 3, "default mode should emit 3 whitespace text events");
        assert_eq!(total, 3);
    }

    #[test]
    fn whitespace_preserved_with_skip_end_tag_check() {
        // Regression: skip_end_tag_check used to silently swallow
        // whitespace.  With the fix, the same 3 whitespace text events
        // appear regardless of the end-tag flag.
        let src = "<root>\n  <a/>\n  <b/>\n</root>";
        let opts = ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() };
        let (total, ws) = count_text_events(src, opts);
        assert_eq!(ws, 3,
            "skip_end_tag_check should NOT swallow inter-element whitespace");
        assert_eq!(total, 3);
    }

    #[test]
    fn skip_end_tag_check_still_disables_end_tag_match() {
        // The flag must still do its real job: accept mismatched
        // end tags without erroring.
        let opts = ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() };
        let mut r = XmlBytesReader::from_str("<a></b>").with_options(opts);
        // Should NOT error on mismatched a/b.
        let mut events = 0u32;
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                _ => events += 1,
            }
        }
        assert!(events >= 2, "got both Start and End despite mismatch");
    }

    #[test]
    fn top_level_whitespace_still_skipped_in_both_modes() {
        // Whitespace BETWEEN top-level constructs (depth 0) was always
        // skipped — that part of the behaviour is correct and stays.
        // This is whitespace before the root and after the root close.
        let src = "  \n  <root/>  \n  ";
        for opts in [
            ParseOptions::default(),
            ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() },
        ] {
            let (total, ws) = count_text_events(src, opts.clone());
            assert_eq!(ws, 0, "no whitespace text events at depth 0 (opts: {opts:?})");
            assert_eq!(total, 0);
        }
    }

    // ── XML 1.0 § 2.1 / § 3.1 / § 2.8 well-formedness ───────────────
    //
    // These tests pin the bug fixes for the structural well-formedness
    // checks that were missing.  Each one of these inputs is forbidden
    // by XML 1.0 — every other major XML parser (libxml2, roxmltree,
    // xml-rs) rejects them; we used to accept them.  See the parallel
    // tests in `crates/bench/benches/text_validation_check.rs`.

    fn parse_all(src: &str, opts: ParseOptions) -> Result<u32> {
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src.as_bytes()) }
            .with_options(opts);
        let mut n = 0u32;
        loop {
            match r.next()? {
                BytesEvent::Eof => return Ok(n),
                _ => n += 1,
            }
        }
    }

    #[test]
    fn rejects_unclosed_element_at_eof() {
        // XML 1.0 § 3.1: every start tag must have a matching end
        // tag.  `<r><x>` ends with `<x>` still open.
        let err = parse_all("<r><x>", ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("unclosed"),
            "expected 'unclosed' in error, got: {err}");
    }

    #[test]
    fn unclosed_at_eof_relaxed_under_skip_end_tag_check() {
        // The opt-in flag relaxes the structural check for callers
        // streaming partial fragments.
        let opts = ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() };
        assert!(parse_all("<r><x>", opts).is_ok());
    }

    #[test]
    fn recover_mode_synthesises_closes_for_unclosed_at_eof() {
        // recovery_mode: true → unclosed elements at EOF
        // become synthetic EndElement events, and the errors are
        // collected on the reader.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r><a><b>") }
            .with_options(opts);
        let mut closed: Vec<Vec<u8>> = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::EndElement(tag) => closed.push(tag.name().to_vec()),
                _ => {}
            }
        }
        // Three start tags → three synthetic closes, in
        // innermost-first order (b, a, r).
        assert_eq!(
            closed.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
            vec!["b".to_string(), "a".to_string(), "r".to_string()],
        );
        // Three errors logged, one per unclosed element.
        let errs = r.recovered_errors();
        assert_eq!(errs.len(), 3, "got {} errors", errs.len());
        assert!(errs.iter().all(|e| e.message.contains("unclosed")),
            "expected 'unclosed' in every recovery message");
    }

    #[test]
    fn recover_mode_logs_empty_doc_error_but_continues() {
        // An empty document logs the "no root element" error in
        // recover mode and returns Eof immediately; nothing to
        // synthesise.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"") }
            .with_options(opts);
        assert!(matches!(r.next().unwrap(), BytesEvent::Eof));
        let errs = r.recovered_errors();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].message.contains("no root element"));
    }

    #[test]
    fn strict_mode_errors_unchanged_in_phase1() {
        // Sanity: turning recovery_mode OFF (default) keeps
        // every error fatal.  No regression vs pre-recovery
        // behaviour.
        assert!(parse_all("<r><x>", ParseOptions::default()).is_err());
    }

    #[test]
    fn recover_mode_walks_stack_for_mismatched_end_tag() {
        // libxml2-style recovery: `<a><b><c></a>` — `</a>` doesn't
        // match the top of stack `c`, but `a` IS on the stack at
        // depth 0.  Synthesise closes for c and b, then close a.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<a><b><c></a>") }
            .with_options(opts);
        let mut closed: Vec<Vec<u8>> = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::EndElement(tag) => closed.push(tag.name().to_vec()),
                _ => {}
            }
        }
        assert_eq!(
            closed.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
            vec!["c".to_string(), "b".to_string(), "a".to_string()],
            "expected innermost-first synth closes then real `</a>`",
        );
        // 2 errors logged: the first for top=c expected, got=a;
        // the second for top=b expected, got=a.
        // (When the cursor finally aligns at depth 0 with `</a>`
        // matching top=a, we stop logging.)
        let errs = r.recovered_errors();
        assert!(errs.len() >= 1, "got {} errors", errs.len());
        assert!(errs.iter().all(|e| e.message.contains("mismatched")),
            "expected 'mismatched' in recovery messages");
    }

    #[test]
    fn recover_mode_drops_orphan_end_tag() {
        // `</orphan>` with no open element → log error, discard
        // the tag, continue with the rest of the document.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r></orphan></r>") }
            .with_options(opts.clone());
        // Element-stack now has `r`.  We see `</orphan>`, which
        // doesn't match `r` — that's "mismatched", not "orphan".
        // The orphan path fires when the stack is EMPTY at the
        // moment of an unmatched end tag — exercise via:
        let mut r2 = unsafe { XmlBytesReader::from_bytes_unchecked(b"</orphan><r/>") }
            .with_options(opts);
        let mut events = 0u32;
        loop {
            match r2.next().unwrap() {
                BytesEvent::Eof => break,
                _ => events += 1,
            }
        }
        assert!(events >= 2, "got {} events from `</orphan><r/>`", events);
        let errs = r2.recovered_errors();
        assert!(errs.iter().any(|e| e.message.contains("orphan")
                              || e.message.contains("no open")),
            "expected orphan/no-open error, got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>());
        // (Suppress unused-warning for r — kept for future
        // expansion.)
        let _ = r;
    }

    #[test]
    fn recover_mode_accepts_second_root() {
        // `<a/><b/>` — XML 1.0 § 2.1 forbids two root elements.  In
        // recover mode, log the error and accept anyway.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<a/><b/>") }
            .with_options(opts);
        let mut starts: Vec<Vec<u8>> = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::StartElement(t) => starts.push(t.name().to_vec()),
                _ => {}
            }
        }
        assert_eq!(
            starts.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()],
            "both root elements should be emitted in recover mode",
        );
        let errs = r.recovered_errors();
        assert!(errs.iter().any(|e| e.message.contains("one root")),
            "expected 'one root' error, got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>());
    }

    #[test]
    fn recover_mode_keeps_bare_lt_literal_in_text() {
        // Bare `<` followed by non-name-start in text content —
        // strict rejects, recover keeps the `<` literal across
        // a sequence of Text events.  Better than libxml2 which
        // silently drops the `<`.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r>1 < 2</r>") }
            .with_options(opts);
        let mut text = String::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => text.push_str(&String::from_utf8_lossy(t.as_bytes())),
                _ => {}
            }
        }
        assert_eq!(text, "1 < 2",
            "all bytes preserved (libxml2 would silently drop the '<')");
        assert!(r.recovered_errors().iter().any(|e| e.message.contains("bare '<'")),
            "expected bare-< error logged");
    }

    #[test]
    fn recover_mode_accepts_text_before_root() {
        // `hello<r/>` — text at doc level is forbidden, but in
        // recover mode we emit it as a Text event so the user
        // can see what was there.  Better than libxml2 which
        // sometimes loses the root element entirely.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"hello<r/>") }
            .with_options(opts);
        let mut events: Vec<String> = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) =>
                    events.push(format!("Text({:?})",
                        String::from_utf8_lossy(t.as_bytes()))),
                BytesEvent::StartElement(t) =>
                    events.push(format!("Start({})",
                        String::from_utf8_lossy(t.name()))),
                BytesEvent::EndElement(t) =>
                    events.push(format!("End({})",
                        String::from_utf8_lossy(t.name()))),
                _ => {}
            }
        }
        assert!(events.iter().any(|e| e.contains("Text(\"hello\")")),
            "expected leading text preserved, events: {:?}", events);
        assert!(events.iter().any(|e| e.contains("Start(r)")),
            "expected root start emitted, events: {:?}", events);
        assert!(r.recovered_errors().iter().any(|e| e.message.contains("document level")),
            "expected doc-level error logged");
    }

    #[test]
    fn recover_mode_accepts_text_after_root() {
        // `<r/>trailing text` — text after the root close is
        // also a violation, also preserved in recover mode.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r/>trailing text") }
            .with_options(opts);
        let mut text = String::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => text.push_str(&String::from_utf8_lossy(t.as_bytes())),
                _ => {}
            }
        }
        assert_eq!(text, "trailing text",
            "trailing text preserved in recover mode");
    }

    #[test]
    fn recover_mode_keeps_cdata_close_literal_in_text() {
        // `]]>` in text — strict mode rejects, recover mode keeps
        // the bytes literal in the text payload.  Better than
        // libxml2's behaviour (which silently mangles surrounding
        // text).
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r>oops]]>more</r>") }
            .with_options(opts);
        let mut text: Option<Vec<u8>> = None;
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => text = Some(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        let text = text.expect("expected a text event");
        assert_eq!(
            String::from_utf8_lossy(&text), "oops]]>more",
            "all bytes preserved literal in recover mode",
        );
        assert!(r.recovered_errors().iter().any(|e| e.message.contains("]]>")),
            "expected ']]>' error logged");
    }

    #[test]
    fn recover_mode_keeps_bare_amp_literal_in_text() {
        // Bare `&` in text — strict mode rejects (must be
        // `&amp;` or a real reference), recover mode keeps it
        // literal.  Better than libxml2 which DROPS the `&`.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r>tom & jerry</r>") }
            .with_options(opts);
        let mut text: Option<Vec<u8>> = None;
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => text = Some(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        let text = text.expect("expected a text event");
        assert_eq!(
            String::from_utf8_lossy(&text), "tom & jerry",
            "bare '&' preserved in recover mode (libxml2 silently drops it)",
        );
        assert!(r.recovered_errors().iter().any(|e| e.message.contains("bare '&'")),
            "expected bare-& error logged");
    }

    #[test]
    fn recover_mode_skips_malformed_xml_decl() {
        // `<?xml?>` — missing version.  Strict rejects; recover
        // logs and resyncs past `?>` so the rest of the document
        // parses normally.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<?xml?><r>ok</r>") }
            .with_options(opts);
        let mut starts: Vec<Vec<u8>> = Vec::new();
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(BytesEvent::StartElement(t)) => starts.push(t.name().to_vec()),
                Ok(_) => {}
                Err(e) => panic!("next() errored unexpectedly in recover mode: {e}"),
            }
        }
        assert_eq!(
            starts.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect::<Vec<_>>(),
            vec!["r".to_string()],
            "root element should still parse after malformed decl",
        );
        assert!(r.recovered_errors().iter().any(|e| e.message.to_lowercase().contains("xml decl")
                                              || e.message.contains("XMLDecl")
                                              || e.message.contains("version")),
            "expected XML-decl error logged, got: {:?}",
            r.recovered_errors().iter().map(|e| &e.message).collect::<Vec<_>>());
    }

    /// XML 1.0 5th-edition (the default) accepts CJK combining marks
    /// like `U+309A` as a NameStartChar via the `U+3001..=U+D7FF`
    /// range.  XML 1.0 4th-edition rejects them — they belong to the
    /// removed `CombiningChar` production, not `Letter`.  Same input
    /// flips outcome based on `ParseOptions::xml10_fourth_edition`,
    /// matching libxml2's `XML_PARSE_OLD10` behaviour.
    #[test]
    fn name_start_combining_mark_accepted_5e_rejected_4e() {
        let src = "<\u{309A}/>".as_bytes().to_vec();

        // 5th edition (modern default) — accept.
        let mut r5 = unsafe { XmlBytesReader::from_bytes_unchecked(&src) };
        let mut hit_start_5e = false;
        loop {
            match r5.next().expect("5th-edition parse should accept U+309A") {
                BytesEvent::Eof => break,
                BytesEvent::StartElement(_) => hit_start_5e = true,
                _ => {}
            }
        }
        assert!(hit_start_5e, "5th-edition: U+309A should open a Start event");

        // 4th edition (opt-in) — reject as invalid NameStartChar.
        let opts = ParseOptions {
            xml10_fourth_edition: true,
            ..ParseOptions::default()
        };
        let mut r4 = unsafe { XmlBytesReader::from_bytes_unchecked(&src) }
            .with_options(opts);
        let err = loop {
            match r4.next() {
                Ok(BytesEvent::Eof) => panic!("4th-edition: should have rejected U+309A name start"),
                Ok(_)               => continue,
                Err(e)              => break e,
            }
        };
        assert!(err.message.contains("name-start") || err.message.contains("name start")
             || err.message.to_lowercase().contains("invalid name"),
            "4th-edition rejection should mention name-start invalidity, got: {}",
            err.message);
    }

    /// `U+00B7` (middle dot) is excluded from NameStartChar in BOTH
    /// editions — 4e tags it as `Extender` (NameChar only, not
    /// NameStartChar); 5e's NameStartChar ranges similarly exclude
    /// it.  Parser rejection should be edition-independent.
    #[test]
    fn name_start_middle_dot_rejected_in_both_editions() {
        let src = "<\u{00B7}/>".as_bytes().to_vec();

        for fourth in [false, true] {
            let opts = ParseOptions { xml10_fourth_edition: fourth, ..ParseOptions::default() };
            let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(&src) }
                .with_options(opts);
            let err = loop {
                match r.next() {
                    Ok(BytesEvent::Eof) => panic!("edition={fourth}: U+00B7 should be rejected"),
                    Ok(_)               => continue,
                    Err(e)              => break e,
                }
            };
            assert!(err.message.to_lowercase().contains("name"),
                "edition={fourth}: rejection should mention name, got: {}", err.message);
        }
    }

    #[test]
    fn recover_mode_leaves_undefined_entity_literal() {
        // `&xyz;` is undefined.  In recover mode the text event
        // contains `&xyz;` verbatim and the error is logged.
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(b"<r>before &xyz; after</r>") }
            .with_options(opts);
        let mut text_seen: Option<Vec<u8>> = None;
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => text_seen = Some(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        let text = text_seen.expect("expected a text event");
        assert!(text.windows(5).any(|w| w == b"&xyz;"),
            "expected `&xyz;` literal in text, got: {:?}",
            String::from_utf8_lossy(&text));
        let errs = r.recovered_errors();
        assert!(errs.iter().any(|e| e.message.contains("undefined entity")
                              && e.message.contains("xyz")),
            "expected undefined-entity error, got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>());
    }

    // ── external entity resolver wiring ────────────────────────

    #[test]
    fn external_entity_with_resolver_loads_replacement_text() {
        // With an InMemoryResolver mapping the system_id, an
        // external entity reference expands to the resolver's
        // bytes — same code path as if it were declared inline.
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;

        let resolver = Arc::new(
            InMemoryResolver::new()
                .with_system("file:///fake/foo.ent", b"hello world".to_vec())
        );
        let opts = ParseOptions {
            external_resolver: Some(resolver),
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);
        let mut got: Option<Vec<u8>> = None;
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) if t.as_bytes() != b"\n        "
                                    && !t.as_bytes().iter().all(u8::is_ascii_whitespace)
                                    => got = Some(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        let text = got.expect("expected text event with resolved content");
        assert_eq!(String::from_utf8_lossy(&text), "hello world",
            "resolver bytes should be the replacement text");
    }

    #[test]
    fn external_entity_with_markup_replacement_text_parses_as_subtree() {
        // External entity whose replacement text contains element
        // markup (rather than plain text) should expand into a
        // sub-tree: the parser sees `&foo;` and continues reading
        // `<evil>XML</evil>` from the pushed entity stream, surfacing
        // a Start event, the inner Text, and a balanced End event.
        // This exercises element_stack handling across the entity
        // boundary — the start tag was scanned from inside the
        // entity, so its source-offset would be meaningless against
        // the document's `src_bytes()`.  Pre-fix this surfaced as
        // "mismatched end tag: expected '</!DOC>', got '</oc [>'".
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;

        let resolver = Arc::new(
            InMemoryResolver::new()
                .with_system("file:///fake/foo.ent", b"<evil>XML</evil>".to_vec())
        );
        let opts = ParseOptions {
            external_resolver: Some(resolver),
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);

        // Count structurally — entity-stream-pushed start/end tags
        // expose empty name bytes through the byte reader (the
        // element_stack still tracks them by name internally, which
        // is what end-tag matching needs).  We assert the shape
        // of the event stream: <doc> Start, then a Start from
        // inside the entity, a Text "XML", a balancing End for the
        // entity-pushed Start, then </doc> End.
        let mut starts = 0;
        let mut ends   = 0;
        let mut texts: Vec<Vec<u8>> = Vec::new();
        loop {
            match r.next().expect("parse should succeed with markup-bearing entity") {
                BytesEvent::Eof => break,
                BytesEvent::StartElement(_) => starts += 1,
                BytesEvent::EndElement(_)   => ends   += 1,
                BytesEvent::Text(t)         => texts.push(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        assert_eq!(starts, 2, "expected <doc> and entity-pushed <evil> as Start events");
        assert_eq!(ends,   2, "expected balancing End events for both");
        assert!(texts.iter().any(|t| t == b"XML"),
            "expected `XML` Text event from inside the entity, got: {texts:?}");
    }

    #[test]
    fn external_entity_without_resolver_errors_on_reference() {
        // Without a resolver (default), the SYSTEM-declared entity
        // is recorded but never loaded — referencing it errors as
        // "undefined entity" because the entities map doesn't
        // have it.
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let err = parse_all(std::str::from_utf8(src).unwrap(),
                            ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("undefined entity")
             || err.to_string().contains("foo"),
            "expected undefined-entity error, got: {err}");
    }

    #[test]
    fn external_entity_resolver_refused_propagates_error() {
        // A *referenced* external general entity whose resolver refuses
        // → parser surfaces the failure as an error in strict mode.
        // (Loading is lazy per XML 1.0 § 4.4.3, so the entity must be
        // referenced for the resolver to run at all.)
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;
        // Empty InMemoryResolver refuses every resolve.
        let opts = ParseOptions {
            external_resolver: Some(Arc::new(InMemoryResolver::new())),
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);
        let mut hit_err = None;
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => continue,
                Err(e) => { hit_err = Some(e); break; }
            }
        }
        let e = hit_err.expect("resolver-refused entity reference should error in strict mode");
        assert!(e.to_string().contains("resolver"),
            "expected resolver-related error, got: {e}");
    }

    #[test]
    fn unreferenced_external_entity_is_not_loaded() {
        // XML 1.0 § 4.4.3 + XXE hardening: declaring an external general
        // entity must not, on its own, trigger the resolver — only a
        // reference does.  An unreferenced entity whose resolver would
        // refuse (or whose target is missing) therefore parses cleanly.
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;
        let opts = ParseOptions {
            external_resolver: Some(Arc::new(InMemoryResolver::new())),
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc/>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => continue,
                Err(e) => panic!("unreferenced external entity must not load: {e}"),
            }
        }
        assert!(r.recovered_errors().iter().all(|e| !e.message.contains("resolver")),
            "no resolver should run for an unreferenced external entity");
    }

    /// Soundness regression: a safe-Rust `EntityResolver` impl that
    /// returns bytes that aren't valid UTF-8 must NOT trigger UB via
    /// `String::from_utf8_unchecked`.  The parser must surface a
    /// clean error and keep the in-memory `String`s valid.
    ///
    /// The trait signature is `Result<Vec<u8>, _>` — nothing in the
    /// type system enforces UTF-8, and a perfectly safe impl (e.g.
    /// one that fetches bytes over the network and forgot to decode
    /// the charset) can produce non-UTF-8 input.  The parser owns
    /// the validation, not the resolver author.
    #[test]
    fn external_entity_resolver_invalid_utf8_propagates_error() {
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;
        // 0x80 is a UTF-8 continuation byte with no leading byte —
        // unambiguously invalid UTF-8 in any position.
        let resolver = Arc::new(
            InMemoryResolver::new()
                .with_system("file:///fake/foo.ent", vec![0x80, 0x80, 0x80])
        );
        let opts = ParseOptions {
            external_resolver: Some(resolver),
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);
        let mut hit_err: Option<XmlError> = None;
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => continue,
                Err(e) => { hit_err = Some(e); break; }
            }
        }
        let e = hit_err.expect(
            "resolver returning non-UTF-8 bytes must surface a parse error, \
             not silently UB-coerce into a String"
        );
        let msg = e.to_string();
        assert!(
            msg.to_lowercase().contains("utf-8") || msg.to_lowercase().contains("utf8"),
            "expected UTF-8-related error, got: {msg}"
        );
    }

    #[test]
    fn external_entity_resolver_refused_recovered() {
        // Recovery mode: a *referenced* entity whose resolver refuses
        // logs the error and the parse continues (the reference expands
        // to nothing).  Loading is lazy, so the entity is referenced
        // here to drive the resolver.
        use crate::entity_resolver::InMemoryResolver;
        use std::sync::Arc;
        let opts = ParseOptions {
            external_resolver: Some(Arc::new(InMemoryResolver::new())),
            recovery_mode: true,
            ..ParseOptions::default()
        };
        let src = br#"<!DOCTYPE doc [
            <!ENTITY foo SYSTEM "file:///fake/foo.ent">
        ]>
        <doc>&foo;</doc>"#;
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src) }
            .with_options(opts);
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                _ => {}
            }
        }
        assert!(r.recovered_errors().iter().any(|e| e.message.contains("resolver")),
            "expected resolver error in recovered list");
    }

    #[test]
    fn rejects_two_root_elements() {
        // XML 1.0 § 2.1 [document]: exactly one root.
        let err = parse_all("<a/><b/>", ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("one root"),
            "expected 'one root' in error, got: {err}");
    }

    #[test]
    fn rejects_text_at_document_level_before_root() {
        let err = parse_all("hello<r/>", ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("document level"),
            "expected 'document level' in error, got: {err}");
    }

    #[test]
    fn rejects_text_after_root() {
        let err = parse_all("<r/>trailing text", ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("document level"),
            "expected 'document level' in error, got: {err}");
    }

    #[test]
    fn allows_comments_and_pis_at_document_level() {
        // Misc (comment / PI / whitespace) is legal at the document
        // level both before and after the root element.
        assert!(parse_all("<!-- before --><r/><!-- after -->", ParseOptions::default()).is_ok());
        assert!(parse_all("<?pi ?><r/><?pi ?>", ParseOptions::default()).is_ok());
        assert!(parse_all("  <r/>\n", ParseOptions::default()).is_ok());
    }

    #[test]
    fn rejects_empty_xml_declaration() {
        // XML 1.0 § 2.8 [XMLDecl]: VersionInfo is required.
        let err = parse_all("<?xml?><r/>", ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("version") || err.to_string().contains("XMLDecl"),
            "expected 'version' / 'XMLDecl' in error, got: {err}");
    }

    #[test]
    fn rejects_xml_decl_without_version() {
        let err = parse_all(r#"<?xml encoding="UTF-8"?><r/>"#, ParseOptions::default()).unwrap_err();
        assert!(err.to_string().contains("version"),
            "expected 'version' in error, got: {err}");
    }

    #[test]
    fn accepts_full_xml_decl() {
        // Sanity: the canonical full declaration still parses.
        assert!(parse_all(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><r/>"#,
            ParseOptions::default()).is_ok());
    }

    // ── opt-in: skip_inter_element_whitespace ────────────────────────
    //
    // Mirrors quick-xml's `trim_text(true)`.  Default is off — we
    // emit every whitespace text event correctly.  When enabled,
    // pure-whitespace runs between tags are dropped entirely.

    #[test]
    fn skip_inter_element_whitespace_drops_indent_runs() {
        let src = "<root>\n  <a/>\n  <b/>\n</root>";
        let opts = ParseOptions {
            skip_inter_element_whitespace: true,
            ..ParseOptions::default()
        };
        let (total, ws) = count_text_events(src, opts);
        assert_eq!(ws, 0, "opt-in should suppress whitespace-only text");
        assert_eq!(total, 0, "no other text in this document");
    }

    #[test]
    fn skip_inter_element_whitespace_keeps_non_blank_text_verbatim() {
        // `remove_blank_text` drops only *entirely* whitespace runs that
        // sit between elements; a run that is the leading whitespace of a
        // non-blank text node is kept verbatim (libxml2 never strips
        // prose).  So both "foo  " and " baz" survive intact — only the
        // run's not being all-whitespace-between-elements matters.
        let src = "<p>foo  <b>bar</b> baz</p>";
        let opts = ParseOptions {
            skip_inter_element_whitespace: true,
            ..ParseOptions::default()
        };
        let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(src.as_bytes()) }
            .with_options(opts);
        let mut texts = Vec::new();
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                BytesEvent::Text(t) => texts.push(t.as_bytes().to_vec()),
                _ => {}
            }
        }
        assert_eq!(
            texts.iter().map(|t| String::from_utf8_lossy(t).into_owned()).collect::<Vec<_>>(),
            vec!["foo  ".to_string(), "bar".to_string(), " baz".to_string()],
        );
    }

    /// Lazy contract: discarding `BytesEvent::StartElement(_)` should
    /// not extract the name or construct a Scanner.  We can't measure
    /// allocations from a unit test cheaply, but we *can* verify the
    /// event simply exists with no panics, no out-of-band work, and
    /// `next()` advances correctly afterward.
    #[test]
    fn lazy_discard_event_advances_correctly() {
        // Wrapped in a root element — XML 1.0 § 2.1 forbids multiple
        // top-level elements, and the reader now enforces that.  The
        // test's intent (verify lazy discard advances correctly) is
        // unchanged.
        let mut r = XmlBytesReader::from_str("<root><a/><b/><c/></root>");
        // Consume the root opener.
        match r.next().unwrap() {
            BytesEvent::StartElement(_) => {}
            other => panic!("expected root StartElement, got {other:?}"),
        }
        // Discard each StartElement entirely (don't even bind tag).
        for expected in [b"a", b"b", b"c"] {
            match r.next().unwrap() {
                BytesEvent::StartElement(_) => {} // discard
                other => panic!("expected StartElement, got {other:?}"),
            }
            match r.next().unwrap() {
                BytesEvent::EndElement(tag) => assert_eq!(tag.name(), &expected[..]),
                other => panic!("expected EndElement, got {other:?}"),
            }
        }
        // Consume the wrapping `</root>` then EOF.
        match r.next().unwrap() {
            BytesEvent::EndElement(tag) => assert_eq!(tag.name(), &b"root"[..]),
            other => panic!("expected root EndElement, got {other:?}"),
        }
        assert!(matches!(r.next().unwrap(), BytesEvent::Eof));
    }

    // ── Debug impls on all event types ────────────────────────────

    #[test]
    fn debug_impls_for_event_payloads() {
        let mut r = XmlBytesReader::from_str(
            r#"<root attr="v"><![CDATA[cd]]>text<!-- c --><?pi data?></root>"#,
        );

        // StartElement
        match r.next().unwrap() {
            BytesEvent::StartElement(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesStartTag"), "got {s}");
                assert!(s.contains("root"), "got {s}");
            }
            _ => panic!(),
        }
        // CData
        match r.next().unwrap() {
            BytesEvent::CData(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesCData"), "got {s}");
                assert!(s.contains("cd"), "got {s}");
            }
            _ => panic!(),
        }
        // Text
        match r.next().unwrap() {
            BytesEvent::Text(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesText"), "got {s}");
                assert!(s.contains("text"), "got {s}");
            }
            _ => panic!(),
        }
        // Comment
        match r.next().unwrap() {
            BytesEvent::Comment(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesComment"), "got {s}");
                assert!(s.contains(" c "), "got {s}");
            }
            _ => panic!(),
        }
        // Pi
        match r.next().unwrap() {
            BytesEvent::Pi(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesPi"), "got {s}");
                assert!(s.contains("pi"),   "got {s}");
                assert!(s.contains("data"), "got {s}");
            }
            _ => panic!(),
        }
        // EndElement
        match r.next().unwrap() {
            BytesEvent::EndElement(t) => {
                let s = format!("{t:?}");
                assert!(s.contains("BytesEndTag"), "got {s}");
                assert!(s.contains("root"), "got {s}");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn debug_impl_for_entity_ref() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY foo "bar">]>
<r>&foo;</r>"#;
        let opts = crate::options::ParseOptions {
            resolve_entities: false,
            ..crate::options::ParseOptions::default()
        };
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        loop {
            match r.next().unwrap() {
                BytesEvent::EntityRef(e) => {
                    // BytesEntityRef::name accessor.
                    assert_eq!(e.name(), b"foo");
                    let s = format!("{e:?}");
                    assert!(s.contains("BytesEntityRef"), "got {s}");
                    assert!(s.contains("foo"), "got {s}");
                    break;
                }
                BytesEvent::Eof => panic!("EntityRef not seen"),
                _ => continue,
            }
        }
    }

    #[test]
    fn start_tag_name_inside_entity_replacement_stream_is_captured() {
        // An entity whose replacement text contains an element start
        // tag.  When the parser expands `&inner;` and emits the
        // StartElement event, `name()` used to return `&[]` because
        // the source-offsets indexed into the entity-stream buffer,
        // not the document source.  Now the name is captured into
        // an owned slot.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY inner "<span/>">]>
<r>&inner;</r>"#;
        let mut r = XmlBytesReader::from_str(src);
        let mut saw_span = false;
        loop {
            match r.next().unwrap() {
                BytesEvent::StartElement(tag) => {
                    if tag.name() == b"span" {
                        saw_span = true;
                        // name_cow returns Cow::Owned for entity-stream
                        // names (no source slice to borrow from).
                        assert!(matches!(tag.name_cow(), Cow::Owned(_)),
                            "entity-stream start tag must carry an owned name");
                    }
                }
                BytesEvent::Eof => break,
                _ => continue,
            }
        }
        assert!(saw_span, "<span/> from inside &inner; was never emitted");
    }

    #[test]
    fn start_tag_name_on_original_source_stays_borrowed() {
        // Sanity check: the common path still returns Cow::Borrowed
        // (no allocation when the name lives in the source buffer).
        let mut r = XmlBytesReader::from_str("<el/>");
        match r.next().unwrap() {
            BytesEvent::StartElement(tag) => {
                assert_eq!(tag.name(), b"el");
                assert!(matches!(tag.name_cow(), Cow::Borrowed(_)),
                    "source-borrowed start tag must avoid the heap copy");
            }
            _ => panic!(),
        }
    }

    // ── BytesAttr::name accessor ──────────────────────────────────

    #[test]
    fn bytes_attr_name_accessor() {
        let src = r#"<el id="1" class="x"/>"#;
        let mut r = XmlBytesReader::from_str(src);
        match r.next().unwrap() {
            BytesEvent::StartElement(tag) => {
                let attrs: Vec<_> = tag.attrs().map(|a| a.unwrap()).collect();
                // .name() method (vs .name field).
                assert_eq!(attrs[0].name(),  b"id");
                assert_eq!(attrs[1].name(),  b"class");
            }
            _ => panic!(),
        }
    }

    // ── unescape_bytes function ───────────────────────────────────

    #[test]
    fn unescape_bytes_no_amp_returns_borrowed() {
        let out = unescape_bytes(b"no entities here");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(&*out, b"no entities here");
    }

    #[test]
    fn unescape_bytes_predefined() {
        assert_eq!(&*unescape_bytes(b"&amp;"),  b"&");
        assert_eq!(&*unescape_bytes(b"&lt;"),   b"<");
        assert_eq!(&*unescape_bytes(b"&gt;"),   b">");
        assert_eq!(&*unescape_bytes(b"&quot;"), b"\"");
        assert_eq!(&*unescape_bytes(b"&apos;"), b"'");
    }

    #[test]
    fn unescape_bytes_numeric_decimal_and_hex() {
        assert_eq!(&*unescape_bytes(b"&#65;"),   b"A");
        assert_eq!(&*unescape_bytes(b"&#x41;"),  b"A");
        assert_eq!(&*unescape_bytes(b"&#X41;"),  b"A");
        // Multi-byte UTF-8 codepoint.
        let euro = unescape_bytes(b"&#8364;");
        assert_eq!(&*euro, "€".as_bytes());
    }

    #[test]
    fn probe_p68_standalone_yes_dom_path_rejects() {
        // ibm68n06 shape: standalone="yes", entity declared in external
        // subset, referenced in body.  XML 1.0 § 4.1 WFC: Entity Declared.
        use crate::parser::parse_bytes;
        use crate::options::ParseOptions;
        use std::sync::Arc;
        use std::collections::HashMap;
        use crate::entity_resolver::{EntityResolver, ResolveError};
        #[derive(Debug)] struct Stub { served: HashMap<String, Vec<u8>> }
        impl EntityResolver for Stub {
            fn resolve(&self, _: Option<&str>, sid: &str, _: Option<&str>)
                -> std::result::Result<Vec<u8>, ResolveError>
            {
                self.served.get(sid).cloned().ok_or_else(|| ResolveError::Io(format!("no: {sid}")))
            }
        }
        let mut served = HashMap::new();
        served.insert(
            "file:///x/ibm.dtd".to_string(),
            br#"<!ENTITY aaa "aString">"#.to_vec(),
        );
        let opts = ParseOptions {
            load_external_dtd: true,
            base_url: Some("file:///x/ibm.xml".to_string()),
            external_resolver: Some(Arc::new(Stub { served }) as Arc<dyn EntityResolver>),
            ..ParseOptions::default()
        };
        let src = br#"<?xml version="1.0" standalone="yes"?>
<!DOCTYPE root SYSTEM "ibm.dtd" [<!ELEMENT root (#PCDATA)><!ATTLIST root att CDATA #IMPLIED>]>
<root att="&aaa;">x</root>"#;
        let res = parse_bytes(src, &opts);
        assert!(res.is_err(),
            "standalone=yes + external entity decl must be rejected; got {:?}", res);
    }

    #[test]
    fn probe_p66_sax_path_must_also_reject() {
        // Same fixture, but driven through XmlBytesReader::next() —
        // the SAX/event path the bench harness uses.  Today this
        // silently accepts because we never iterate the attribute
        // values, so scan_att_value_cow doesn't run.
        //
        // The contract of "Eof reached without error implies the
        // document is well-formed" requires us to validate
        // attributes eagerly here.
        let src = br#"<!DOCTYPE r [<!ELEMENT r EMPTY><!ATTLIST r att CDATA #IMPLIED>]><r att="&#x0000;"/>"#;
        let opts = crate::options::ParseOptions {
            load_external_dtd: false,
            ..crate::options::ParseOptions::default()
        };
        let mut r = XmlBytesReader::from_bytes(src)
            .expect("UTF-8 ok")
            .with_options(opts);
        let mut hit_error = false;
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => continue,
                Err(_) => { hit_error = true; break; }
            }
        }
        assert!(hit_error, "SAX path must surface invalid Char in attr value");
    }

    #[test]
    fn probe_p66_attr_charref_to_u0000() {
        // ibm66n12 shape: char ref to U+0000 in an attribute value.
        // XML 1.0 § 4.1 requires this to be NOT-WF.
        use crate::parser::parse_bytes;
        use crate::options::ParseOptions;
        let src = br#"<!DOCTYPE r [<!ELEMENT r EMPTY><!ATTLIST r att CDATA #IMPLIED>]><r att="&#x0000;"/>"#;
        let res = parse_bytes(src, &ParseOptions::default());
        assert!(res.is_err(), "U+0000 char ref in attr value must be rejected; got Ok");
    }

    #[test]
    fn probe_p66_attr_charref_to_ufffe() {
        use crate::parser::parse_bytes;
        use crate::options::ParseOptions;
        let src = br#"<!DOCTYPE r [<!ELEMENT r EMPTY><!ATTLIST r att CDATA #IMPLIED>]><r att="&#xfffe;"/>"#;
        let res = parse_bytes(src, &ParseOptions::default());
        assert!(res.is_err(), "U+FFFE char ref in attr value must be rejected; got Ok");
    }

    #[test]
    fn unescape_bytes_invalid_charref_left_literal() {
        // Unparseable codepoint → keep literal.
        assert_eq!(&*unescape_bytes(b"&#abc;"), b"&#abc;");
        // Out-of-range codepoint → keep literal.
        assert_eq!(&*unescape_bytes(b"&#99999999;"), b"&#99999999;");
    }

    #[test]
    fn unescape_bytes_unknown_entity_left_literal() {
        assert_eq!(&*unescape_bytes(b"&bogus;"), b"&bogus;");
    }

    #[test]
    fn unescape_bytes_ampersand_without_semicolon_left_literal() {
        assert_eq!(&*unescape_bytes(b"& not an entity"), b"& not an entity");
        // Semicolon further than 16 bytes away → not treated as entity.
        let s = b"&very_long_pseudo_entity_name_that_is_too_far_away;";
        assert_eq!(&*unescape_bytes(s), s);
    }

    #[test]
    fn unescape_bytes_mixed_content() {
        let out = unescape_bytes(b"a &amp; b &lt; c &gt; d");
        assert_eq!(&*out, b"a & b < c > d");
    }

    // ── resolve_uri ───────────────────────────────────────────────

    #[test]
    fn resolve_uri_absolute_file_url_passes_through() {
        // Already-absolute `file://` URL stays untouched, regardless of base.
        assert_eq!(
            resolve_uri("file:///abs/path/foo.xml", Some("file:///other/base.xml")),
            "file:///abs/path/foo.xml"
        );
    }

    #[test]
    fn resolve_uri_absolute_path_passes_through() {
        // Leading `/` is treated as absolute even without a `file://` scheme.
        assert_eq!(
            resolve_uri("/abs/foo.xml", Some("file:///base/dir.xml")),
            "/abs/foo.xml"
        );
    }

    #[test]
    fn resolve_uri_http_url_passes_through() {
        // Any URI scheme is considered absolute — no joining.
        assert_eq!(
            resolve_uri("http://example.com/x.dtd", Some("file:///doc.xml")),
            "http://example.com/x.dtd"
        );
    }

    #[test]
    fn resolve_uri_relative_joined_against_parent_of_base() {
        // Relative path is joined against the *parent directory* of base
        // (matching how XML 1.0 § 4.2.2 defines base URI semantics).
        assert_eq!(
            resolve_uri("rel/path/foo.ent", Some("file:///docs/E18.xml")),
            "file:///docs/rel/path/foo.ent"
        );
    }

    #[test]
    fn resolve_uri_dotdot_components_preserved_for_resolver() {
        // `..` segments are *not* resolved here — the resolver's
        // canonicalize() step handles them, which keeps the
        // security check (starts_with(root)) accurate.
        let r = resolve_uri("../sib/foo.ent", Some("file:///docs/sub/E18.xml"));
        assert_eq!(r, "file:///docs/sub/../sib/foo.ent");
    }

    #[test]
    fn resolve_uri_no_base_passes_through() {
        // No base → return verbatim; the resolver will decide what
        // to do with a relative path (often: refuse).
        assert_eq!(resolve_uri("rel/foo.ent", None), "rel/foo.ent");
    }

    #[test]
    fn resolve_uri_non_file_base_preserved_without_scheme() {
        // No `file://` scheme on base means the result also has no
        // scheme — joining is purely path-level.
        assert_eq!(
            resolve_uri("foo.ent", Some("/abs/docs/E18.xml")),
            "/abs/docs/foo.ent"
        );
    }

    // ── End-to-end: parser pre-resolves URLs per E18 rule ────────
    //
    // Verifies the parser hands resolver an *absolute* system_id
    // computed per XML 1.0 errata E18: PE decls use containing
    // entity's base URI; general-entity decls use the document URL.

    #[test]
    fn parser_e18_pe_decl_uses_containing_entity_base_uri() {
        use crate::entity_resolver::{EntityResolver, ResolveError};
        use std::sync::{Arc, Mutex};

        // Recording resolver: captures every URL the parser asks
        // for, and serves canned bytes from an in-memory map keyed
        // by the absolute URL.
        #[derive(Debug, Default)]
        struct Recorder {
            asked: Mutex<Vec<String>>,
            served: std::collections::HashMap<String, Vec<u8>>,
        }
        impl EntityResolver for Recorder {
            fn resolve(
                &self,
                _public_id: Option<&str>,
                system_id: &str,
                _base_uri: Option<&str>,
            ) -> std::result::Result<Vec<u8>, ResolveError> {
                self.asked.lock().unwrap().push(system_id.to_string());
                self.served.get(system_id).cloned().ok_or_else(|| {
                    ResolveError::Io(format!("no fixture for {system_id}"))
                })
            }
        }

        let mut served = std::collections::HashMap::new();
        // PE `outer` declares `<!ENTITY % inner SYSTEM "sib.ent">`.
        // The relative `sib.ent` must resolve against `outer`'s URL
        // → `file:///docs/sub/sib.ent`, NOT against the document
        // URL (which would give `file:///docs/sib.ent`).
        served.insert(
            "file:///docs/sub/outer.ent".to_string(),
            b"<!ENTITY % inner SYSTEM 'sib.ent'>".to_vec(),
        );
        served.insert(
            "file:///docs/sub/sib.ent".to_string(),
            b"<!-- inner -->".to_vec(),
        );

        let recorder = Arc::new(Recorder { asked: Mutex::new(Vec::new()), served });
        let opts = ParseOptions {
            base_url: Some("file:///docs/E18.xml".to_string()),
            external_resolver: Some(recorder.clone() as Arc<dyn EntityResolver>),
            // External parameter-entity loading is gated behind this opt-in
            // (XXE/SSRF hardening); these tests exercise that loading to
            // verify E18 base-URI resolution, so they enable it explicitly.
            load_external_dtd: true,
            ..ParseOptions::default()
        };
        // Declare the outer PE in the doc; %outer; then expands and
        // its `<!ENTITY % inner SYSTEM "sib.ent">` is parsed.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
<!ENTITY % outer SYSTEM "sub/outer.ent">
%outer;
%inner;
]>
<r/>"#;
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                _ => {}
            }
        }
        let asked = recorder.asked.lock().unwrap().clone();
        // Two URLs requested in order:
        //   1. `sub/outer.ent` resolved against the doc URL
        //   2. `sib.ent` resolved against the *outer* PE's URL
        assert_eq!(asked, vec![
            "file:///docs/sub/outer.ent".to_string(),
            "file:///docs/sub/sib.ent".to_string(),
        ]);
    }

    #[test]
    fn parser_e18_general_entity_decl_uses_document_base_uri() {
        // The E18 rule's punchline: a general-entity decl inside a
        // deeply-nested external PE resolves its SYSTEM URL against
        // the *document* URL, not the containing PE.  This is the
        // unusual rule rmt-e2e-18 was designed to catch.
        use crate::entity_resolver::{EntityResolver, ResolveError};
        use std::sync::{Arc, Mutex};

        #[derive(Debug, Default)]
        struct Recorder {
            asked: Mutex<Vec<String>>,
            served: std::collections::HashMap<String, Vec<u8>>,
        }
        impl EntityResolver for Recorder {
            fn resolve(
                &self,
                _public_id: Option<&str>,
                system_id: &str,
                _base_uri: Option<&str>,
            ) -> std::result::Result<Vec<u8>, ResolveError> {
                self.asked.lock().unwrap().push(system_id.to_string());
                self.served.get(system_id).cloned().ok_or_else(|| {
                    ResolveError::Io(format!("no fixture for {system_id}"))
                })
            }
        }

        let mut served = std::collections::HashMap::new();
        // Doc: file:///docs/E18.xml
        // %outer; bytes live at file:///docs/sub/outer.ent.
        // Inside outer, a general-entity decl `ent SYSTEM 'E18-ent'`.
        // Per E18: `E18-ent` resolves against the *document* URL →
        // `file:///docs/E18-ent`, NOT the containing PE's URL
        // (which would give `file:///docs/sub/E18-ent`).
        served.insert(
            "file:///docs/sub/outer.ent".to_string(),
            b"<!ENTITY ent SYSTEM 'E18-ent'>".to_vec(),
        );
        served.insert(
            "file:///docs/E18-ent".to_string(),
            b"main-dir-content".to_vec(),
        );

        let recorder = Arc::new(Recorder { asked: Mutex::new(Vec::new()), served });
        let opts = ParseOptions {
            base_url: Some("file:///docs/E18.xml".to_string()),
            external_resolver: Some(recorder.clone() as Arc<dyn EntityResolver>),
            // External parameter-entity loading is gated behind this opt-in
            // (XXE/SSRF hardening); these tests exercise that loading to
            // verify E18 base-URI resolution, so they enable it explicitly.
            load_external_dtd: true,
            ..ParseOptions::default()
        };
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
<!ENTITY % outer SYSTEM "sub/outer.ent">
%outer;
]>
<r>&ent;</r>"#;
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        loop {
            match r.next().unwrap() {
                BytesEvent::Eof => break,
                _ => {}
            }
        }
        let asked = recorder.asked.lock().unwrap().clone();
        // Crucially: `E18-ent` was resolved against `/docs/`, not
        // `/docs/sub/`.  This is the E18 rule.
        assert_eq!(asked, vec![
            "file:///docs/sub/outer.ent".to_string(),
            "file:///docs/E18-ent".to_string(),
        ]);
    }

    // ── WFC: Entity Declared carve-out ───────────────────────────
    //
    // XML 1.0 § 4.1: "for an entity reference that does NOT occur
    // within the external subset or a parameter entity, the Name
    // given in the entity reference MUST match that of an entity
    // declared."  The contrapositive — refs inside external content
    // are exempt from the WF rule.  These tests cover the carve-out
    // for general entities, parameter entities, and ATTLIST default
    // values (each goes through a different code path).

    #[test]
    fn carve_out_undeclared_pe_inside_external_pe_accepted() {
        use crate::entity_resolver::{EntityResolver, ResolveError};
        use std::sync::{Arc, Mutex};
        use std::collections::HashMap;

        #[derive(Debug)]
        struct Stub { served: HashMap<String, Vec<u8>> }
        impl EntityResolver for Stub {
            fn resolve(&self, _: Option<&str>, sid: &str, _: Option<&str>)
                -> std::result::Result<Vec<u8>, ResolveError>
            {
                self.served.get(sid).cloned().ok_or_else(|| {
                    ResolveError::Io(format!("no fixture: {sid}"))
                })
            }
        }
        let mut served = HashMap::new();
        // Outer PE's content references an undeclared `%pe_missing;`.
        // Per the WFC carve-out (ref is inside external PE), parse
        // should accept; expansion is skipped with a warning logged.
        served.insert(
            "file:///docs/outer.ent".to_string(),
            b"<!ELEMENT root EMPTY> %pe_missing;".to_vec(),
        );
        let opts = ParseOptions {
            base_url: Some("file:///docs/doc.xml".to_string()),
            external_resolver: Some(Arc::new(Stub { served }) as Arc<dyn EntityResolver>),
            ..ParseOptions::default()
        };
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE root [
<!ENTITY % outer SYSTEM "outer.ent">
%outer;
]>
<root/>"#;
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        let _ = Mutex::new(()); // silence unused-import in some builds
        loop {
            match r.next().expect("WFC carve-out should let parse succeed") {
                BytesEvent::Eof => break,
                _ => {}
            }
        }
    }

    #[test]
    fn carve_out_undeclared_ge_inside_external_pe_accepted() {
        // ATTLIST default value contains `&ge_missing;` (general
        // entity).  The ATTLIST is parsed inside the external PE's
        // content, so the carve-out applies — non-validating parse
        // accepts.  This is the ibm-invalid-P68-i03/i04 shape.
        use crate::entity_resolver::{EntityResolver, ResolveError};
        use std::sync::Arc;
        use std::collections::HashMap;

        #[derive(Debug)]
        struct Stub { served: HashMap<String, Vec<u8>> }
        impl EntityResolver for Stub {
            fn resolve(&self, _: Option<&str>, sid: &str, _: Option<&str>)
                -> std::result::Result<Vec<u8>, ResolveError>
            {
                self.served.get(sid).cloned().ok_or_else(|| {
                    ResolveError::Io(format!("no fixture: {sid}"))
                })
            }
        }
        let mut served = HashMap::new();
        served.insert(
            "file:///docs/outer.ent".to_string(),
            br#"<!ATTLIST root attr CDATA "&ge_missing;">"#.to_vec(),
        );
        let opts = ParseOptions {
            base_url: Some("file:///docs/doc.xml".to_string()),
            external_resolver: Some(Arc::new(Stub { served }) as Arc<dyn EntityResolver>),
            ..ParseOptions::default()
        };
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE root [
<!ELEMENT root EMPTY>
<!ENTITY % outer SYSTEM "outer.ent">
%outer;
]>
<root/>"#;
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        loop {
            match r.next().expect("WFC carve-out should let parse succeed") {
                BytesEvent::Eof => break,
                _ => {}
            }
        }
    }

    #[test]
    fn undeclared_ge_in_original_source_still_rejected() {
        // Negative control: WFC: Entity Declared still applies for
        // refs *outside* external content (regular doc body refs).
        // Otherwise the carve-out would have silently weakened the
        // rule for everyone, which would be a regression.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE root [
<!ELEMENT root (#PCDATA)>
]>
<root>&undeclared;</root>"#;
        let opts = ParseOptions::default();
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        let mut got_err = false;
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => {}
                Err(e) => {
                    assert!(
                        e.message.contains("undefined entity") &&
                        e.message.contains("WFC: Entity Declared"),
                        "expected WFC: Entity Declared error, got: {}", e.message
                    );
                    got_err = true;
                    break;
                }
            }
        }
        assert!(got_err, "expected undeclared-entity error outside external content");
    }

    #[test]
    fn undeclared_pe_in_internal_subset_still_rejected() {
        // Negative control for the PE path: a PE reference at the
        // top of the doc's internal subset is NOT inside external
        // content, so the carve-out does not apply.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE root [
%missing_pe;
<!ELEMENT root EMPTY>
]>
<root/>"#;
        let opts = ParseOptions::default();
        let mut r = XmlBytesReader::from_str(src).with_options(opts);
        let mut got_err = false;
        loop {
            match r.next() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => {}
                Err(e) => {
                    assert!(
                        e.message.contains("undefined parameter entity"),
                        "expected undefined-PE error, got: {}", e.message
                    );
                    got_err = true;
                    break;
                }
            }
        }
        assert!(got_err, "expected undeclared-PE error outside external content");
    }
}
