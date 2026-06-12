//! SAX-driven arena DOM parser.
//!
//! Produces a [`sup_xml_tree::dom::Document`] by feeding [`XmlReader`]
//! events into a [`DocumentBuilder`].  Compared to the existing
//! [`parse_str`](crate::parse_str) / [`parse_bytes`](crate::parse_bytes) DOM
//! parsers — which build a per-node-heap-allocated tree — this version
//! allocates the entire tree in a single [`bumpalo::Bump`].  Per-node alloc
//! cost drops to a pointer bump; drop is free per node.
//!
//! # Why a separate entry point
//!
//! The arena DOM has a different type ([`arena::Document`]) than the legacy
//! tree.  We expose this as a parallel API so consumers can migrate one at a
//! time.  Once everything's ported, the legacy entry points can become thin
//! wrappers (or get deleted).
//!
//! # Implementation
//!
//! Wraps [`XmlReader`] — the SAX layer.  All XML correctness (well-formedness
//! checks, entity expansion, encoding handling, recovery mode) comes from the
//! reader.  This module is pure tree assembly: pop/push elements on a stack,
//! attach leaf nodes to the current top, copy strings into the arena.
//!
//! # Example
//!
//! ```
//! use sup_xml_core::{parse_str, ParseOptions};
//! let doc = parse_str("<r><a id='1'/></r>", &ParseOptions::default()).unwrap();
//! let root = doc.root();
//! assert_eq!(root.name(), "r");
//! let a = root.children().next().unwrap();
//! assert_eq!(a.name(), "a");
//! assert_eq!(a.attributes().next().unwrap().value(), "1");
//! ```

use crate::encoding;
use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::ns_helpers::{
    ns_err, validate_qname, validate_xmlns_decl, XML_NS_URI, XMLNS_NS_URI,
};
use crate::options::ParseOptions;
use crate::xml_bytes_reader::{BytesAttr, BytesEvent, XmlBytesReader};
use rustc_hash::FxHashSet;
use sup_xml_tree::dom::{Attribute, Document, DocumentBuilder, Namespace, Node};

/// Parse `input` into an arena-allocated [`Document`].  Uses default
/// [`ParseOptions`].
pub fn parse_str(input: &str, opts: &ParseOptions) -> Result<Document> {
    let source: Box<[u8]> = input.as_bytes().to_vec().into_boxed_slice();
    parse_owned_bytes(source, opts)
}

/// Recovery-mode sibling of [`parse_str`].  Returns the (best-effort)
/// parsed [`Document`] along with the list of non-fatal errors that
/// recovery mode forgave.  When [`ParseOptions::recovery_mode`] is `false`
/// the second element is always empty and the first is the same `Result`
/// as [`parse_str`] would have produced.
pub fn parse_str_with_recovered(
    input: &str,
    opts: &ParseOptions,
) -> (Result<Document>, Vec<XmlError>) {
    let source: Box<[u8]> = input.as_bytes().to_vec().into_boxed_slice();
    parse_owned_bytes_with_recovered(source, opts)
}

/// Byte-slice sibling of [`parse_str`].
///
/// With [`ParseOptions::auto_transcode`] true (the default), non-UTF-8
/// input is detected and converted to UTF-8 before parsing — see that
/// field's docs for the supported set.  Set `auto_transcode = false` to
/// require UTF-8 input and reject anything else with
/// [`ErrorDomain::Encoding`].
pub fn parse_bytes(input: &[u8], opts: &ParseOptions) -> Result<Document> {
    let source = transcode_and_validate(input, opts)?;
    parse_owned_bytes(source, opts)
}

/// Variant of [`parse_bytes`] that also returns the DTD captured from
/// the internal subset.  Returns an empty [`Dtd`](crate::dtd::Dtd)
/// when the document had no `<!DOCTYPE … [ … ]>` or no
/// `<!ELEMENT>` / `<!ATTLIST>` declarations.
pub fn parse_bytes_with_dtd(
    input: &[u8],
    opts:  &ParseOptions,
) -> Result<(Document, crate::dtd::Dtd)> {
    let source = transcode_and_validate(input, opts)?;
    let b = DocumentBuilder::new();
    b.set_source(source);
    let src_bytes: &[u8] = b.source().expect("source just set");
    // SAFETY: transcode_and_validate guarantees UTF-8.
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(src_bytes) }
        .with_options(opts.clone());
    drive(&b, &mut reader, opts)?;
    let dtd = reader.take_dtd();
    Ok((b.build(), dtd))
}

/// Parse a standalone external DTD subset into a [`Dtd`](crate::dtd::Dtd).
///
/// `input` is the raw bytes of a DTD — the markup declarations a `.dtd`
/// file (or an `etree.DTD(...)` source) contains, with no surrounding
/// document or `<!DOCTYPE>` wrapper.  Conditional sections and
/// top-level parameter-entity references are permitted, per the XML 1.0
/// § 2.8 external-subset grammar (the internal subset forbids both).
///
/// Returns the captured declarations.  A fatal malformation returns
/// `Err`; recoverable issues are tolerated (a non-validating DTD parse
/// is best-effort, matching libxml2's `xmlParseDTD`).
pub fn parse_external_subset(
    input: &[u8],
    opts:  &ParseOptions,
) -> Result<crate::dtd::Dtd> {
    let bytes = transcode_and_validate(input, opts)?;
    // SAFETY: transcode_and_validate guarantees the buffer is UTF-8.
    let text = unsafe { String::from_utf8_unchecked(bytes.into_vec()) };
    // The subset is read from a pushed entity-stream frame; the reader's
    // own source is the empty (static) slice, which is a valid origin.
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(&[]) }
        .with_options(opts.clone());
    reader.parse_standalone_external_subset(text)?;
    Ok(reader.take_dtd())
}

/// Variant of [`parse_bytes_with_dtd`] that interns names through
/// a caller-supplied refcounted dict instead of an internal one.
/// Use when the resulting document needs to share name canonicals
/// with another consumer that owns the same dict (e.g. a C-ABI
/// parser context whose `ctxt->dict` already points to a thread-
/// shared interner).
///
/// The dict's refcount is bumped for the new document's reference;
/// the caller's own reference remains independent.
///
/// # Safety
///
/// `dict` must be a valid pointer returned by
/// [`crate::dict::Dict::new_refcounted`] (or otherwise refcount-
/// managed), with at least one outstanding reference.
#[cfg(feature = "c-abi")]
pub unsafe fn parse_bytes_with_dtd_and_dict(
    input: &[u8],
    opts:  &ParseOptions,
    dict:  *mut sup_xml_tree::dict::Dict,
) -> Result<(Document, crate::dtd::Dtd)> {
    let source = transcode_and_validate(input, opts)?;
    // SAFETY: caller asserts `dict` is live with positive refcount.
    let b = unsafe { DocumentBuilder::new_with_dict(dict) };
    b.set_source(source);
    let src_bytes: &[u8] = b.source().expect("source just set");
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(src_bytes) }
        .with_options(opts.clone());
    drive(&b, &mut reader, opts)?;
    let dtd = reader.take_dtd();
    Ok((b.build(), dtd))
}

/// Variant of [`parse_bytes_with_dtd_and_dict`] that also adopts an
/// externally-supplied [`Bump`] arena (shared via `Arc`).  Used by
/// C-ABI consumers that route every per-thread parse through a
/// single shared arena — node memory then outlives any individual
/// document and cross-doc graft operations are safe by construction.
///
/// # Safety
///
/// `dict` must be a valid refcount-managed [`crate::dict::Dict`].
/// `arena` may be cloned from any source; it becomes one of the
/// document's references.
#[cfg(feature = "c-abi")]
pub unsafe fn parse_bytes_with_dtd_dict_arena(
    input: &[u8],
    opts:  &ParseOptions,
    dict:  *mut sup_xml_tree::dict::Dict,
    arena: std::sync::Arc<bumpalo::Bump>,
) -> Result<(Document, crate::dtd::Dtd)> {
    let source = transcode_and_validate(input, opts)?;
    // SAFETY: caller asserts `dict` is live with positive refcount.
    let b = unsafe { DocumentBuilder::new_with_dict_and_arena(dict, arena) };
    b.set_source(source);
    let src_bytes: &[u8] = b.source().expect("source just set");
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(src_bytes) }
        .with_options(opts.clone());
    drive(&b, &mut reader, opts)?;
    let dtd = reader.take_dtd();
    Ok((b.build(), dtd))
}

/// Recovery-mode sibling of [`parse_bytes`].  See
/// [`parse_str_with_recovered`] for semantics.
pub fn parse_bytes_with_recovered(
    input: &[u8],
    opts: &ParseOptions,
) -> (Result<Document>, Vec<XmlError>) {
    let source = match transcode_and_validate(input, opts) {
        Ok(s) => s,
        Err(e) => return (Err(e), Vec::new()),
    };
    parse_owned_bytes_with_recovered(source, opts)
}

/// Byte-slice version that skips the upfront UTF-8 validation.  Mirrors
/// [`parse_bytes_unchecked`](crate::parse_bytes_unchecked).
///
/// # Safety
///
/// `input` must be valid UTF-8.
pub unsafe fn parse_bytes_unchecked(input: &[u8], opts: &ParseOptions) -> Result<Document> {
    // SAFETY: caller asserts UTF-8 (per the function contract).
    let source: Box<[u8]> = input.to_vec().into_boxed_slice();
    parse_owned_bytes(source, opts)
}

