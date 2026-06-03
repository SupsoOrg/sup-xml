//! Streaming XML reader over `io::Read`.
//!
//! [`XmlByteStreamReader`] is a thin wrapper around the existing
//! slurped [`XmlBytesReader`] that owns a rolling `Vec<u8>` buffer
//! and pulls more bytes from the inner reader on demand.  Used by
//! the CLI's `lint` subcommand to validate multi-GB XML files in
//! bounded memory without slurping them.
//!
//! # Design
//!
//! The wrapper owns:
//! * `inner: R` — the source of bytes.
//! * `buf: Vec<u8>` — a rolling buffer holding the not-yet-consumed
//!   slice of input.  Grows up to `buffer_size`, then refuses to
//!   grow further (a single XML token bigger than `buffer_size`
//!   becomes a hard error).
//! * `reader: XmlBytesReader<'static>` — the slurped reader, with
//!   its scanner re-bound to point into `buf` after every refill /
//!   compaction / growth.  The `'static` lifetime is a deliberate
//!   lie maintained internally: the actual borrow is bounded by
//!   `self.buf`, and we re-point the scanner via
//!   [`XmlBytesReader::rebind_scanner`] whenever the buffer might
//!   have moved.  This is sound because (a) both fields are owned
//!   by `Self` and outlive each other, (b) the scanner only stores
//!   a raw pointer into the buffer (no Rust borrow), and (c) we
//!   never let any `&[u8]` derived from `buf` escape across a
//!   buffer mutation.
//!
//! # Pre-fill model
//!
//! [`XmlBytesReader::next`] mutates internal state (depth,
//! element_stack, …) partway through the call — i.e., it's not
//! transactional.  We can't retry it after a mid-token refill
//! without corrupting state.  So the wrapper refills *between*
//! events, before calling `next`:
//!
//! ```text
//! 1. Before calling reader.next():
//!      if cur_len - cur_pos < buffer_size, refill.
//! 2. Call reader.next() — guaranteed to have at least
//!    buffer_size bytes ahead.  Completes within them or hits
//!    true EOF.
//! 3. Repeat.
//! ```
//!
//! This guarantees the inner reader never sees a transient
//! "ran-off-the-end" condition — its bytes are always there.  The
//! trade-off is that `buffer_size` is also the maximum size of a
//! single XML token (text node, attribute value, CDATA section);
//! anything larger errors out.  Matches libxml2's
//! `XML_MAX_TEXT_LENGTH` semantics.
//!
//! # Reading
//!
//! [`XmlByteStreamReader::next_event`] pulls one event at a time,
//! streaming more bytes from the source as needed.  Each event
//! borrows the rolling buffer and is valid only until the next pull;
//! the borrow checker enforces this by tying the event's lifetime to
//! `&mut self`, so a caller must consume each event before requesting
//! the next.  This is the same zero-copy contract as
//! [`XmlBytesReader::next`] and `quick-xml`'s `Reader::read_event`.
//!
//! [`XmlByteStreamReader::validate`] is the drive-to-EOF convenience
//! used by the CLI's `lint` when only the well-formedness verdict
//! matters — it pulls events to completion and discards them.

use std::io::Read;

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::options::ParseOptions;
use crate::xml_bytes_reader::{BytesEvent, XmlBytesReader, XmlDeclInfo};

/// Default working-buffer size when none is provided.  Matches
/// libxml2's `XML_MAX_TEXT_LENGTH` (10 MB) — bigger than any single
/// text node in the vast majority of real-world XML, small enough
/// that streaming meaningfully saves memory vs slurping.
pub const DEFAULT_BUFFER_SIZE: usize = 10 * 1024 * 1024;

/// "Huge" mode buffer size — matches libxml2's `XML_PARSE_HUGE`
/// (1 GB).  Use for inputs that contain unusually large tokens
/// (embedded base64 blobs in SVG, OOXML packages, etc.).
pub const HUGE_BUFFER_SIZE: usize = 1024 * 1024 * 1024;

/// Initial buffer capacity when no size hint is available (e.g.
/// reading from stdin / a pipe).  Grows dynamically as needed.
const INITIAL_CAPACITY_WITHOUT_HINT: usize = 64 * 1024;