/// Destructive-parse fast path.  Takes ownership of `buf`, mutates it
/// in place during parsing, and returns a [`Document`] whose strings
/// point directly into the (now-mutated) buffer.  The Document keeps
/// the buffer alive for its lifetime.
///
/// **The speedup vs [`parse_bytes`] is workload-dependent and depends
/// on what flags you pass in `opts`.**  On entity-heavy documents the
/// structural in-place mechanism (in-place entity decode, zero string
/// copy) wins ~20-30% even with full XML 1.0 validation enabled.  On
/// documents that contain few or no entities (most "data" XML —
/// swiss_prot, OSM, sitemaps, RSS) the structural win is small —
/// often within run-to-run noise — because the validation cost
/// dominates and both paths pay it equally.  In that regime, the
/// bigger lever is the four `skip_*` validation flags: passing all
/// four `true` reaches roughly half of pugixml's throughput.  See
/// "When to use this" below for which combination matches your needs.
///
/// # Why it's faster
///
/// Two structural advantages over [`parse_bytes`], independent of any
/// validation flags:
///
/// 1. **No string copy into the arena.**  Element names, attribute
///    values, and text content slices point directly at bytes inside
///    `buf`.  [`parse_bytes`] copies them into a fresh arena (or, when
///    borrowing succeeds, holds the source separately).
/// 2. **In-place entity decode.**  Builtin entities (`&amp;`, `&lt;`,
///    `&gt;`, `&apos;`, `&quot;`), numeric character references, and
///    newline normalization are decoded by mutating the source bytes
///    in place — no scratch buffer per text chunk.  User-defined
///    entities with replacement text smaller than the `&name;`
///    reference also fit in place; larger ones are rejected (see
///    Errors below).
///
/// **The skip-all validation flags are NOT applied automatically.**
/// If you want the maximum speed shown in the benchmarks (about ~30%
/// on top of the structural wins above), build `ParseOptions` with
/// `skip_xml_char_validation`, `skip_name_validation`,
/// `skip_attr_validation`, and `skip_end_tag_check` set to `true`.
/// Otherwise the parser still performs full XML 1.0 validation while
/// it parses destructively.
///
/// ```
/// use sup_xml_core::{parse_bytes_in_place, ParseOptions};
///
/// let buf: Vec<u8> = b"<root><child id=\"1\"/></root>".to_vec();
///
/// // Full XML 1.0 validation + destructive parse:
/// let _doc = parse_bytes_in_place(buf.clone(), &ParseOptions::default())?;
///
/// // Trust-the-input maximum-speed:
/// let fast_opts = ParseOptions {
///     skip_xml_char_validation: true,
///     skip_name_validation:     true,
///     skip_attr_validation:     true,
///     skip_end_tag_check:       true,
///     ..ParseOptions::default()
/// };
/// let _doc = parse_bytes_in_place(buf, &fast_opts)?;
/// # Ok::<(), sup_xml_core::XmlError>(())
/// ```
///
/// # When to use this vs [`parse_bytes`]
///
/// Pick **`parse_bytes_in_place`** when:
/// - You own the input buffer and don't need to preserve its original
///   bytes (round-trip-byte-identical serialization isn't a goal).
/// - Your inputs use only the 5 XML 1.0 builtin entities, OR any
///   user-defined `<!ENTITY>` declarations have replacement text whose
///   byte length is ≤ the corresponding `&name;` reference.
/// - You do NOT need [`ParseOptions::recovery_mode`].
///
/// Pick **[`parse_bytes`]** when:
/// - You need lossless round-trip (preserve the input bytes verbatim).
/// - You need [`ParseOptions::recovery_mode`].
/// - You don't own the buffer or can't have it consumed.
///
/// # Errors
///
/// Returns `Err` immediately (before any mutation) for:
/// - `opts.recovery_mode == true` — recovery is incompatible with
///   destructive parsing (we can't unmutate after the fact).
///
/// Returns `Err` during parsing for:
/// - User `<!ENTITY>` whose expansion exceeds its reference length —
///   the bytes don't fit in place.
/// - Cyclic entity references — well-formedness error, same as
///   [`parse_bytes`].
/// - Any other XML 1.0 well-formedness violation.
///
/// On any error, `buf` is consumed and dropped (it has been partially
/// mutated by the time most errors fire; handing it back would be
/// misleading).
///
/// # Buffer ownership
///
/// `buf` is consumed unconditionally — successful parse returns a
/// [`Document`] that owns the buffer; failed parse drops it.  If you
/// might need to fall back to [`parse_bytes`], use that entry point
/// from the start; speculative pre-cloning defeats the performance
/// benefit this function exists for.
pub fn parse_bytes_in_place(buf: Vec<u8>, opts: &ParseOptions) -> Result<Document> {
    // `parse_bytes_in_place` honors every flag on `opts` as-is.  In
    // particular it does NOT silently flip the four `skip_*` validation
    // flags on — callers who want the fastest possible path build their
    // own `ParseOptions` with `skip_xml_char_validation`,
    // `skip_name_validation`, `skip_attr_validation`, and
    // `skip_end_tag_check` all set to `true`.  Callers who want
    // destructive parsing PLUS full validation (e.g. content from a
    // semi-trusted source, but they own the buffer and want the
    // in-place perf win on entity decode + zero string copy) pass
    // `ParseOptions::default()` and get that.

    // Up-front: recovery + in-place is fundamentally incompatible.
    if opts.recovery_mode {
        return Err(XmlError::new(
            ErrorDomain::Parser,
            ErrorLevel::Fatal,
            "parse_bytes_in_place does not support ParseOptions::recovery_mode \
             — destructive parsing can't unwind after a partial mutation. \
             Use parse_bytes if you need recovery."
                .to_string(),
        ));
    }

    // Encoding: if auto-transcode is on and the input isn't UTF-8, we
    // transcode upfront into a fresh Vec<u8>.  That's one copy; from
    // there, all subsequent string handling is in-place against the
    // transcoded buffer.
    let source: Box<[u8]> = if let Some(enc) = opts.forced_encoding.clone() {
        // Explicit encoding overrides auto-detection (BOM / declaration).
        encoding::transcode_to_utf8_as(&buf, enc)?
            .into_owned()
            .into_boxed_slice()
    } else if opts.auto_transcode {
        encoding::transcode_to_utf8(&buf)?
            .into_owned()
            .into_boxed_slice()
    } else {
        // Without auto_transcode the caller asserts the input is already
        // UTF-8 (or any byte sequence we should just try to parse).
        // We still validate before parsing to keep the unsafe boundary
        // tight; the arena's name/text fields are `&str`, so we must
        // know the bytes are valid UTF-8 before we point at them.
        simdutf8::compat::from_utf8(&buf).map_err(|e| {
            XmlError::new(
                ErrorDomain::Encoding,
                ErrorLevel::Fatal,
                format!("invalid UTF-8: {e}"),
            )
        })?;
        buf.into_boxed_slice()
    };

    parse_owned_bytes_inplace(source, opts)
}

/// In-place variant of `parse_owned_bytes`.  Mirrors that function but
/// constructs the reader via `XmlReader::from_bytes_in_place_unchecked`
/// so the SAX layer can mutate the source buffer during entity decoding,
/// newline normalization, etc.  See `crate::scanner::Scanner::compact_at`.
fn parse_owned_bytes_inplace(source: Box<[u8]>, opts: &ParseOptions) -> Result<Document> {
    let b = DocumentBuilder::new();
    b.set_source(source);
    // SAFETY: the builder owns the source buffer (we just gave it via
    // `set_source`); we hand mutable access to the reader for the
    // duration of parsing.  The reader is dropped before `b.build()` is
    // called, so no outstanding `&mut` exists when ownership of the
    // bytes transfers to the resulting `Document`.
    let src_bytes: &mut [u8] = {
        // Reconstruct a `&mut [u8]` from the builder's leaked pointer.
        // The builder's `Drop` (or `build()`'s ownership transfer) is
        // the sole other potential consumer; neither runs concurrently
        // with the reader.
        let (ptr, len) = (b.source_ptr_for_inplace(), b.source_len_for_inplace());
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    };
    let mut reader = unsafe { XmlBytesReader::from_bytes_in_place_unchecked(src_bytes) }
        .with_options(opts.clone());
    drive(&b, &mut reader, opts)?;
    Ok(b.build())
}

/// Transcode if needed and validate UTF-8; return an owned byte buffer
/// suitable for stashing on the builder.
fn transcode_and_validate(input: &[u8], opts: &ParseOptions) -> Result<Box<[u8]>> {
    let owned: Vec<u8> = if let Some(enc) = opts.forced_encoding.clone() {
        // Explicit encoding overrides auto-detection (BOM / declaration).
        encoding::transcode_to_utf8_as(input, enc)?.into_owned()
    } else if opts.auto_transcode {
        // transcode_to_utf8 returns Cow; only re-owns if it transcoded.
        // Either way we end up with an owned Vec we can box for stashing.
        encoding::transcode_to_utf8(input)?.into_owned()
    } else {
        input.to_vec()
    };
    simdutf8::compat::from_utf8(&owned).map_err(|e| {
        // `valid_up_to` is the byte index of the first ill-formed
        // sequence in the post-transcode buffer — identical to the
        // caller's input byte index when input was already UTF-8.
        // Compute line/col against the prefix that *is* valid UTF-8
        // (compute_line_col only inspects newline bytes, so a partial
        // UTF-8 buffer is safe to feed).
        let off = e.valid_up_to();
        let (line, col) = crate::scanner::compute_line_col(&owned, off);
        XmlError::new(ErrorDomain::Encoding, ErrorLevel::Fatal, format!("invalid UTF-8: {e}"))
            .at("<input>", line, col, off as u64)
    })?;
    Ok(owned.into_boxed_slice())
}

/// Drive the arena parser over an already-owned source buffer.  Used
/// by both `parse_str` and `parse_bytes` after they've
/// produced (and possibly transcoded) a UTF-8 byte buffer.  The source
/// is stashed on the builder so the resulting [`Document`] keeps the
/// bytes alive — letting arena strings borrow into them.
fn parse_owned_bytes(source: Box<[u8]>, opts: &ParseOptions) -> Result<Document> {
    let b = DocumentBuilder::new();
    b.set_source(source);
    // `b.source()` returns a `&[u8]` pointing at the leaked-Box bytes.  The
    // bytes live at a stable heap address until the builder (or, after
    // `build`, the resulting Document) drops them — so the slice is valid
    // for the entire parse.
    let src_bytes: &[u8] = b.source().expect("source just set");
    // SAFETY: `transcode_and_validate` (and the str → bytes conversion in
    // `parse_str`) guarantees UTF-8.
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(src_bytes) }
        .with_options(opts.clone());
    drive(&b, &mut reader, opts)?;
    let dtd = reader.take_dtd();
    let mut doc = b.build();
    if !dtd.unparsed_entities.is_empty() {
        doc.set_unparsed_entities(dtd.unparsed_entities.clone());
    }
    // XML 1.0 §3.3.2 — apply ATTLIST-declared default / #FIXED
    // attribute values to elements that didn't supply them
    // explicitly.  This was previously gated on `validating: true`,
    // but XSLT 1.0 §3.4 expects the source tree to carry defaults
    // (id() / xsl:copy / etc. rely on them) regardless of whether
    // the caller asked for validation.
    if !dtd.is_empty() {
        let _ = crate::dtd::inject::inject_defaults(&doc, &dtd);
        // Snapshot DTD-declared ID attributes (`<!ATTLIST e a ID>`)
        // onto the document so XPath's `id()` can find them.  Stored
        // as element local-name → set of attribute local-names; the
        // DTD model is element-name + attr-name keyed so we keep the
        // same shape.
        let id_map = crate::dtd::collect_id_attrs(&dtd);
        if !id_map.is_empty() {
            doc.set_id_attributes(id_map);
        }
        let idref_map = crate::dtd::collect_idref_attrs(&dtd);
        if !idref_map.is_empty() {
            doc.set_idref_attributes(idref_map);
        }
    }
    Ok(doc)
}

fn parse_owned_bytes_with_recovered(
    source: Box<[u8]>,
    opts: &ParseOptions,
) -> (Result<Document>, Vec<XmlError>) {
    let b = DocumentBuilder::new();
    b.set_source(source);
    let src_bytes: &[u8] = b.source().expect("source just set");
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(src_bytes) }
        .with_options(opts.clone());
    let drive_result = drive(&b, &mut reader, opts);
    let recovered = reader.recovered_errors().to_vec();
    let unparsed = reader.take_dtd().unparsed_entities;
    let result = drive_result.map(|()| {
        let mut d = b.build();
        if !unparsed.is_empty() { d.set_unparsed_entities(unparsed); }
        d
    });
    (result, recovered)
}

/// Parse `input` with XML Namespaces 1.0 processing enabled — resolves
/// `xmlns` declarations, fills the `namespace` field on every element and
/// prefixed attribute, validates QName syntax, and rejects undeclared
/// prefixes.  Convenience wrapper over [`parse_str`] with
/// `namespace_aware: true`.
///
/// Arena-DOM equivalent of [`parse_ns_str`](crate::parse_ns_str).
pub fn parse_ns_str(input: &str) -> Result<Document> {
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    parse_str(input, &opts)
}

/// Byte-slice sibling of [`parse_ns_str`].  Validates UTF-8 (or
/// auto-transcodes if `auto_transcode` is on) before parsing.
pub fn parse_ns_bytes(input: &[u8]) -> Result<Document> {
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    parse_bytes(input, &opts)
}

// ── driver ──────────────────────────────────────────────────────────────────

/// Type-erased pointer to a `Node` living in the builder's arena.  Stored on
/// the construction stack; re-bound to `&'a Node<'a>` only at deref time, in
/// short scopes where the builder borrow is also active.
///
/// We don't use `*const Node<'static>` directly: `Node<'doc>` is invariant
/// over `'doc`, so casts between `*const Node<'a>` and `*const Node<'static>`
/// are not free at the type-checker level.  An untyped pointer sidesteps the
/// invariance dance entirely.
type ErasedNodePtr = *const ();
type ErasedAttrPtr = *const ();

/// Convert a typed node reference into a type-erased pointer.
#[inline] fn erase(node: &Node<'_>) -> ErasedNodePtr {
    node as *const Node<'_> as *const ()
}