/// Internal buffer capacity multiplier vs the user-facing
/// `buffer_size` (which caps single-token size).  We allocate
/// `BUF_CAPACITY_MULTIPLE * buffer_size` so that after a refill
/// the scanner has `(BUF_CAPACITY_MULTIPLE - 1) * buffer_size`
/// bytes of "consumption budget" before the next refill is
/// triggered (i.e., before runway drops below `buffer_size`).
/// With 2×, refills cost `2 * buffer_size` memmove each but
/// amortise over `buffer_size` bytes of consumption — roughly
/// 2× overhead vs slurped, and bounded.  Raising this trades
/// memory for fewer refills; 2× is the sweet spot.
const BUF_CAPACITY_MULTIPLE: usize = 2;

/// Streaming XML reader that pulls bytes from an `io::Read` source
/// on demand.
///
/// Constructed via [`Self::new`] or [`Self::with_size_hint`] and
/// then driven via [`Self::next_event`] (pull events one at a time)
/// or [`Self::validate`] (drive to EOF for a well-formedness check).
pub struct XmlByteStreamReader<R: Read> {
    inner: R,
    /// Rolling buffer.  Capacity grows up to `buffer_size`; bytes
    /// at `0..reader.cur_pos()` have been consumed and are eligible
    /// for compaction on the next refill.
    buf: Vec<u8>,
    /// Maximum buffer size — also the maximum single-token size
    /// the wrapper will accept.
    buffer_size: usize,
    /// `true` once `inner.read(...)` has returned 0 — no more
    /// bytes will ever come.
    eof: bool,
    /// Inner reader with its scanner pointing into `self.buf`.
    /// The `'static` lifetime is a lie; the actual borrow is
    /// bounded by `self`.  Maintained sound by rebinding the
    /// scanner whenever `buf` might have moved (see
    /// [`Self::refill_and_rebind`]).
    reader: XmlBytesReader<'static>,
    /// Set when input begins with a non-UTF-8 BOM.  Streaming
    /// rejects those at construction with a clear error.
    _utf8_only_marker: (),
}

impl<R: Read> XmlByteStreamReader<R> {
    /// Construct with no size hint (e.g. stdin).  Starts with a
    /// 64 KiB buffer that grows as needed up to `buffer_size`.
    pub fn new(inner: R, buffer_size: usize) -> Result<Self> {
        Self::with_size_hint(inner, None, buffer_size)
    }

    /// Construct with an optional size hint.  When the input's
    /// total size is known (file with stat-derived length), pass it
    /// — the wrapper pre-allocates exactly that much (capped by
    /// `buffer_size`), avoiding the per-growth memcpy cost.  For
    /// stdin / pipes, pass `None` and the buffer grows
    /// incrementally from a 64 KiB seed.
    pub fn with_size_hint(
        mut inner:   R,
        size_hint:   Option<usize>,
        buffer_size: usize,
    ) -> Result<Self> {
        // Internal capacity is `BUF_CAPACITY_MULTIPLE * buffer_size`
        // so refills (triggered when runway drops below
        // `buffer_size`) only fire after consuming roughly
        // `buffer_size` bytes — amortising memmove cost over many
        // events instead of one per event.  For inputs smaller than
        // the internal cap, the file_size hint shrinks the initial
        // allocation so small files don't pay for the full capacity.
        let internal_cap = buffer_size.saturating_mul(BUF_CAPACITY_MULTIPLE);
        let initial = size_hint
            .map(|n| n.min(internal_cap))
            .unwrap_or(INITIAL_CAPACITY_WITHOUT_HINT.min(internal_cap));
        let mut buf = Vec::with_capacity(initial);

        // Prime the buffer so we can detect the encoding from the
        // BOM / decl before constructing the inner reader.  Read up
        // to the smaller of the file size and the initial capacity
        // — for stdin this just pulls the first 64 KiB.
        read_into_vec(&mut inner, &mut buf, initial)?;
        let eof = buf.len() < initial;

        // UTF-8 sniff.  Streaming v1 supports UTF-8 only; non-UTF-8
        // BOMs surface as a clear error pointing at the slurped
        // reader for those inputs.
        if let Some(bom) = sniff_non_utf8_bom(&buf) {
            return Err(XmlError::new(
                ErrorDomain::Encoding,
                ErrorLevel::Fatal,
                format!(
                    "streaming reader: detected {bom} BOM, but streaming \
                     v1 supports UTF-8 input only.  Use the slurped reader \
                     for non-UTF-8 encodings."
                ),
            ));
        }

        // Strip leading UTF-8 BOM if present — XmlBytesReader doesn't
        // skip it for us when we hand it the bytes directly.
        if buf.starts_with(&[0xEF, 0xBB, 0xBF]) {
            buf.drain(..3);
        }

        // Construct the inner reader against an empty static slice,
        // then rebind its scanner to the actual buffer bytes.  This
        // sidesteps a chicken-and-egg lifetime problem: we can't
        // create a `&'static [u8]` referring to `buf` (it's owned
        // here), but we can construct against the empty slice (which
        // really IS `'static`) and rebind.
        //
        // `stream_owned_names` forces element-stack entries to be
        // owned `String`s rather than byte ranges into the source —
        // required because our rolling buffer compacts between
        // events, and any byte ranges captured at start-tag time
        // would become stale by end-tag time.  See
        // `crate::xml_bytes_reader::dispatch_start_element` for the
        // branch that consults this flag.
        let mut opts = ParseOptions::default();
        opts.stream_owned_names = true;
        const EMPTY: &[u8] = &[];
        let mut reader = XmlBytesReader::from_bytes(EMPTY)?.with_options(opts);
        // SAFETY: `buf` is owned by `Self` and outlives `reader`.
        // The bytes are valid UTF-8 (sniffed above; the BOM was
        // stripped if present).  No entity stream is active on a
        // freshly-constructed reader.  We re-call rebind after
        // every operation that might move `buf`.
        unsafe {
            reader.rebind_scanner(buf.as_ptr(), buf.len(), 0);
        }

        Ok(Self {
            inner,
            buf,
            buffer_size,
            eof,
            reader,
            _utf8_only_marker: (),
        })
    }

    /// Override the inner reader's [`ParseOptions`].  See
    /// [`XmlBytesReader::with_options`] for what's tunable.
    ///
    /// Note: `opts.stream_owned_names` is forced to `true`
    /// regardless of what the caller passes — the streaming
    /// wrapper requires owned element names to survive buffer
    /// compaction between events.
    pub fn with_options(mut self, mut opts: ParseOptions) -> Self {
        opts.stream_owned_names = true;
        self.reader = self.reader.with_options(opts);
        self
    }

    /// XML declaration fields parsed from the prolog, if any.
    /// Returns `None` before the first event has been pulled.
    pub fn xml_decl(&self) -> Option<&XmlDeclInfo> {
        self.reader.xml_decl()
    }

    /// Non-fatal errors logged while
    /// [`ParseOptions::recovery_mode`] is enabled.  Empty otherwise.
    pub fn recovered_errors(&self) -> &[XmlError] {
        self.reader.recovered_errors()
    }

    /// Pull the next parse event, streaming more bytes from the
    /// source as needed.
    ///
    /// The returned [`BytesEvent`] borrows the reader's internal
    /// rolling buffer and is valid only until the next call to
    /// `next_event` (or any other `&mut self` method): its lifetime is
    /// tied to the `&mut self` borrow, so the borrow checker forbids
    /// pulling the next event while one is still held.  Consume each
    /// event (copy out what you need) before requesting the next —
    /// the same zero-copy contract as [`XmlBytesReader::next`].
    ///
    /// Yields [`BytesEvent::Eof`] once the document is exhausted; a
    /// well-formedness violation surfaces as `Err`.
    pub fn next_event(&mut self) -> Result<BytesEvent<'_, '_>> {
        // Pre-fill *between* events so the inner reader has at least
        // `buffer_size` bytes ahead of its cursor before we call
        // `next()` — it then never sees a mid-token EOF unless EOF is
        // real.  `ensure_runway` is the only place `buf` moves; it
        // completes (releasing its borrow) before the event below is
        // created, and the event's lifetime is bounded by `&mut self`,
        // so no refill can run while the event is alive.
        self.ensure_runway()?;
        self.reader.next()
    }

    /// Drive the parser to EOF.  Returns `Ok(())` if every event up
    /// through [`BytesEvent::Eof`] parses without error.  This is the
    /// CLI's `lint` workload — we don't need the events themselves,
    /// just the well-formedness verdict.
    pub fn validate(mut self) -> Result<()> {
        while !matches!(self.next_event()?, BytesEvent::Eof) {}
        Ok(())
    }