/// Re-type an erased pointer back to a node reference with lifetime `'a`.
///
/// # Safety
///
/// The pointer must have been obtained from [`erase`] on a node allocated
/// in an arena that is still alive at the call site (with lifetime ≥ `'a`).
#[inline] unsafe fn unerase<'a>(p: ErasedNodePtr) -> &'a Node<'a> {
    unsafe { &*(p as *const Node<'a>) }
}

/// Consume events from `reader` and build the arena Document.
///
/// Namespace handling is gated on [`ParseOptions::namespace_aware`].  When
/// false (the default for `ParseOptions::default()`), this matches the legacy
/// `parse_bytes` path — no QName validation, no xmlns scanning, no per-element
/// namespace assignment.  When true, full XML Namespaces 1.0 resolution runs
/// inline with a per-element fast-path that skips the work for elements +
/// attributes that have no `:` in their names *and* are not inside a default-
/// namespace scope.
fn drive(
    b: &DocumentBuilder,
    reader: &mut XmlBytesReader<'_>,
    opts: &ParseOptions,
) -> Result<()> {
    // Every XML document parse funnels through here; the license gate
    // verifies once per process (cached) and is a no-op thereafter.
    crate::license_gate::ensure_licensed()?;

    // Pre-reserve typical capacities so the first few pushes don't trigger
    // grow-doublings on the hot path.  XML element nesting rarely exceeds
    // ~16 (deeper than that is unusual); per-element attrs in
    // attribute-heavy formats (OSM, SVG) regularly hit 8-16.
    let mut stack:    Vec<ErasedNodePtr> = Vec::with_capacity(16);
    let mut attr_buf: Vec<BytesAttr<'_>> = Vec::with_capacity(16);
    let mut root:     Option<ErasedNodePtr> = None;

    // Incremental line-number cursor for `node.line` / `Element.sourceline`.
    // The naive approach — call `compute_line_col` per StartElement — rescans
    // `src[0..name_offset]` each time, which is O(N × file_size) ≈ O(N²)
    // across the whole parse and dominates throughput on docs with many
    // elements (10× — 100× slowdown observed).  Instead we keep a `(offset,
    // line)` cursor that only ever moves forward: each StartElement scans
    // newlines in `src[cursor.0 .. tag.name_offset]`, then bumps the cursor.
    // Total scanning work over the parse is O(file_size).
    let mut line_cursor: (usize, u32) = (0, 1);

    // The source URI (when the caller supplied one) becomes the
    // document node's base URI — `fn:base-uri()`/`fn:document-uri()`
    // resolve against it (XPath 2.0 §2.5).
    if opts.base_url.is_some() {
        b.set_base_url(opts.base_url.clone());
    }

    let ns_aware = opts.namespace_aware;

    // Namespace scope state — only populated when `ns_aware` is true.  Kept
    // as a flat Vec of (prefix, &Namespace) bindings with per-element frame
    // markers; type-erased pointers sidestep `Namespace<'_>` invariance.
    let mut ns_bindings: Vec<(Option<&str>, *const ())> = Vec::new();
    let mut ns_frames:   Vec<usize>                     = Vec::new();
    // Per-frame count of `xmlns`/`xmlns:foo` bindings added by that element,
    // so EndElement can decrement `active_user_bindings` correctly.
    let mut ns_frame_count_stack: Vec<u32>              = Vec::new();
    // Cached "is there a non-None binding in scope right now?" — set when
    // any `xmlns` or `xmlns:foo` is declared; cleared as frames pop.  This
    // is the fast-path gate: when false, no prefix can resolve (built-in
    // xml/xmlns aside) and no default namespace applies, so elements and
    // attributes without `:` need no resolution work at all.
    let mut active_user_bindings: u32 = 0;

    if ns_aware {
        let xml_ns   = b.new_namespace(Some("xml"),   XML_NS_URI);
        let xmlns_ns = b.new_namespace(Some("xmlns"), XMLNS_NS_URI);
        ns_bindings.push((Some("xml"),   erase_ns(xml_ns)));
        ns_bindings.push((Some("xmlns"), erase_ns(xmlns_ns)));
    }

    // Borrow-from-source: when a `BytesEvent` payload arrives as
    // `Cow::Borrowed`, the bytes live in the input buffer (now owned by the
    // builder via `set_source`).  Stash a `&str` slice directly via
    // `alloc_str_borrow` — zero copy.  When the payload is `Cow::Owned`, the
    // reader had to materialize it (entity decode, char ref, encoding
    // conversion, etc.) — we copy into the bump.
    //
    // SAFETY: the input lifetime — which the borrowed Cow slices come from —
    // is the lifetime of the builder's pinned `source` buffer.  That buffer
    // moves into the `Document` on `build()` and stays alive for the
    // document's lifetime, so the `&'doc str`s produced here outlive their
    // referents only if the builder retains the source.  All public entry
    // points (`parse_str`, `parse_bytes*`) install the source
    // before calling `drive`.
    //
    // We accept `Cow<'_, [u8]>` (not `Cow<'_, str>`) so we can take payloads
    // straight from `XmlBytesReader` without bouncing through `XmlReader`'s
    // `&str`-typed wrappers (Lever 5 — see commit history).  The Scanner's
    // UTF-8 invariant means `from_utf8_unchecked` is sound at every borrow.
    #[inline]
    fn alloc_cow_bytes_as_str<'b>(
        b: &'b DocumentBuilder,
        c: std::borrow::Cow<'_, [u8]>,
    ) -> &'b str {
        match c {
            std::borrow::Cow::Borrowed(bytes) => {
                // SAFETY: Scanner UTF-8 invariant — every `Cow::Borrowed`
                // payload is a slice of the original source buffer, which
                // was validated as UTF-8 by the entry point.
                let s: &str = unsafe { std::str::from_utf8_unchecked(bytes) };
                // SAFETY: extend `'src` (the reader's input lifetime, which
                // is really `'b` — the builder's `source` buffer) to `'b`.
                // Sound because: caller installed the same buffer on the
                // builder via `set_source` before constructing the reader.
                let extended: &'b str = unsafe { &*(s as *const str) };
                unsafe { b.alloc_str_borrow(extended) }
            }
            std::borrow::Cow::Owned(v) => {
                // SAFETY: Owned payloads come from entity expansion / char
                // refs / encoding transcode — all of which write only
                // complete UTF-8 sequences into the temp buffer.
                let s: &str = unsafe { std::str::from_utf8_unchecked(&v) };
                b.alloc_str(s)
            }
        }
    }

    // Borrow an element-name byte slice as a `&'src str` in the arena.
    // Names never contain entity refs (XML 1.0 § 2.3), so they're always a
    // direct source slice — no `Cow::Owned` arm needed.
    #[inline]
    fn alloc_name_bytes_as_str<'b>(b: &'b DocumentBuilder, bytes: &[u8]) -> &'b str {
        // SAFETY: Scanner UTF-8 invariant + lifetime extension as in
        // `alloc_cow_bytes_as_str`'s Borrowed arm.
        let s: &str = unsafe { std::str::from_utf8_unchecked(bytes) };
        let extended: &'b str = unsafe { &*(s as *const str) };
        unsafe { b.alloc_str_borrow(extended) }
    }

    // Look up the AttType the DTD's `<!ATTLIST>` declared for the
    // given (element, attribute).  Returns `None` if no decl
    // covers it — falls back to CDATA semantics (no
    // normalization).
    fn dtd_att_type<'d>(
        dtd:        &'d crate::dtd::Dtd,
        elem_name:  &str,
        attr_name:  &str,
    ) -> Option<&'d crate::dtd::AttType> {
        let attlist = dtd.attlists.get(elem_name)?;
        attlist.iter().find(|d| d.name == attr_name).map(|d| &d.att_type)
    }

    // Normalize an attribute value according to the DTD-declared
    // type when present.  Returns a `&'b str` interned into the
    // builder's bump so the lifetime matches the rest of the tree.
    //
    // No-op when no `<!ATTLIST>` covers the (element, attr) pair —
    // attributes default to CDATA semantics, which need no
    // additional normalization at this layer (the byte reader has
    // already done its part).  Generic across xmlns and regular
    // attributes; the spec rules don't distinguish.
    fn dtd_normalize_attr_value<'b>(
        b:           &'b DocumentBuilder,
        dtd:         &crate::dtd::Dtd,
        elem_name:   &str,
        attr_name:   &str,
        raw_value:   &'b str,
    ) -> &'b str {
        use crate::dtd::AttType;
        // W3C `xml:id` Recommendation §4 — the attribute's value is
        // assigned ID type regardless of DTD typing, with the same
        // non-CDATA normalization (strip + collapse whitespace).
        // Fast-path the literal-name check before consulting the
        // DTD: every element pays one byte comparison, no DTD lookup
        // needed for the common case of "no xml:id, no DTD typing."
        let is_xml_id = attr_name == "xml:id";
        let needs_non_cdata = is_xml_id || {
            let att_type = dtd_att_type(dtd, elem_name, attr_name);
            matches!(
                att_type,
                Some(AttType::Id | AttType::IdRef | AttType::IdRefs
                    | AttType::Entity | AttType::Entities
                    | AttType::Nmtoken | AttType::Nmtokens
                    | AttType::Notation(_) | AttType::Enumeration(_))
            )
        };
        if !needs_non_cdata { return raw_value; }
        let normalized = normalize_non_cdata(raw_value);
        b.alloc_str(&normalized)
    }

    // Capture the source bytes for line-number translation at
    // StartElement time.  The returned slice has the input's
    // lifetime (`'src` on the reader), so it's safe to use across
    // subsequent mutable borrows of the reader.
    let src_bytes: &[u8] = reader.src_bytes();

    // XML 1.0 § 3.3.3 attribute-value normalization.
    //
    // The reader returns attribute values with no normalization
    // applied (raw bytes between the quotes).  When an ATTLIST
    // declares the attribute as non-CDATA (NMTOKEN, ID, IDREF, …),
    // the spec requires line-break and whitespace handling beyond
    // the CDATA default: strip leading/trailing spaces and collapse
    // internal runs to a single space.  Applied at the xmlns-
    // binding site so namespace URIs feed the "Unique Att Spec"
    // check after normalization — without it,
    // `xmlns:b=" urn:x "` would bind to a URI that doesn't compare
    // equal to another `xmlns:a="urn:x"`, missing a real collision.
    fn normalize_non_cdata(value: &str) -> String {
        // `value` has already been through §3.3.3 CDATA-default
        // normalization, which folds every *literal* whitespace byte
        // (space, tab, CR, LF) to a single `#x20`.  Any `#x9` / `#xA`
        // / `#xD` still present therefore arrived via a character
        // reference (`&#9;` / `&#xA;` / `&#xD;`), and §3.3.3 forbids
        // rewriting those.  The tokenized-type step layered on top
        // accordingly collapses runs of — and trims leading/trailing —
        // `#x20` ONLY, leaving character-reference whitespace intact.
        let bytes = value.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut in_run = true; // leading-trim by treating start as in-run
        for &b in bytes {
            if b == b' ' {
                if !in_run {
                    out.push(b' ');
                    in_run = true;
                }
            } else {
                // Every non-space byte (ASCII or a UTF-8 lead /
                // continuation byte) is copied verbatim — `#x20` never
                // occurs inside a multi-byte sequence, so this can't
                // split one.
                out.push(b);
                in_run = false;
            }
        }
        if out.last() == Some(&b' ') { out.pop(); }
        // SAFETY: `value` was valid UTF-8 and we only dropped or
        // relocated standalone `#x20` bytes, never touching the bytes
        // of a multi-byte sequence.
        unsafe { String::from_utf8_unchecked(out) }
    }
    loop {
        // attr_buf is drained inside the StartElement arm below, so it
        // re-enters the loop empty.  No `clear()` is needed at the top.
        debug_assert!(attr_buf.is_empty());
        match reader.next()? {
            BytesEvent::StartElement(tag) => {
                // Element name: borrow from source on the common
                // path; copy into the arena when the tag came from
                // an entity-replacement stream (those bytes are
                // owned by the tag and die at end-of-match-arm, so
                // a borrow would dangle).
                let name: &str = match tag.name_cow() {
                    std::borrow::Cow::Borrowed(bytes) => {
                        alloc_name_bytes_as_str(b, bytes)
                    }
                    std::borrow::Cow::Owned(bytes) => {
                        b.alloc_str(unsafe { std::str::from_utf8_unchecked(&bytes) })
                    }
                };
                // In c-abi mode + namespace-aware parsing, `Node::name`
                // follows libxml2's convention: the local part only.
                // The prefix lives on `node.ns->prefix` after namespace
                // resolution.  Consumers (lxml, libxslt) read names as
                // local strings and combine with `ns->href` to form
                // the expanded tag; keeping the prefix here breaks tag
                // equality, attribute lookup, and namespacedNameFromNsName.
                //
                // Without namespace awareness, names stay raw (with any
                // prefix) — internal regression tests and tools that
                // process documents pre-namespace-resolution rely on
                // that.
                #[cfg(feature = "c-abi")]
                let elem_name = if ns_aware {
                    match memchr::memchr(b':', name.as_bytes()) {
                        Some(idx) => &name[idx + 1..],
                        None      => name,
                    }
                } else {
                    name
                };
                #[cfg(not(feature = "c-abi"))]
                let elem_name = name;
                let el   = b.new_element(elem_name);
                // Record the source line number so consumers can
                // ask for `node.line` / lxml's `Element.sourceline`.
                // libxml2 caps the field at u16 (65535) — values
                // beyond that get the special "encoded line" trick
                // (`(extra >> 16)`); we keep things simple and
                // saturate.
                {
                    let name_offset = (tag.name_offset() as usize).min(src_bytes.len());
                    // Advance the line cursor through any new bytes since
                    // the last StartElement.  Element name offsets are
                    // monotonically non-decreasing through the parse, so the
                    // cursor only ever moves forward; on the rare chance it
                    // would move backward (shouldn't happen in practice) we
                    // leave the cursor in place and the recorded line is the
                    // last seen value — never worse than the old quadratic
                    // path that would just produce the same number with
                    // more work.
                    if name_offset > line_cursor.0 {
                        let slice = &src_bytes[line_cursor.0..name_offset];
                        line_cursor.1 += memchr::memchr_iter(b'\n', slice).count() as u32;
                        line_cursor.0 = name_offset;
                    }
                    let raw_line = line_cursor.1;
                    // The `line` field is `u16` in c-abi mode
                    // (matching libxml2's 16-bit slot) and `u32`
                    // in the lean build (no ABI constraint, so we can
                    // handle large files better).
                    // Saturate when narrowing to u16.
                    #[cfg(feature = "c-abi")]
                    let line: u16 = raw_line.min(u16::MAX as u32) as u16;
                    #[cfg(not(feature = "c-abi"))]
                    let line: u32 = raw_line;
                    el.line = line;
                    // Keep the uncapped line for files past 65535 lines;
                    // `xmlGetLineNo` returns it in preference to the
                    // saturated `line`.
                    #[cfg(feature = "c-abi")]
                    {
                        el.full_line = raw_line;
                    }
                }
                // Entity-stream start tags carry their attrs pre-
                // parsed (the lazy iterator can't surface bytes that
                // don't live in `src`).  Take them out first; the
                // lazy `attrs()` path returns nothing in that case.
                let entity_attr_pairs: Option<Vec<(Vec<u8>, Vec<u8>)>> =
                    tag.entity_attrs().map(|v| v.to_vec());

                // Skip the attribute iterator entirely when there's
                // nothing between the name and the closing `>` — a
                // single empty-slice check is cheaper than constructing
                // a Scanner just to have its `next()` immediately return
                // None.  Trailing whitespace inside the start tag still
                // falls through (rare in practice; attrs() short-
                // circuits on the first .next() either way).
                if !tag.attrs_bytes().is_empty() {
                    for a in tag.attrs() { attr_buf.push(a?); }
                }

                // Splice pre-parsed entity-stream attrs into the buffer
                // so the existing ns-aware / ns-blind loops below see
                // them uniformly.  Bytes are arena-copied; the
                // lifetime cast launders the arena's lifetime to the
                // attr_buf slot, sound because the arena outlives the
                // BytesAttr.
                if let Some(pairs) = entity_attr_pairs {
                    for (n_bytes, v_bytes) in pairs {
                        let n_arena: &str = b.alloc_str(unsafe {
                            std::str::from_utf8_unchecked(&n_bytes)
                        });
                        let v_arena: &str = b.alloc_str(unsafe {
                            std::str::from_utf8_unchecked(&v_bytes)
                        });
                        let n_slice: &[u8] = n_arena.as_bytes();
                        let v_slice: &[u8] = v_arena.as_bytes();
                        let n_ext: &[u8] = unsafe { &*(n_slice as *const [u8]) };
                        let v_ext: &[u8] = unsafe { &*(v_slice as *const [u8]) };
                        attr_buf.push(BytesAttr {
                            name:  n_ext,
                            value: std::borrow::Cow::Borrowed(v_ext),
                        });
                    }
                }

                if !ns_aware {
                    // Fastest path — namespace-blind, mirrors legacy parse_bytes.
                    for a in attr_buf.drain(..) {
                        // Attr name is always &'src [u8] (no entity decode for
                        // names per XML 1.0 § 2.3) so we can always borrow it.
                        let aname  = alloc_name_bytes_as_str(b, a.name);
                        let raw_avalue = alloc_cow_bytes_as_str(b, a.value);
                        // XML 1.0 § 3.3.3 non-CDATA normalization — same
                        // helper the ns-aware branch uses.  No-op when
                        // no ATTLIST covers this attribute.
                        let avalue: &str =
                            dtd_normalize_attr_value(b, reader.dtd(), name, aname, raw_avalue);
                        let attr   = b.new_attribute(aname, avalue);
                        b.append_attribute(el, attr);
                    }
                } else {
                    // ── namespace-aware path ──────────────────────────────────
                    validate_qname(name, "element")?;
                    ns_frames.push(ns_bindings.len());
                    let mut new_bindings_this_frame = 0u32;
                    let mut any_attr_prefixed = false;
                    // Prefixed attributes paired with their original QName.
                    // Namespace resolution must wait until every `xmlns`
                    // declaration on this element has been seen, so collect
                    // them and resolve in the pass below.  In c-abi the
                    // stored `name` is already reduced to the local part
                    // (libxml2 convention); the prefix is recovered from the
                    // QName recorded here.
                    let mut prefixed_attrs: Vec<(&Attribute<'_>, &str)> = Vec::new();

                    for a in attr_buf.drain(..) {
                        let aname  = alloc_name_bytes_as_str(b, a.name);
                        let raw_avalue = alloc_cow_bytes_as_str(b, a.value);

                        // Namespace declarations (`xmlns` / `xmlns:foo`)
                        // do NOT belong in the element's attribute
                        // list under the c-abi layout — libxml2's
                        // `_xmlNode::properties` holds only real
                        // attributes; xmlns declarations live on the
                        // separate `nsDef` chain.  In the lean
                        // (non-c-abi) build we keep them in both
                        // places for backwards-compatibility with
                        // existing API consumers that iterate
                        // `attributes()` expecting xmlns decls there.
                        let is_xmlns_decl =
                            aname == "xmlns" || aname.starts_with("xmlns:");

                        // XML 1.0 § 3.3.3 attribute-value normalization.
                        // Applied uniformly: if an `<!ATTLIST>` declared
                        // this attribute as non-CDATA (ID, IDREF,
                        // IDREFS, ENTITY/ENTITIES, NMTOKEN/NMTOKENS,
                        // NOTATION, enumeration), strip leading/trailing
                        // whitespace and collapse internal runs to a
                        // single space.  Without this, ID equality,
                        // IDREF resolution, and enumeration matching
                        // would all silently disagree with the spec on
                        // values like `id=" abc "`.  Helper no-ops when
                        // no ATTLIST covers the pair, so non-DTD docs
                        // pay nothing.
                        let avalue: &str =
                            dtd_normalize_attr_value(b, reader.dtd(), name, aname, raw_avalue);

                        #[cfg(not(feature = "c-abi"))]
                        let always_attach = true;
                        #[cfg(feature = "c-abi")]
                        let always_attach = false;
                        let is_prefixed = !is_xmlns_decl
                            && memchr::memchr(b':', aname.as_bytes()).is_some();
                        if always_attach || !is_xmlns_decl {
                            // c-abi follows libxml2: `name` holds the local
                            // part only, with the prefix carried on
                            // `attr->ns` after resolution.  Mirror the
                            // element-name reduction so a prefixed attribute
                            // doesn't serialize as `p:p:name` and `keys()`
                            // reports the bare local name.  The lean build
                            // keeps the raw QName (its serializer never
                            // re-prepends a prefix).
                            #[cfg(feature = "c-abi")]
                            let stored_name = if ns_aware && is_prefixed {
                                let i = memchr::memchr(b':', aname.as_bytes()).unwrap();
                                &aname[i + 1..]
                            } else {
                                aname
                            };
                            #[cfg(not(feature = "c-abi"))]
                            let stored_name = aname;
                            let attr: &Attribute<'_> = b.new_attribute(stored_name, avalue);
                            b.append_attribute(el, attr);
                            if ns_aware && is_prefixed {
                                prefixed_attrs.push((attr, aname));
                            }
                        }

                        if aname == "xmlns" {
                            let ns = b.new_namespace(None, avalue);
                            ns_bindings.push((None, erase_ns(ns)));
                            #[cfg(feature = "c-abi")]
                            { b.append_ns_def(el, ns); }
                            new_bindings_this_frame += 1;
                        } else if let Some(local) = aname.strip_prefix("xmlns:") {
                            validate_xmlns_decl(local, avalue)?;
                            if local == "xml" { continue; }
                            let ns = b.new_namespace(Some(local), avalue);
                            ns_bindings.push((Some(local), erase_ns(ns)));
                            #[cfg(feature = "c-abi")]
                            { b.append_ns_def(el, ns); }
                            new_bindings_this_frame += 1;
                        } else if memchr::memchr(b':', aname.as_bytes()).is_some() {
                            any_attr_prefixed = true;
                        }
                    }
                    active_user_bindings += new_bindings_this_frame;

                    // ── Element QName resolution ──
                    // Fast path: no prefix and no user bindings ever in scope → no namespace.
                    let elem_has_colon = memchr::memchr(b':', name.as_bytes()).is_some();
                    if elem_has_colon || active_user_bindings > 0 {
                        let el_ns = resolve_qname(name, &ns_bindings, /*is_attribute=*/ false)?;
                        el.namespace.set(el_ns);
                    }

                    // ── Attribute QName resolution + dup-by-expanded-name check ──
                    // Skip the FxHashSet allocation entirely when no attribute is prefixed
                    // (no namespace = no collision possible after expansion).
                    if any_attr_prefixed {
                        // Resolve each prefixed attribute's namespace now that
                        // every xmlns declaration on this element is in scope.
                        // `resolve_qname` takes the original prefixed QName
                        // recorded at creation; the stored `name` may already
                        // be the local part (c-abi).
                        for &(attr, qname) in &prefixed_attrs {
                            validate_qname(qname, "attribute")?;
                            let ns = resolve_qname(qname, &ns_bindings, true)?;
                            attr.namespace.set(ns);
                        }
                        // XML NS § 6.3: no two attributes may share an expanded
                        // name (namespace-uri, local-name).  The local part is
                        // `name` after the colon — which equals `name` itself
                        // once reduced (c-abi).
                        let mut seen: FxHashSet<(&str, &str)> = FxHashSet::default();
                        let mut attr_cur = el.first_attribute.get();
                        while let Some(attr) = attr_cur {
                            if attr.name() != "xmlns" && !attr.name().starts_with("xmlns:") {
                                let ns_uri = attr.namespace.get().map(|n| n.href()).unwrap_or("");
                                let local  = attr.name().rfind(':')
                                    .map(|i| &attr.name()[i + 1..])
                                    .unwrap_or(attr.name());
                                if !seen.insert((ns_uri, local)) {
                                    return Err(ns_err(if ns_uri.is_empty() {
                                        format!("duplicate attribute '{local}' after namespace expansion")
                                    } else {
                                        format!("duplicate attribute '{local}' in namespace '{ns_uri}' after namespace expansion")
                                    }));
                                }
                            }
                            attr_cur = attr.next.get();
                        }
                    }
                    // Stash the per-frame binding count in ns_frames is not enough — we
                    // also need to know how many to subtract from active_user_bindings
                    // on EndElement.  Pack the count into the frame marker by using a
                    // sentinel: separate Vec for the counts.  (Simpler: piggyback on
                    // ns_frames by recording `new_bindings_this_frame` in a parallel Vec.)
                    // For now we track via a side Vec.
                    ns_frame_count_stack.push(new_bindings_this_frame);
                }

                // Attach to parent (if any) — first StartElement becomes the root.
                if let Some(&parent_ptr) = stack.last() {
                    // SAFETY: parent_ptr points into `b`, still alive here.
                    let parent: &Node<'_> = unsafe { unerase(parent_ptr) };
                    b.append_child(parent, el);
                } else {
                    root = Some(erase(el));
                }
                stack.push(erase(el));
            }
            BytesEvent::EndElement(_) => {
                stack.pop().expect("EndElement without StartElement — XmlBytesReader invariant");
                if ns_aware {
                    if let Some(frame_start) = ns_frames.pop() {
                        ns_bindings.truncate(frame_start);
                    }
                    if let Some(count) = ns_frame_count_stack.pop() {
                        active_user_bindings -= count;
                    }
                }
            }
            BytesEvent::Text(t)    => attach_leaf(b, &stack, b.new_text   (alloc_cow_bytes_as_str(b, t.into_bytes())), root.is_some()),
            BytesEvent::CData(s)   => {
                // `cdata_as_text` (libxml2 NOCDATA / lxml strip_cdata)
                // delivers CDATA content as a plain text node.
                let content = alloc_cow_bytes_as_str(b, s.into_bytes());
                let leaf = if opts.cdata_as_text { b.new_text(content) } else { b.new_cdata(content) };
                attach_leaf(b, &stack, leaf, root.is_some());
            }
            BytesEvent::Comment(s) => {
                // `remove_comments` (lxml NULLs the SAX comment callback):
                // skip building the node entirely.
                if !opts.remove_comments {
                    attach_leaf(b, &stack, b.new_comment(alloc_cow_bytes_as_str(b, s.into_bytes())), root.is_some());
                }
            }
            BytesEvent::Pi(p)      => {
                if !opts.remove_pis {
                    let (t, c) = p.into_parts();
                    let target  = alloc_cow_bytes_as_str(b, t);
                    // A PI with no data section serializes without the
                    // trailing space; libxml2 marks that as NULL content.
                    let content = if c.is_empty() { None } else { Some(alloc_cow_bytes_as_str(b, c)) };
                    attach_leaf(b, &stack, b.new_pi(target, content), root.is_some());
                }
            }
            BytesEvent::EntityRef(e) => {
                // `resolve_entities: false` left an `&name;` literal in
                // the source; materialize it as a dedicated
                // `NodeKind::EntityRef` node so the tree round-trips
                // back to source and lxml's `tag == Entity` /
                // `text == "&name;"` semantics work.
                let name_bytes = e.name();
                // SAFETY: Scanner UTF-8 invariant — entity-name bytes
                // are valid UTF-8 (NameChar+).
                let name: &str = unsafe { std::str::from_utf8_unchecked(name_bytes) };
                let name = b.alloc_str(name);
                // Literal source form `&name;` for the serializer.
                let lit = format!("&{name};");
                let content = b.alloc_str(&lit);
                attach_leaf(b, &stack, b.new_entity_ref(name, content), root.is_some());
            }
            BytesEvent::Eof => break,
        }
    }

    let root_ptr = root.ok_or_else(|| XmlError::new(
        ErrorDomain::Parser, ErrorLevel::Fatal,
        "document has no root element",
    ))?;
    // SAFETY: root_ptr was erased from a node allocated in `b`; `b` is still
    // alive at this line.
    let root_ref: &Node<'_> = unsafe { unerase(root_ptr) };
    b.set_root(root_ref);

    // Plumb XML declaration fields from the reader's prolog state into the
    // Document.  When the document had no `<?xml ... ?>` declaration the
    // builder's defaults ("1.0" / "UTF-8" / None) are kept.
    if let Some(decl) = reader.xml_decl() {
        b.set_version(decl.version.clone());
        if let Some(enc) = &decl.encoding {
            b.set_encoding(enc.clone());
        }
        b.set_standalone(decl.standalone);
    }

    Ok(())
}