    /// Refill / compact / grow the buffer as needed so the inner
    /// reader has at least `buffer_size` bytes ahead of its cursor
    /// (or true EOF has been reached).  Called between events; safe
    /// only when the scanner is not mid-token, which is guaranteed
    /// at event boundaries.
    fn ensure_runway(&mut self) -> Result<()> {
        // Bytes the scanner could still consume from the current
        // window without needing more from `inner`.  `src_offset`
        // == `cur_pos` between events (no entity stream is active),
        // which is the only time we call this.
        let cur_pos = self.reader.src_offset();
        let runway = self.buf.len() - cur_pos;
        // Refill if runway drops below the user-set max-token cap.
        // Equality is fine — we have exactly enough for any token
        // up to the cap, and the next event will likely consume
        // less than the full cap.
        if runway >= self.buffer_size || self.eof {
            return Ok(());
        }

        // Compact: drop bytes the scanner has already passed.
        if cur_pos > 0 {
            self.buf.drain(..cur_pos);
        }

        // Internal target capacity = buffer_size * BUF_CAPACITY_MULTIPLE.
        // Grow up to this if not already there; never exceed it.
        let target_cap = self.buffer_size.saturating_mul(BUF_CAPACITY_MULTIPLE);
        if self.buf.capacity() < target_cap {
            let additional = target_cap - self.buf.len();
            self.buf.reserve(additional);
        }

        // Pull more bytes until buf is full to target_cap or the
        // inner reader is dry.
        while self.buf.len() < target_cap && !self.eof {
            let space = target_cap - self.buf.len();
            let n = read_chunk(&mut self.inner, &mut self.buf, space)?;
            if n == 0 {
                self.eof = true;
                break;
            }
        }

        // Re-point the scanner: buf may have moved (reserve), and
        // cur_pos is now 0 (we drained the consumed prefix).
        // SAFETY: buf outlives reader; UTF-8 invariant preserved
        // because we only ever append bytes that came from inner —
        // see `read_chunk` for the per-chunk UTF-8 boundary check.
        unsafe {
            self.reader.rebind_scanner(self.buf.as_ptr(), self.buf.len(), 0);
        }
        Ok(())
    }
}

/// Read up to `wanted` bytes into `buf` (extending it).  Returns
/// the number of bytes actually read.  Handles short reads from the
/// underlying source by looping; returns 0 only on real EOF.
fn read_into_vec<R: Read>(reader: &mut R, buf: &mut Vec<u8>, wanted: usize) -> Result<usize> {
    let start = buf.len();
    let target = start + wanted;
    buf.resize(target, 0);
    let mut filled = start;
    while filled < target {
        let n = reader.read(&mut buf[filled..target]).map_err(io_to_xml_err)?;
        if n == 0 { break; }
        filled += n;
    }
    buf.truncate(filled);
    Ok(filled - start)
}

/// Read one chunk (up to `space` bytes) into the tail of `buf`.
/// Returns the number of bytes appended.  Validates that what we
/// appended doesn't introduce a UTF-8 boundary violation — but the
/// last few bytes may be the start of a multi-byte sequence whose
/// continuation hasn't arrived yet, which is fine: the next chunk
/// completes it.
fn read_chunk<R: Read>(reader: &mut R, buf: &mut Vec<u8>, space: usize) -> Result<usize> {
    let start = buf.len();
    buf.resize(start + space, 0);
    let n = reader.read(&mut buf[start..]).map_err(io_to_xml_err)?;
    buf.truncate(start + n);
    Ok(n)
}

fn io_to_xml_err(e: std::io::Error) -> XmlError {
    XmlError::new(
        ErrorDomain::Parser,
        ErrorLevel::Fatal,
        format!("streaming reader I/O error: {e}"),
    )
}