// ── namespace helpers ───────────────────────────────────────────────────────

#[inline] fn erase_ns(ns: &Namespace<'_>) -> *const () {
    ns as *const Namespace<'_> as *const ()
}

/// # Safety
///
/// `p` must have been produced by `erase_ns` from a live `&Namespace`,
/// and the caller-chosen lifetime `'a` must not outlive that namespace's
/// arena.
#[inline] unsafe fn unerase_ns<'a>(p: *const ()) -> &'a Namespace<'a> {
    unsafe { &*(p as *const Namespace<'a>) }
}

/// Find the innermost binding for `prefix` (None = default namespace).
/// Returns `None` if no binding is in scope.
fn lookup_ns<'a>(
    prefix:   Option<&str>,
    bindings: &[(Option<&'a str>, *const ())],
) -> Option<&'a Namespace<'a>> {
    for &(p, ns_ptr) in bindings.iter().rev() {
        if p == prefix {
            // SAFETY: ns_ptr was minted by erase_ns from a Namespace allocated
            // in the builder's arena, which is still alive while this function
            // runs (called from inside drive() which owns the arena).
            return Some(unsafe { unerase_ns(ns_ptr) });
        }
    }
    None
}

/// Resolve a QName against the namespace scope.  Unprefixed elements use the
/// default namespace; unprefixed attributes never do (XML Namespaces § 6.2).
fn resolve_qname<'a>(
    qname:        &'a str,
    bindings:     &[(Option<&'a str>, *const ())],
    is_attribute: bool,
) -> Result<Option<&'a Namespace<'a>>> {
    if let Some(colon) = qname.find(':') {
        let prefix = &qname[..colon];
        match lookup_ns(Some(prefix), bindings) {
            Some(ns) => Ok(Some(ns)),
            None     => Err(ns_err(format!("undeclared namespace prefix '{prefix}' in '{qname}'"))),
        }
    } else if is_attribute {
        Ok(None)
    } else {
        // Unprefixed element: default namespace if declared with non-empty URI.
        match lookup_ns(None, bindings) {
            Some(ns) if !ns.href().is_empty() => Ok(Some(ns)),
            _                                => Ok(None),
        }
    }
}


/// Attach a freshly-allocated leaf node to the top-of-stack element.
/// When the stack is empty (prolog before the root or epilogue
/// after `</root>`) the leaf is recorded as a document-level
/// orphan instead of dropped; [`DocumentBuilder::build`] later
/// links it as a sibling of the root so consumers see comments /
/// PIs that appeared outside the document element.
///
/// `after_root` tells the builder whether this orphan goes in the
/// prolog (false: root not yet seen) or the epilogue (true: root
/// element has opened, regardless of whether it has also closed).
fn attach_leaf<'a>(
    b:           &'a DocumentBuilder,
    stack:       &[ErasedNodePtr],
    node:        &'a Node<'a>,
    after_root:  bool,
) {
    if let Some(&parent_ptr) = stack.last() {
        // SAFETY: stack invariant — pointer points into the live `b.bump`,
        // which is the same arena `node` lives in, so unifying lifetimes is sound.
        let parent: &'a Node<'a> = unsafe { unerase(parent_ptr) };
        b.append_child(parent, node);
    } else if after_root {
        b.attach_epilogue_orphan(node);
    } else {
        b.attach_prolog_orphan(node);
    }
}

// Silence the unused-`ErasedAttrPtr` lint until we extend to namespace tables.
#[allow(dead_code)] type _AttrAlias = ErasedAttrPtr;

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_tree::dom::NodeKind;

    fn parse(xml: &str) -> Document {
        // Default `ParseOptions` has `namespace_aware: false` — most tests here
        // exercise the structural shape (children, attrs, kinds), so it doesn't
        // matter.  Namespace-specific tests use `parse_ns` instead.
        parse_str(xml, &ParseOptions::default()).expect("parse")
    }

    /// Namespace-aware parse helper for the ns-resolution tests.
    fn parse_ns(xml: &str) -> Document {
        let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
        parse_str(xml, &opts).expect("parse")
    }

    #[test]
    fn empty_element() {
        let doc = parse("<r/>");
        assert_eq!(doc.root().name(), "r");
        assert!(doc.root().children().next().is_none());
    }

    // ── XML 1.0 § 3.3.3 attribute-value normalization for xmlns ──

    /// `xmlns:b` declared as NMTOKEN: leading/trailing whitespace
    /// in the URI literal MUST be stripped before the URI binds.
    /// CDATA attributes get no such treatment.  Without normalization
    /// `xmlns:a="urn:x"` and `xmlns:b=" urn:x "` would bind to two
    /// distinct URIs even though the spec treats them as one — the
    /// downstream "Unique Att Spec" check would then miss real
    /// namespace collisions like W3C rmt-ns10-012.
    #[test]
    fn xmlns_nmtoken_value_is_stripped_before_binding() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE foo [
<!ELEMENT foo ANY>
<!ATTLIST foo xmlns:a CDATA   #IMPLIED
              xmlns:b NMTOKEN #IMPLIED>
]>
<foo xmlns:a="urn:x" xmlns:b="  urn:x  "/>"#;
        let doc = parse_ns(xml);
        let root = doc.root();
        // Both prefixes should now bind to the same URI string.
        // Walk the namespace defs and check.
        let nsdefs: Vec<(Option<&str>, &str)> = {
            #[cfg(feature = "c-abi")]
            { root.ns_declarations().collect() }
            #[cfg(not(feature = "c-abi"))]
            {
                // Without c-abi we exposed xmlns decls as attributes
                // instead of nsDef.  Pull from the attribute list.
                let mut out = Vec::new();
                let mut a = root.first_attribute.get();
                while let Some(attr) = a {
                    let name = attr.name();
                    if name == "xmlns" {
                        out.push((None, attr.value()));
                    } else if let Some(p) = name.strip_prefix("xmlns:") {
                        out.push((Some(p), attr.value()));
                    }
                    a = attr.next.get();
                }
                out
            }
        };
        let a_uri = nsdefs.iter().find(|(p, _)| *p == Some("a")).map(|(_, u)| *u);
        let b_uri = nsdefs.iter().find(|(p, _)| *p == Some("b")).map(|(_, u)| *u);
        assert_eq!(a_uri, Some("urn:x"));
        assert_eq!(b_uri, Some("urn:x"),
            "NMTOKEN normalization should strip whitespace; got {b_uri:?}");
    }

    /// CDATA-typed xmlns (the default when no ATTLIST covers it):
    /// whitespace inside the value is preserved.  This is the spec
    /// behaviour — without an `<!ATTLIST>` redeclaring the type the
    /// raw value goes through.  Guards against over-normalization.
    #[test]
    fn xmlns_cdata_value_is_not_stripped() {
        let xml = r#"<r xmlns:b="  urn:x  "/>"#;
        let doc = parse_ns(xml);
        let root = doc.root();
        let b_uri = {
            #[cfg(feature = "c-abi")]
            { root.ns_declarations().find(|(p, _)| *p == Some("b")).map(|(_, u)| u) }
            #[cfg(not(feature = "c-abi"))]
            {
                let mut a = root.first_attribute.get();
                let mut found = None;
                while let Some(attr) = a {
                    if attr.name() == "xmlns:b" { found = Some(attr.value()); break; }
                    a = attr.next.get();
                }
                found
            }
        };
        assert_eq!(b_uri, Some("  urn:x  "),
            "no ATTLIST → no non-CDATA normalization, URI must stay verbatim");
    }

    // ── XML Namespaces 1.0 § 6.3 "Unique Att Spec" ──

    /// Two prefixes binding to the SAME namespace URI on the same
    /// element make `a:attr` and `b:attr` resolve to the same
    /// expanded name `{URI}attr` — must be rejected.  Catches the
    /// straightforward (same-element) collision.
    #[test]
    fn duplicate_expanded_attribute_name_rejected() {
        let xml = r#"<r xmlns:a="urn:x" xmlns:b="urn:x" a:attr="1" b:attr="2"/>"#;
        let err = parse_str(xml, &ParseOptions { namespace_aware: true, ..ParseOptions::default() })
            .expect_err("two prefixes binding the same URI with same local must be rejected");
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("duplicate") && (msg.contains("namespace") || msg.contains("attribute")),
            "expected duplicate-attribute message, got: {err}");
    }

    /// The same collision but the xmlns declarations live on a
    /// parent element — collision still detected because the
    /// namespace resolver walks the full in-scope binding stack.
    /// Mirrors W3C rmt-ns10-012's structural shape (xmlns on
    /// outer, prefixed attrs on inner) without the DTD-driven
    /// normalization layer.
    #[test]
    fn duplicate_expanded_attribute_name_rejected_across_elements() {
        let xml = r#"<f xmlns:a="urn:x" xmlns:b="urn:x"><g a:attr="1" b:attr="2"/></f>"#;
        let err = parse_str(xml, &ParseOptions { namespace_aware: true, ..ParseOptions::default() })
            .expect_err("inner-element collision must be caught through inherited bindings");
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("duplicate"),
            "expected duplicate-attribute message, got: {err}");
    }

    /// The combined case — DTD-aware normalization + namespace-
    /// aware uniqueness must both fire to catch this.  This is
    /// exactly W3C rmt-ns10-012's input shape, distilled to the
    /// minimum needed to flip the test verdict.  Drops the W3C
    /// suite's external machinery so the failure mode is testable
    /// from inside the core crate without the harness.
    #[test]
    fn nmtoken_normalization_unlocks_ns_uniqueness_collision() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE foo [
<!ELEMENT foo ANY>
<!ATTLIST foo xmlns:a CDATA   #IMPLIED
              xmlns:b NMTOKEN #IMPLIED>
<!ELEMENT bar ANY>
<!ATTLIST bar a:attr CDATA #IMPLIED
              b:attr CDATA #IMPLIED>
]>
<foo xmlns:a="urn:x" xmlns:b=" urn:x ">
<bar a:attr="1" b:attr="2"/>
</foo>"#;
        let err = parse_str(xml, &ParseOptions { namespace_aware: true, ..ParseOptions::default() })
            .expect_err("rmt-ns10-012 minimum repro must be rejected once NMTOKEN normalization \
                         strips the whitespace from xmlns:b's value");
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("duplicate"),
            "expected duplicate-after-expansion error, got: {err}");
    }

    /// XML 1.0 § 3.3.3 attribute-value normalization for a regular
    /// (non-xmlns) NMTOKEN attribute: leading/trailing whitespace
    /// stripped, internal runs collapsed.  Confirms the normalization
    /// helper applies to ALL non-CDATA attributes, not just xmlns:*.
    #[test]
    fn nmtoken_attribute_value_is_normalized() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE r [
<!ELEMENT r EMPTY>
<!ATTLIST r kind NMTOKEN #IMPLIED>
]>
<r kind="  alpha  "/>"#;
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let kind = doc.root().attributes()
            .find(|a| a.name() == "kind")
            .map(|a| a.value());
        assert_eq!(kind, Some("alpha"),
            "NMTOKEN value should be stripped; got {kind:?}");
    }

    /// Same rule for ID-typed attributes — internal whitespace
    /// collapses too.  Without normalization, two `id` values that
    /// differ only by whitespace would compare unequal under
    /// `getElementById` etc.
    #[test]
    fn id_attribute_value_is_normalized_collapsed() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE r [
<!ELEMENT r EMPTY>
<!ATTLIST r tag ID #IMPLIED>
]>
<r tag="  a   b   c  "/>"#;
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let tag = doc.root().attributes()
            .find(|a| a.name() == "tag")
            .map(|a| a.value());
        assert_eq!(tag, Some("a b c"),
            "ID value should strip + collapse; got {tag:?}");
    }

    /// CDATA-typed attribute (the default when no ATTLIST covers it)
    /// preserves whitespace verbatim — guards against
    /// over-normalization of CDATA-shaped values.
    #[test]
    fn cdata_attribute_value_is_not_stripped() {
        // No ATTLIST → defaults to CDATA → no non-CDATA stripping.
        let xml = r#"<r tag="  a   b  "/>"#;
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let tag = doc.root().attributes()
            .find(|a| a.name() == "tag")
            .map(|a| a.value());
        assert_eq!(tag, Some("  a   b  "),
            "CDATA-defaulted value must stay verbatim; got {tag:?}");
    }

    /// Enumeration-typed attribute: stripped before matching the
    /// enum.  Without this, `<r kind=" yes ">` would silently fail
    /// even though the declared enum is `(yes|no)` — and we'd quietly
    /// accept the un-matched value instead of validating against it.
    #[test]
    fn enumeration_attribute_value_is_normalized() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE r [
<!ELEMENT r EMPTY>
<!ATTLIST r kind (yes|no) #IMPLIED>
]>
<r kind="  yes  "/>"#;
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let kind = doc.root().attributes()
            .find(|a| a.name() == "kind")
            .map(|a| a.value());
        assert_eq!(kind, Some("yes"));
    }

    // ── resolve_entities = false (EntityRef node kind) ──

    /// Default: entity references expand inline into Text.  Confirms
    /// the baseline behaviour the new flag opts out of.
    #[test]
    fn resolve_entities_default_expands_inline() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY hi "hello">]>