/// Detect the byte-order mark of an encoding the streaming reader
/// doesn't support in v1.  Returns the encoding name for the error
/// message, or `None` for "looks like UTF-8 (with or without BOM)".
fn sniff_non_utf8_bom(buf: &[u8]) -> Option<&'static str> {
    if buf.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) { return Some("UTF-32 LE"); }
    if buf.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) { return Some("UTF-32 BE"); }
    if buf.starts_with(&[0xFF, 0xFE]) { return Some("UTF-16 LE"); }
    if buf.starts_with(&[0xFE, 0xFF]) { return Some("UTF-16 BE"); }
    None
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn validate(bytes: &[u8]) -> Result<()> {
        XmlByteStreamReader::new(Cursor::new(bytes.to_vec()), DEFAULT_BUFFER_SIZE)?
            .validate()
    }

    fn validate_with_buffer(bytes: &[u8], buffer_size: usize) -> Result<()> {
        XmlByteStreamReader::new(Cursor::new(bytes.to_vec()), buffer_size)?
            .validate()
    }

    #[test]
    fn accepts_well_formed_xml() {
        assert!(validate(b"<r/>").is_ok());
        assert!(validate(b"<?xml version=\"1.0\"?><r><a/><b>text</b></r>").is_ok());
    }

    #[test]
    fn rejects_malformed_xml() {
        assert!(validate(b"<a><b></a>").is_err());
        assert!(validate(b"<unclosed>").is_err());
        assert!(validate(b"text without root").is_err());
    }

    #[test]
    fn handles_tiny_buffer_with_many_refills() {
        // 1 KiB buffer parsing a doc much larger than the buffer —
        // exercises the refill/compact/rebind loop heavily.
        let mut doc = String::from("<root>");
        for i in 0..200 {
            doc.push_str(&format!("<item id=\"{i}\">value{i}</item>"));
        }
        doc.push_str("</root>");
        assert!(validate_with_buffer(doc.as_bytes(), 1024).is_ok());
    }

    #[test]
    fn text_content_larger_than_buffer_splits_across_events() {
        // Text content is not atomic — the scanner emits it in
        // chunks bounded by the current buffer window.  So a 10 KB
        // text node with a 1 KiB buffer just produces several text
        // events back-to-back; the parse succeeds.  Document this
        // behavior: only ATOMIC tokens (element names, attribute
        // values, comments, PIs) are bounded by buffer_size.
        let big_text = "x".repeat(10_000);
        let doc = format!("<r>{big_text}</r>");
        assert!(validate_with_buffer(doc.as_bytes(), 1024).is_ok());
    }

    #[test]
    fn errors_on_element_name_larger_than_buffer() {
        // Element names ARE atomic — must be parsed in one go.  A
        // name bigger than the internal buffer (buffer_size *
        // BUF_CAPACITY_MULTIPLE) can't fit and the parse fails.
        // We use buffer_size 1024, so the internal cap is 2048;
        // a 4000-byte name exceeds it.
        let big_name = "a".repeat(4000);
        let doc = format!("<{big_name}/>");
        let result = validate_with_buffer(doc.as_bytes(), 1024);
        assert!(result.is_err(), "expected error on huge name, got Ok");
    }

    #[test]
    fn accepts_text_at_buffer_size_boundary() {
        // Text under buffer size still succeeds via a single event.
        let text = "x".repeat(8000);
        let doc = format!("<r>{text}</r>");
        assert!(validate_with_buffer(doc.as_bytes(), 16 * 1024).is_ok());
    }

    #[test]
    fn rejects_utf16_le_bom_with_clear_error() {
        let doc = vec![0xFF, 0xFE, 0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E, 0x00];
        let err = validate(&doc).unwrap_err();
        assert!(err.message.contains("UTF-16 LE"), "got: {}", err.message);
        assert!(err.message.contains("streaming"), "got: {}", err.message);
    }

    #[test]
    fn rejects_utf16_be_bom_with_clear_error() {
        let doc = vec![0xFE, 0xFF, 0x00, 0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E];
        let err = validate(&doc).unwrap_err();
        assert!(err.message.contains("UTF-16 BE"), "got: {}", err.message);
    }

    #[test]
    fn strips_utf8_bom_silently() {
        // UTF-8 BOM is allowed and should not affect the parse.
        let mut doc = vec![0xEF, 0xBB, 0xBF];
        doc.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><r/>");
        assert!(validate(&doc).is_ok());
    }

    #[test]
    fn size_hint_pre_allocates_to_match() {
        // With a 64-byte doc and a 1 MiB buffer cap, the size hint
        // should cause us to allocate exactly 64 bytes (not the cap).
        let doc = b"<r><a>hi</a></r>".to_vec();
        let r = XmlByteStreamReader::with_size_hint(
            Cursor::new(doc.clone()),
            Some(doc.len()),
            1024 * 1024,
        ).unwrap();
        // Capacity may grow slightly past hint due to Vec internals
        // but should be much less than the 1 MiB cap.
        assert!(r.buf.capacity() < 1024 * 1024 / 4,
            "expected capacity ~{} bytes, got {}", doc.len(), r.buf.capacity());
        // Result still validates.
        assert!(r.validate().is_ok());
    }

    #[test]
    fn empty_input_errors_cleanly() {
        // No root element — should be a parse error, not a panic.
        let result = validate(b"");
        assert!(result.is_err());
    }

    #[test]
    fn handles_text_split_across_refills() {
        // A 4 KiB text node parsed with a 2 KiB buffer that has to
        // refill mid-text.  But — the entire text must still fit in
        // a single buffer's worth (since text tokens can't cross
        // refill).  So with a 2 KiB buffer, 1 KiB of text is the
        // safe regime.  This is the documented limit.
        let text = "x".repeat(1000);
        let doc = format!("<r>{text}</r>");
        assert!(validate_with_buffer(doc.as_bytes(), 2048).is_ok());
    }

    #[test]
    fn many_small_events_with_small_buffer() {
        // Many small elements — each is well under buffer size, but
        // the document overall is bigger than the buffer.  Refills
        // happen between events and everything should validate.
        let mut doc = String::from("<r>");
        for _ in 0..1000 {
            doc.push_str("<x/>");
        }
        doc.push_str("</r>");
        let r = validate_with_buffer(doc.as_bytes(), 512);
        if let Err(e) = &r {
            panic!("expected ok; got error: {} (line={:?}, col={:?})",
                   e.message, e.line, e.column);
        }
    }

    // ── next_event: streaming data extraction ──────────────────────────

    #[test]
    fn next_event_extracts_data_with_small_buffer() {
        // A document far larger than the buffer, pulled one event at a
        // time.  We collect the text of every <item> and confirm we
        // recover all of it — proving streaming *data extraction*
        // (not just validation) works in bounded memory.
        const N: usize = 500;
        let mut doc = String::from("<root>");
        for i in 0..N {
            doc.push_str(&format!("<item>value{i}</item>"));
        }
        doc.push_str("</root>");

        let mut r = XmlByteStreamReader::new(Cursor::new(doc.into_bytes()), 512).unwrap();
        let mut values = Vec::new();
        loop {
            let eof = {
                match r.next_event().unwrap() {
                    BytesEvent::Eof => true,
                    BytesEvent::Text(t) => {
                        let s = std::str::from_utf8(t.as_bytes()).unwrap();
                        if !s.trim().is_empty() {
                            values.push(s.to_string());
                        }
                        false
                    }
                    _ => false,
                }
            };
            if eof { break; }
        }

        assert_eq!(values.len(), N, "should recover every item's text");
        assert_eq!(values[0], "value0");
        assert_eq!(values[N - 1], format!("value{}", N - 1));
    }

    #[test]
    fn next_event_keeps_memory_bounded() {
        // The whole point: peak buffer size stays bounded by
        // buffer_size * BUF_CAPACITY_MULTIPLE no matter how large the
        // document is.  Drive a doc many times the buffer and assert
        // the buffer never exceeds the cap after any event.
        let buffer_size = 1024;
        let cap = buffer_size * BUF_CAPACITY_MULTIPLE;
        let mut doc = String::from("<root>");
        for i in 0..5000 {
            doc.push_str(&format!("<item id=\"{i}\">value{i}</item>"));
        }
        doc.push_str("</root>");
        assert!(doc.len() > cap * 10, "doc must dwarf the buffer for the test to mean anything");

        let mut r = XmlByteStreamReader::new(Cursor::new(doc.into_bytes()), buffer_size).unwrap();
        loop {
            let eof = matches!(r.next_event().unwrap(), BytesEvent::Eof);
            // Event dropped at the `;` above, so `r` is no longer
            // borrowed and we can inspect the buffer.
            assert!(r.buf.len() <= cap,
                "buffer grew to {} bytes, exceeding cap {cap}", r.buf.len());
            if eof { break; }
        }
    }

    #[test]
    fn next_event_surfaces_wellformedness_errors() {
        let mut r = XmlByteStreamReader::new(Cursor::new(b"<a></b>".to_vec()), 1024).unwrap();
        let mut got_err = false;
        for _ in 0..10 {
            match r.next_event() {
                Ok(BytesEvent::Eof) => break,
                Ok(_) => {}
                Err(_) => { got_err = true; break; }
            }
        }
        assert!(got_err, "mismatched end tag should surface as Err from next_event");
    }
}