<r>&hi;</r>"#;
        let doc = parse_str(xml, &ParseOptions::default()).expect("parse");
        let root = doc.root();
        // Should have a single Text child "hello", no EntityRef.
        let kinds: Vec<NodeKind> = root.children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![NodeKind::Text]);
        let text = root.children().next().unwrap().content();
        assert_eq!(text, "hello");
    }

    /// `resolve_entities = false` preserves user-defined references
    /// as `NodeKind::EntityRef` nodes whose `name` is the entity
    /// name and whose `content` round-trips to `&name;` source.
    #[test]
    fn resolve_entities_false_emits_entity_ref_node() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE r [<!ENTITY hi "hello">]>
<r>before &hi; after</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let doc = parse_str(xml, &opts).expect("parse");
        let root = doc.root();
        let kinds: Vec<NodeKind> = root.children().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            vec![NodeKind::Text, NodeKind::EntityRef, NodeKind::Text],
            "expected Text-EntityRef-Text triple, got {kinds:?}"
        );
        // The middle child carries the entity name + literal form.
        let ref_node = root.children().nth(1).unwrap();
        assert_eq!(ref_node.name(),    "hi");
        assert_eq!(ref_node.content(), "&hi;");
    }

    /// Predefined entities always expand regardless of the flag —
    /// they're part of the character data production, not the
    /// entity-reference machinery.
    #[test]
    fn resolve_entities_false_still_expands_predefined() {
        let xml = r#"<r>a &amp; b &lt; c</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let doc = parse_str(xml, &opts).expect("parse");
        let root = doc.root();
        let kinds: Vec<NodeKind> = root.children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![NodeKind::Text],
            "predefined entities must expand inline, got {kinds:?}");
        assert_eq!(root.children().next().unwrap().content(), "a & b < c");
    }

    /// Numeric character references always expand inline too.
    #[test]
    fn resolve_entities_false_still_expands_numeric() {
        let xml = r#"<r>before &#65; after</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let doc = parse_str(xml, &opts).expect("parse");
        let root = doc.root();
        let kinds: Vec<NodeKind> = root.children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![NodeKind::Text]);
        assert_eq!(root.children().next().unwrap().content(), "before A after");
    }

    /// Round-trip: serialize a tree with EntityRef nodes back to
    /// source.  The literal `&name;` form is preserved.
    #[test]
    fn resolve_entities_false_round_trips_through_serializer() {
        let xml = r#"<r>x &hi; y</r>"#;
        let opts = ParseOptions { resolve_entities: false, ..ParseOptions::default() };
        let doc = parse_str(xml, &opts).expect("parse");
        let out = crate::serialize_to_string(&doc);
        assert!(out.contains("&hi;"),
            "EntityRef should serialize as `&hi;`, got: {out}");
    }

    /// Edition guard: without `namespace_aware`, lexical names rule.
    /// `a:attr` and `b:attr` are different names → accepted.  Same
    /// input as the previous tests, but the namespace pass doesn't
    /// run.  Confirms the new check is properly gated.
    #[test]
    fn duplicate_expanded_name_accepted_when_namespace_blind() {
        let xml = r#"<r xmlns:a="urn:x" xmlns:b="urn:x" a:attr="1" b:attr="2"/>"#;
        // namespace_aware = false (the default).
        parse_str(xml, &ParseOptions::default())
            .expect("namespace-blind parse must accept lexically-distinct attribute names");
    }

    #[test]
    fn line_numbers_basic() {
        // line 1: <r>
        // line 2:   <a/>
        // line 3:   <b/>
        // line 4: </r>
        let doc = parse("<r>\n  <a/>\n  <b/>\n</r>");
        let root = doc.root();
        let a = root.children().find(|n| n.is_element()).unwrap();
        let b = a.next_sibling.get().and_then(|n|
            if n.is_element() { Some(n) } else { n.next_sibling.get() }
        ).unwrap();

        assert_eq!(root.line, 1, "root <r> on line 1");
        assert_eq!(a.line,    2, "<a/> on line 2");
        assert_eq!(b.line,    3, "<b/> on line 3");
    }

    /// Regression guard: a previous implementation called
    /// `scanner::compute_line_col` once per StartElement, which rescans
    /// `src[0..name_offset]` from byte 0 each call — O(N × file_size).
    /// On docs with many elements this slowed the parser by 10×–100×.
    /// The current implementation maintains an incremental cursor in
    /// `drive()`; this test exercises it across many lines to make sure
    /// the cursor stays in sync.
    #[test]
    fn line_numbers_many_elements() {
        // 300 elements, one per line.  Each <e i="N"/> on line N+1
        // (the <root> opener is line 1).
        let mut src = String::from("<root>\n");
        let n = 300u32;
        for i in 0..n {
            src.push_str(&format!("  <e i=\"{i}\"/>\n"));
        }
        src.push_str("</root>\n");

        let doc = parse(&src);
        assert_eq!(doc.root().line, 1);

        let mut expected: u32 = 2;
        let mut walked = 0u32;
        for child in doc.root().children().filter(|n| n.is_element()) {
            // line is u16 in c-abi mode, u32 in lean — both fit < 65535 here.
            assert_eq!(child.line as u32, expected,
                "child #{walked}: expected line {expected}, got {}", child.line);
            walked += 1;
            expected += 1;
        }
        assert_eq!(walked, n);
    }

    #[test]
    fn nested_elements() {
        let doc = parse("<a><b><c/></b></a>");
        let a = doc.root();
        assert_eq!(a.name(), "a");
        let b = a.children().next().unwrap();
        assert_eq!(b.name(), "b");
        let c = b.children().next().unwrap();
        assert_eq!(c.name(), "c");
        // parent pointers
        assert!(std::ptr::eq(c.parent.get().unwrap(), b));
        assert!(std::ptr::eq(b.parent.get().unwrap(), a));
    }

    #[test]
    fn attributes_in_order() {
        let doc = parse(r#"<el id="1" class="x" data-y="42"/>"#);
        let pairs: Vec<(&str, &str)> = doc.root().attributes()
            .map(|a| (a.name(), a.value()))
            .collect();
        assert_eq!(pairs, vec![("id", "1"), ("class", "x"), ("data-y", "42")]);
    }

    #[test]
    fn mixed_content() {
        let doc = parse("<r>before<!-- c --><b>x</b><![CDATA[<raw>]]>after</r>");
        let kinds: Vec<NodeKind> = doc.root().children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![
            NodeKind::Text, NodeKind::Comment, NodeKind::Element,
            NodeKind::CData, NodeKind::Text,
        ]);
    }

    #[test]
    fn pi_inside_element() {
        let doc = parse(r#"<r><?xml-stylesheet href="s.xsl"?></r>"#);
        let pi = doc.root().children().next().unwrap();
        assert_eq!(pi.kind, NodeKind::Pi);
        assert_eq!(pi.name(), "xml-stylesheet");
        assert_eq!(pi.content(), r#"href="s.xsl""#);
    }

    #[test]
    fn entity_in_text_is_expanded() {
        let doc = parse("<r>a&amp;b&lt;c</r>");
        let t = doc.root().children().next().unwrap();
        assert_eq!(t.kind, NodeKind::Text);
        assert_eq!(t.content(), "a&b<c");
    }

    #[test]
    fn entity_in_attr_is_expanded() {
        let doc = parse(r#"<r v="a&amp;b"/>"#);
        let v = doc.root().attributes().next().unwrap();
        assert_eq!(v.value(), "a&b");
    }

    #[test]
    fn deeply_nested_doc() {
        let mut xml = String::new();
        for _ in 0..50 { xml.push_str("<n>"); }
        xml.push_str("hello");
        for _ in 0..50 { xml.push_str("</n>"); }
        let doc = parse(&xml);
        let mut cur = doc.root();
        let mut depth = 1;
        while let Some(c) = cur.children().next() {
            if c.kind == NodeKind::Element { cur = c; depth += 1; } else { break; }
        }
        assert_eq!(depth, 50);
        assert_eq!(cur.children().next().unwrap().content(), "hello");
    }

    #[test]
    fn root_lifetime_keeps_tree_alive() {
        let doc = parse("<r><a><b>x</b></a></r>");
        let a = doc.root().children().next().unwrap();
        let b = a.children().next().unwrap();
        assert_eq!(b.text_content(), Some("x"));
    }

    #[test]
    fn errors_propagate_from_reader() {
        let err = parse_str("<r><a></b></r>", &ParseOptions::default());
        assert!(err.is_err());
    }

    #[test]
    fn missing_root_returns_error() {
        // An empty document fails earlier (XML decl alone) — use whitespace-only.
        let r = parse_str("   ", &ParseOptions::default());
        assert!(r.is_err());
    }

    // ── namespace tests ────────────────────────────────────────────────

    #[test]
    fn no_namespaces_unchanged() {
        let doc = parse_ns("<root><child/></root>");
        assert!(doc.root().namespace.get().is_none());
    }

    #[test]
    fn default_namespace_applied_to_element() {
        let doc = parse_ns(r#"<root xmlns="http://example.com/"/>"#);
        let ns = doc.root().namespace.get().unwrap();
        assert!(ns.prefix.is_none());
        assert_eq!(ns.href(), "http://example.com/");
    }

    #[test]
    fn default_namespace_not_applied_to_attr() {
        let doc = parse_ns(r#"<root xmlns="http://example.com/" id="1"/>"#);
        let id_attr = doc.root().attributes().find(|a| a.name() == "id").unwrap();
        assert!(id_attr.namespace.get().is_none(), "default ns must not apply to unprefixed attrs");
    }

    #[test]
    fn prefixed_element_resolved() {
        let doc = parse_ns(r#"<dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">X</dc:title>"#);
        let ns = doc.root().namespace.get().unwrap();
        assert_eq!(ns.prefix(), Some("dc"));
        assert_eq!(ns.href(),   "http://purl.org/dc/elements/1.1/");
    }

    #[test]
    fn prefixed_attribute_resolved() {
        let doc = parse_ns(
            r#"<root xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:nil="true"/>"#
        );
        // Match by (local-name, prefix): `name()` is the full QName on
        // the lean build but the local part under c-abi.
        let nil = doc.root().attributes()
            .find(|a| a.local_name() == "nil"
                   && a.namespace.get().and_then(|n| n.prefix()) == Some("xsi"))
            .unwrap();
        assert_eq!(nil.namespace.get().unwrap().href(),
                   "http://www.w3.org/2001/XMLSchema-instance");
    }

    #[test]
    fn xml_prefix_builtin() {
        let doc = parse_ns(r#"<root xml:lang="en"/>"#);
        let lang = doc.root().attributes()
            .find(|a| a.local_name() == "lang"
                   && a.namespace.get().and_then(|n| n.prefix()) == Some("xml"))
            .unwrap();
        assert_eq!(lang.namespace.get().unwrap().href(),
                   "http://www.w3.org/XML/1998/namespace");
    }

    #[test]
    fn nested_prefix_scope_inherits_outer() {
        let doc = parse_ns(r#"
            <root xmlns:a="http://a.com/">
                <a:child xmlns:b="http://b.com/">
                    <b:leaf/>
                </a:child>
            </root>
        "#);
        let root = doc.root();
        assert!(root.namespace.get().is_none());
        // c-abi mode stores `name` as the local part only (libxml2
        // convention); the lean build keeps the full QName.
        #[cfg(feature = "c-abi")]
        let child_local = "child";
        #[cfg(not(feature = "c-abi"))]
        let child_local = "a:child";
        let child = root.children().find(|c| c.is_element() && c.name() == child_local).unwrap();
        assert_eq!(child.namespace.get().unwrap().href(), "http://a.com/");
        let leaf = child.children().find(|c| c.is_element()).unwrap();
        assert_eq!(leaf.namespace.get().unwrap().href(), "http://b.com/");
    }

    #[test]
    fn undeclared_prefix_is_error() {
        let r = parse_str("<dc:title>X</dc:title>", &ParseOptions { namespace_aware: true, ..ParseOptions::default() });
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("undeclared") || msg.contains("prefix"), "{msg}");
    }

    #[test]
    fn default_namespace_override_in_child() {
        let doc = parse_ns(r#"
            <root xmlns="http://outer.com/">
                <inner xmlns="http://inner.com/"/>
            </root>
        "#);
        assert_eq!(doc.root().namespace.get().unwrap().href(), "http://outer.com/");
        let inner = doc.root().children().find(|c| c.is_element()).unwrap();
        assert_eq!(inner.namespace.get().unwrap().href(), "http://inner.com/");
    }

    #[test]
    fn undeclare_default_namespace() {
        let doc = parse_ns(r#"
            <root xmlns="http://example.com/">
                <child xmlns=""/>
            </root>
        "#);
        assert!(doc.root().namespace.get().is_some());
        let child = doc.root().children().find(|c| c.is_element()).unwrap();
        assert!(child.namespace.get().is_none(), "xmlns='' should clear the default namespace");
    }

    #[test]
    fn xmlns_prefix_cannot_be_declared() {
        let r = parse_str(
            r#"<r xmlns:xmlns="http://x.com/"/>"#,
            &ParseOptions { namespace_aware: true, ..ParseOptions::default() },
        );
        assert!(r.is_err());
    }

    #[test]
    fn duplicate_attribute_after_ns_expansion() {
        // Two prefixed attrs that expand to the same (ns, local) pair.
        let r = parse_str(
            r#"<r xmlns:a="http://x.com/" xmlns:b="http://x.com/" a:id="1" b:id="2"/>"#,
            &ParseOptions { namespace_aware: true, ..ParseOptions::default() },
        );
        assert!(r.is_err(), "duplicate expanded attribute name must be rejected");
    }

    #[test]
    fn deep_nesting_pops_scope_on_close() {
        // After nesting popped back to root level, an inner prefix becomes undeclared.
        let r = parse_str(r#"
            <root>
                <a xmlns:p="http://x.com/"><p:leaf/></a>
                <p:bad/>
            </root>
        "#, &ParseOptions { namespace_aware: true, ..ParseOptions::default() });
        assert!(r.is_err());
    }

    #[test]
    fn xml_decl_fields_are_captured() {
        let doc = parse(r#"<?xml version="1.1" encoding="ISO-8859-1" standalone="yes"?><r/>"#);
        assert_eq!(doc.version,    "1.1");
        assert_eq!(doc.encoding,   "ISO-8859-1");
        assert_eq!(doc.standalone, Some(true));
    }

    #[test]
    fn xml_decl_defaults_when_absent() {
        // No `<?xml … ?>` declaration → encoding stays empty
        // (matches libxml2's NULL doc->encoding) so serializers
        // omit the encoding attribute on output.
        let doc = parse("<r/>");
        assert_eq!(doc.version,    "1.0");
        assert_eq!(doc.encoding,   "");
        assert_eq!(doc.standalone, None);
    }

    #[test]
    fn xml_decl_partial_keeps_encoding_default() {
        // Only version present — encoding stays empty (no
        // declaration to copy), standalone absent.
        let doc = parse(r#"<?xml version="1.0"?><r/>"#);
        assert_eq!(doc.version,    "1.0");
        assert_eq!(doc.encoding,   "");
        assert_eq!(doc.standalone, None);
    }

    #[test]
    fn xml_decl_standalone_no_is_captured() {
        // Note: standalone without encoding is rejected by both legacy
        // and arena parsers (pre-existing behaviour) — include encoding.
        let doc = parse(r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?><r/>"#);
        assert_eq!(doc.standalone, Some(false));
    }

    #[test]
    fn many_siblings_preserve_order() {
        let mut xml = String::from("<r>");
        for i in 0..100 {
            xml.push_str(&format!("<i>{i}</i>"));
        }
        xml.push_str("</r>");
        let doc = parse(&xml);
        let texts: Vec<&str> = doc.root().children()
            .filter(|c| c.kind == NodeKind::Element)
            .map(|c| c.text_content().unwrap_or(""))
            .collect();
        assert_eq!(texts.len(), 100);
        assert_eq!(texts[0],  "0");
        assert_eq!(texts[99], "99");
    }

    // ── parse_bytes_in_place — entry point + call-time gates ──────

    #[test]
    fn in_place_parses_basic_document() {
        let buf = b"<root><child id=\"1\">hello</child></root>".to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect("in-place parse should succeed on well-formed input");
        assert_eq!(doc.root().name(), "root");
        let child = doc.root().children().next().expect("child element");
        assert_eq!(child.name(), "child");
        assert_eq!(child.attributes().next().unwrap().value(), "1");
    }

    #[test]
    fn in_place_rejects_recovery_mode_at_call_time() {
        let buf = b"<r/>".to_vec();
        let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
        let err = parse_bytes_in_place(buf, &opts)
            .expect_err("recovery_mode=true must be rejected up front");
        assert!(
            err.message.contains("recovery_mode"),
            "error should mention recovery_mode, got: {}", err.message,
        );
    }

    #[test]
    fn in_place_transcodes_non_utf8_when_auto_transcode_on() {
        // ISO-8859-1 with `<?xml encoding="ISO-8859-1"?>` and one non-ASCII
        // byte (0xE9 = 'é').  Transcoded to UTF-8 upfront, then parsed.
        let buf: Vec<u8> =
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><r>caf\xe9</r>".to_vec();
        let opts = ParseOptions { auto_transcode: true, ..ParseOptions::default() };
        let doc = parse_bytes_in_place(buf, &opts).expect("transcoded parse should succeed");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, "café");
    }

    #[test]
    fn in_place_rejects_invalid_utf8_when_auto_transcode_off() {
        let buf: Vec<u8> = b"<r>\xff</r>".to_vec();
        let opts = ParseOptions { auto_transcode: false, ..ParseOptions::default() };
        let err = parse_bytes_in_place(buf, &opts).expect_err("invalid UTF-8 must be rejected");
        assert_eq!(err.domain, crate::error::ErrorDomain::Encoding);
    }

    // ── simdutf8 ⇄ std::str::from_utf8 equivalence ───────────────
    //
    // The input-validation gates (`parse_bytes_in_place`,
    // `transcode_and_validate`, `XmlBytesReader::from_bytes`) use
    // `simdutf8::compat::from_utf8` as a drop-in for `std::str::from_utf8`.
    // The whole correctness claim is that it is *behaviorally identical*:
    // same accept/reject verdict, and on rejection the same `valid_up_to()`
    // and `error_len()` — the parser pins error line/col to `valid_up_to()`,
    // so any divergence would silently shift reported error positions.
    // These tests pin that equivalence against `std` as the oracle.

    /// Reduce a validation outcome to a comparable shape: `Ok(())` when
    /// valid, or the error's `(valid_up_to, error_len)` when not.
    fn std_verdict(b: &[u8]) -> std::result::Result<(), (usize, Option<usize>)> {
        std::str::from_utf8(b)
            .map(|_| ())
            .map_err(|e| (e.valid_up_to(), e.error_len()))
    }

    fn simd_verdict(b: &[u8]) -> std::result::Result<(), (usize, Option<usize>)> {
        simdutf8::compat::from_utf8(b)
            .map(|_| ())
            .map_err(|e| (e.valid_up_to(), e.error_len()))
    }

    #[test]
    fn simdutf8_matches_std_at_chunk_boundaries() {
        // SIMD validators process vector-width chunks (16/32/64 bytes) then a
        // scalar tail; bugs hide where a malformed byte straddles that seam.
        // Sweep a lone 0xFF (never valid UTF-8) across every offset of buffers
        // sized around the common vector widths, plus the clean buffer.
        for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129] {
            let base = vec![b'a'; len];
            assert_eq!(std_verdict(&base), simd_verdict(&base), "clean buffer len {len}");
            for pos in 0..len {
                let mut bad = base.clone();
                bad[pos] = 0xFF;
                assert_eq!(
                    std_verdict(&bad), simd_verdict(&bad),
                    "diverged: lone 0xFF at offset {pos} in len {len}",
                );
            }
        }
    }

    #[test]
    fn simdutf8_matches_std_on_adversarial_utf8() {
        // The classic failure modes for hand-rolled UTF-8 validators: truncated
        // multibyte sequences, overlong encodings, surrogate code points, lone
        // continuation bytes, and valid→invalid transitions mid-stream.
        let cases: &[&[u8]] = &[
            b"",
            b"hello",
            &[0xC3, 0xA9],                          // é — valid 2-byte
            &[0xC3],                                // truncated 2-byte lead
            &[0xE2, 0x82, 0xAC],                    // € — valid 3-byte
            &[0xE2, 0x82],                          // truncated 3-byte
            &[0xF0, 0x9F, 0x98, 0x80],              // 😀 — valid 4-byte
            &[0xF0, 0x9F, 0x98],                    // truncated 4-byte
            &[0x80],                                // lone continuation
            &[0xBF],                                // lone continuation
            &[0xC0, 0x80],                          // overlong NUL
            &[0xE0, 0x80, 0x80],                    // overlong 3-byte
            &[0xF0, 0x80, 0x80, 0x80],              // overlong 4-byte
            &[0xED, 0xA0, 0x80],                    // lone high surrogate U+D800
            &[0xED, 0xBF, 0xBF],                    // lone low surrogate U+DFFF
            &[0xF4, 0x90, 0x80, 0x80],              // above U+10FFFF
            &[0xFF],
            &[0xFE],
            b"caf\xe9",                             // Latin-1 é — invalid as UTF-8
            &[b'o', b'k', 0xC3, 0xA9, 0xFF, b'x'],  // valid run then invalid byte
        ];
        for c in cases {
            assert_eq!(std_verdict(c), simd_verdict(c), "diverged on {c:02x?}");
        }
    }

    #[test]
    fn simdutf8_matches_std_on_random_bytes() {
        // Deterministic xorshift corpus — no rng/clock dependency, so failures
        // reproduce exactly.  Bias one byte in four into the 0x80..=0xFF range
        // so lead/continuation logic gets exercised, not just ASCII runs.
        let mut state = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 70) as usize;
            let mut buf = Vec::with_capacity(len);
            for _ in 0..len {
                let r = next();
                let byte = if r & 3 == 0 {
                    (r >> 8) as u8 | 0x80
                } else {
                    (r >> 8) as u8 & 0x7F
                };
                buf.push(byte);
            }
            assert_eq!(std_verdict(&buf), simd_verdict(&buf), "diverged on {buf:02x?}");
        }
    }

    #[test]
    fn in_place_decodes_builtin_entities() {
        // Slow-path exercise: text content with `&amp;` triggers entity
        // expansion in the reader.  In-place mode mutates the source
        // buffer (overwriting `&amp;` with `&`) and emits the text as
        // Cow::Borrowed pointing into the now-mutated buffer.  The arena
        // parser then takes the alloc_str_borrow zero-copy path.
        let buf = b"<r>tom &amp; jerry</r>".to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default()).expect("parse");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, "tom & jerry");
    }

    #[test]
    fn in_place_decodes_multiple_builtin_entities() {
        let buf = b"<r>&lt;hello&gt; &amp; &quot;hi&quot;</r>".to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default()).expect("parse");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, r#"<hello> & "hi""#);
    }

    #[test]
    fn in_place_text_without_entities_works() {
        // Fast path (Cow::Borrowed straight from reader, no slow path).
        let buf = b"<r>plain text with no entities</r>".to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default()).expect("parse");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, "plain text with no entities");
    }

    #[test]
    fn in_place_accepts_user_entity_that_shrinks() {
        // `&x;` (4 source bytes) expands to "AB" (2 bytes) — fits.
        let buf = br#"<!DOCTYPE r [<!ENTITY x "AB">]><r>&x;</r>"#.to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default()).expect("parse");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, "AB");
    }

    #[test]
    fn in_place_rejects_user_entity_that_grows() {
        // `&hi;` (5 source bytes) tries to expand to "Hello, World!" (13 bytes).
        // Doesn't fit → error at use site with a clear message.
        let buf = br#"<!DOCTYPE r [<!ENTITY hi "Hello, World!">]><r>&hi;</r>"#.to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default()).expect_err(
            "expansion bigger than reference must be rejected in in-place mode",
        );
        assert!(
            err.message.contains("exceeds source span")
                || err.message.contains("expansion"),
            "error should explain the expansion mismatch, got: {}", err.message,
        );
    }

    #[test]
    fn in_place_numeric_char_refs_work() {
        // `&#xC9;` (6 source bytes) → `É` (2 UTF-8 bytes) — fits.
        // `&#x1D400;` (9 source bytes) → `𝐀` (4 UTF-8 bytes) — fits.
        let buf = "<r>caf&#xC9; and &#x1D400;</r>".as_bytes().to_vec();
        let doc = parse_bytes_in_place(buf, &ParseOptions::default()).expect("parse");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text, "cafÉ and 𝐀");
    }

    #[test]
    fn in_place_rejects_recursive_entity() {
        // Direct recursion — same well-formedness error as parse_bytes
        // (XML 1.0 § 4.1).  The expansion stack catches it before any
        // in-place mutation happens.
        let buf = br#"<!DOCTYPE r [<!ENTITY a "&a;">]><r>&a;</r>"#.to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect_err("recursive entity must be rejected");
        let msg = err.message.to_lowercase();
        assert!(
            msg.contains("recurs") || msg.contains("cycle"),
            "error should mention recursion/cycle, got: {}", err.message,
        );
    }

    // ── parse_bytes_in_place — flag honoring ────────────────────────────────
    //
    // These tests pin down a behavioral contract: `parse_bytes_in_place`
    // honors every flag on the caller's `ParseOptions` as-is.  It does
    // NOT silently flip the `skip_*` validation flags on (that override
    // used to exist; was removed deliberately so callers control the
    // validation / speed tradeoff).  Each test pair below verifies one
    // flag:
    //   - default `ParseOptions` (flag is `false`) → input rejected
    //   - the same input with the flag set `true`  → input accepted
    // If either half of a pair flips, something has reintroduced the
    // override or weakened the validator.

    /// XML 1.0 § 2.3 [4]: names start with a letter, `_`, or `:` (or
    /// non-ASCII NameStartChar) — never a digit.  `<1foo>` is invalid.
    #[test]
    fn in_place_default_opts_reject_bad_name_start() {
        let buf = b"<1foo/>".to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect_err("default opts must reject name starting with digit");
        assert!(
            err.message.contains("name-start") || err.message.contains("name start"),
            "error should mention name-start, got: {}", err.message,
        );
        // libxml2-compatible code: XML_ERR_NAME_REQUIRED = 68.
        // Validates the Slice 5a contract that consumer-checked codes
        // round-trip via `err.code as i32`.
        assert_eq!(err.code, crate::error::ErrorCode::NameRequired);
        assert_eq!(err.code as i32, 68);
    }

    #[test]
    fn in_place_skip_name_validation_accepts_bad_name_start() {
        // Same malformed name as above; with skip_name_validation the
        // name is accepted as-is and parsing reaches Eof cleanly.
        let buf = b"<1foo/>".to_vec();
        let opts = ParseOptions { skip_name_validation: true, ..ParseOptions::default() };
        let doc = parse_bytes_in_place(buf, &opts)
            .expect("skip_name_validation=true must accept malformed name");
        assert_eq!(doc.root().name(), "1foo");
    }

    /// XML 1.0 § 3.1 [STag] WFC "Unique Att Spec": no element may have
    /// two attributes with the same name.  `<r a="1" a="2"/>` is
    /// rejected when `skip_attr_validation` is false (the default).
    #[test]
    fn in_place_default_opts_reject_duplicate_attribute() {
        let buf = br#"<r a="1" a="2"/>"#.to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect_err("default opts must reject duplicate attribute");
        assert!(
            err.message.to_lowercase().contains("duplicate") || err.message.contains("Unique Att Spec"),
            "error should mention duplicate-attribute, got: {}", err.message,
        );
    }

    #[test]
    fn in_place_skip_attr_validation_accepts_duplicate_attribute() {
        let buf = br#"<r a="1" a="2"/>"#.to_vec();
        let opts = ParseOptions { skip_attr_validation: true, ..ParseOptions::default() };
        let doc = parse_bytes_in_place(buf, &opts)
            .expect("skip_attr_validation=true must accept duplicate attr");
        // Both attributes end up on the element when validation is off —
        // we don't dedup at the structural layer.
        let n_attrs = doc.root().attributes().count();
        assert_eq!(n_attrs, 2, "both duplicate attrs should be present");
    }

    /// XML 1.0 § 3.1 [STag/ETag]: end tag name must match the start
    /// tag name.  `<a></b>` is a well-formedness error.
    #[test]
    fn in_place_default_opts_reject_end_tag_mismatch() {
        let buf = b"<a></b>".to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect_err("default opts must reject mismatched end tag");
        let msg = err.message.to_lowercase();
        assert!(
            msg.contains("mismatched") || msg.contains("expected"),
            "error should mention mismatched end tag, got: {}", err.message,
        );
    }

    #[test]
    fn in_place_skip_end_tag_check_accepts_end_tag_mismatch() {
        let buf = b"<a></b>".to_vec();
        let opts = ParseOptions { skip_end_tag_check: true, ..ParseOptions::default() };
        let doc = parse_bytes_in_place(buf, &opts)
            .expect("skip_end_tag_check=true must accept mismatched end tag");
        assert_eq!(doc.root().name(), "a");
    }

    /// XML 1.0 § 2.2 [2]: only specific Unicode code points are valid
    /// XML characters.  0x01 (control character, not in the allowed
    /// set) is rejected when `skip_xml_char_validation` is false.
    #[test]
    fn in_place_default_opts_reject_invalid_xml_char() {
        // Text content containing 0x01 — valid UTF-8 (single ASCII byte)
        // but invalid as an XML 1.0 character.
        let buf = b"<r>hello\x01world</r>".to_vec();
        let err = parse_bytes_in_place(buf, &ParseOptions::default())
            .expect_err("default opts must reject invalid XML char");
        // Validator's actual wording — check for either the section
        // citation or any mention of "character" / "valid".
        let msg = err.message.to_lowercase();
        assert!(
            msg.contains("xml 1.0") || msg.contains("char") || msg.contains("invalid"),
            "error should mention invalid XML char, got: {}", err.message,
        );
    }

    #[test]
    fn in_place_skip_xml_char_validation_accepts_invalid_xml_char() {
        let buf = b"<r>hello\x01world</r>".to_vec();
        let opts = ParseOptions { skip_xml_char_validation: true, ..ParseOptions::default() };
        let doc = parse_bytes_in_place(buf, &opts)
            .expect("skip_xml_char_validation=true must accept 0x01 in text");
        let text = doc.root().children().find_map(|n| n.text_content()).unwrap_or("");
        assert_eq!(text.as_bytes(), b"hello\x01world");
    }

    /// Combined "fast path" — all four skips enabled.  Exercises the
    /// path callers actually reach for when they want maximum speed.
    /// Verifies it works on a well-formed doc (the more interesting
    /// cases are covered individually above; this is a smoke test).
    #[test]
    fn in_place_with_all_skips_parses_well_formed_doc() {
        let buf = b"<root><a id=\"1\">hello</a></root>".to_vec();
        let opts = ParseOptions {
            skip_xml_char_validation: true,
            skip_name_validation:     true,
            skip_attr_validation:     true,
            skip_end_tag_check:       true,
            ..ParseOptions::default()
        };
        let doc = parse_bytes_in_place(buf, &opts).expect("all-skips parse");
        assert_eq!(doc.root().name(), "root");
        let a = doc.root().children().next().expect("element child");
        assert_eq!(a.name(), "a");
        assert_eq!(a.attributes().next().unwrap().value(), "1");
    }
}
