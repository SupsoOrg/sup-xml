//! Byte cursor over an XML source slice, with an entity-stream stack.
//!
//! # Stream-stack model
//!
//! Real XML parsers can't treat entity references as plain text substitutions
//! because the spec says markup inside an entity replacement must be parsed in
//! the context where the reference appears (XML 1.0 § 4.4.2).  This Scanner
//! supports that by maintaining a stack of input sources:
//!
//! * The **bottom** stream is always the original document bytes.
//! * Each general-entity reference (`&user_entity;`) **pushes** a new
//!   stream containing the replacement text.
//! * When a stream is exhausted, it's **popped** and parsing resumes in the
//!   stream below.
//!
//! While only one stream exists, the Scanner behaves identically to a
//! traditional single-buffer cursor.  Multi-stream behaviour kicks in once an
//! entity is expanded.
//!
//! # Hot-path layout
//!
//! The vast majority of XML documents contain no entity references, so the
//! Scanner caches the *currently active* `(ptr, len, pos)` view as plain
//! fields on the struct.  Every cursor accessor — `cur_bytes`, `cur_pos`,
//! `cur_tail`, `peek`, `advance`, … — is a direct field read with no
//! branch and no heap indirection at all.  The [`InputStream`] frames Vec
//! is only touched during `push_entity_stream` / `try_pop_entity_stream`,
//! which is where the live `cur_*` view is recomputed from the new top of
//! stack (the original source when the stack is empty, or the top entity
//! frame otherwise).
//!
//! `cur_ptr` is a raw `*const u8` because the lifetime of what it points
//! at depends on what's currently active: `'src` for the original source,
//! or the lifetime of an entity frame's owned `Vec<u8>` when an entity is
//! on top.  Rust's safe borrow checker can't express "either-of-these,"
//! so we keep the active buffer description as raw pointer + length and
//! recompute it whenever push/pop changes the top of stack.  Soundness
//! relies on every accessor's slice/borrow having lifetime tied to
//! `&self` (the frame can only be removed via `&mut self`, so an active
//! shared borrow blocks pop).
//!
//! [`Parser`]: crate::parser
//! [`XmlReader`]: crate::reader

use std::borrow::Cow;
use std::marker::PhantomData;

use memchr::{memchr_iter, memrchr};

use crate::charsets::{
    ASCII_XML_NAME as ASCII_NAME, ASCII_XML_NAME_CHAR, NS,
    is_name_char_4e, is_name_char_unicode, is_name_start_char, is_name_start_char_4e,
};
use crate::error::{ErrorCode, ErrorDomain, ErrorLevel, Result, XmlError};
use crate::options::ParseOptions;

/// An entity-replacement input stream pushed on top of the original source.
///
/// # Invariant
///
/// `bytes` must be valid UTF-8.  All slice-to-`&str` conversions on the
/// Scanner rely on this; `from_utf8_unchecked` is used liberally.
pub(crate) struct InputStream<'src> {
    /// Replacement-text bytes.  Currently always `Cow::Owned` (entities are
    /// built up as `String`s); the `Cow` type is kept for symmetry.
    pub bytes: Cow<'src, [u8]>,
    /// Saved cursor position to restore when this frame is popped — i.e.
    /// the read position in the stream *below* this one at the moment this
    /// frame was pushed.  Not used while this frame is the active top
    /// (then the live position lives in `Scanner::cur_pos`).
    pub saved_parent_pos: usize,
    /// Name of the entity that pushed this frame, used for recursion
    /// detection and error messages.
    pub entity_name: String,
    /// Element-stack depth at the moment this stream was pushed.  When the
    /// stream is popped, the element depth must equal this — otherwise the
    /// entity introduced unbalanced markup (XML 1.0 § 4.3.2).
    pub depth_at_push: u32,
    /// Absolute URL the bytes were originally loaded from, for relative
    /// URI resolution of nested SYSTEM identifiers (XML 1.0 § 4.2.2 +
    /// errata E18).  `None` for streams whose bytes don't come from an
    /// external resource (e.g. an internal parameter entity).
    pub base_uri: Option<String>,
}

/// Byte cursor with stream-stack semantics.  See module docs for the hot-
/// path layout — `cur_ptr`/`cur_len`/`cur_pos` always describe the live
/// view so accessors don't need to branch.
///
/// The `'opt` lifetime parameter is on the [`ParseOptions`] reference held
/// inside `opts`.  The *outer* Scanner inside an `XmlReader` / `Parser` holds
/// `Cow::Owned(opts)` and uses `'opt = 'static`.  Short-lived *inner*
/// Scanners (constructed per start tag to parse attribute values) hold
/// `Cow::Borrowed(parent_opts)` and inherit the borrow's lifetime — that's
/// the path the hot loop runs through, and the borrow avoids one
/// `ParseOptions` clone per tag.
pub(crate) struct Scanner<'src, 'opt> {
    /// Pointer to the start of the *currently active* buffer.
    ///
    /// # Invariant
    ///
    /// Points to either `self.src` (when `entity_streams` is empty) or to
    /// the heap allocation owned by `entity_streams.last().unwrap().bytes`
    /// (otherwise).  Both buffers outlive any accessor call that returns
    /// a slice based on `cur_ptr` because the Scanner owns `entity_streams`
    /// and holds `src: &'src [u8]`.  Updated by `push_entity_stream` and
    /// `try_pop_entity_stream` whenever the active buffer changes.
    cur_ptr: *const u8,
    /// Length of the currently active buffer (`src.len()` or the top
    /// entity frame's `bytes.len()`).
    cur_len: usize,
    /// Live read position within the active buffer.
    cur_pos: usize,

    /// Pointer to the start of the original source bytes.  Stored as a
    /// raw pointer (instead of a `&'src [u8]` field) so we can support
    /// **in-place mutation** of the source buffer during parsing — see
    /// [`Self::compact_at`].  When `in_place_mut` is set, writes via that
    /// pointer are sound because we hold exclusive access to the buffer
    /// through it (no outstanding `&[u8]` reborrow lives across a write).
    /// When `in_place_mut` is None (the common case) the pointer is
    /// derived from a `&'src [u8]` and we only ever read through it.
    src_ptr: *const u8,
    /// Original source length.  Equal to `src_ptr`'s allocation length
    /// initially; in in-place mode shrinks as `compact_at` removes
    /// expanded-out bytes.
    src_orig_len: usize,
    /// When `Some(ptr)`, the scanner is in **in-place destructive mode**:
    /// `compact_at` can write through this pointer to mutate the source
    /// buffer.  `ptr` aliases `src_ptr` cast to `*mut` — it's a separate
    /// field so non-in-place callers can't accidentally trip the write
    /// path.
    in_place_mut: Option<std::ptr::NonNull<u8>>,

    /// Active entity-replacement frames, top of stack at the back.  Empty
    /// in the common no-entity case.  Each frame remembers the parent
    /// stream's cursor position at the time it was pushed (so pop can
    /// restore it).
    entity_streams: Vec<InputStream<'src>>,

    /// Carries the `'src` lifetime through to accessors that produce
    /// `Cow::Borrowed(&'src str)`, since `cur_ptr` is a raw pointer.
    _marker: PhantomData<&'src [u8]>,

    pub opts: Cow<'opt, ParseOptions>,
}

impl<'src, 'opt> Scanner<'src, 'opt> {
    /// Construct a `Scanner` reading the original source bytes.
    ///
    /// `opts` is a `Cow` so the same Scanner type works for both the outer
    /// (owned) case and the per-tag inner attribute scanner (borrowed),
    /// avoiding a per-tag `ParseOptions` clone on the hot path.
    ///
    /// # Safety / invariant
    ///
    /// The caller must ensure `src` is valid UTF-8.
    pub fn new(src: &'src [u8], opts: Cow<'opt, ParseOptions>) -> Self {
        Self {
            cur_ptr: src.as_ptr(),
            cur_len: src.len(),
            cur_pos: 0,
            src_ptr: src.as_ptr(),
            src_orig_len: src.len(),
            in_place_mut: None,
            entity_streams: Vec::new(),
            _marker: PhantomData,
            opts,
        }
    }

    /// Construct a Scanner that may mutate its source buffer in place
    /// (destructive parsing).  `compact_at` is enabled in this mode;
    /// all other behaviour is identical to [`new`](Self::new).
    ///
    /// The caller transfers exclusive write access to `src` for the
    /// scanner's lifetime — no other `&[u8]` or `&mut [u8]` view of
    /// these bytes may exist concurrently.
    pub fn new_in_place(src: &'src mut [u8], opts: Cow<'opt, ParseOptions>) -> Self {
        let mut_ptr = src.as_mut_ptr();
        let len = src.len();
        Self {
            cur_ptr: mut_ptr as *const u8,
            cur_len: len,
            cur_pos: 0,
            src_ptr: mut_ptr as *const u8,
            src_orig_len: len,
            in_place_mut: std::ptr::NonNull::new(mut_ptr),
            entity_streams: Vec::new(),
            _marker: PhantomData,
            opts,
        }
    }

    /// Original source bytes as a slice.  Constructed on demand from
    /// `(src_ptr, src_orig_len)` — short-lived; do not hold across any
    /// `compact_at` call (which mutates the same memory).
    #[inline]
    fn src(&self) -> &[u8] {
        // SAFETY: `src_ptr` and `src_orig_len` describe a live buffer the
        // scanner holds for its `'src` lifetime (via PhantomData).  Returning
        // a slice bounded by `&self` keeps it short enough that no caller
        // can hold it across a mutating method (which requires `&mut self`).
        unsafe { std::slice::from_raw_parts(self.src_ptr, self.src_orig_len) }
    }

    /// In-place destructive mutation: replace source bytes at
    /// `start..end` with `new_bytes`, then memmove the tail left so the
    /// rest of the buffer stays contiguous.  Decreases the scanner's
    /// logical source length by `(end - start) - new_bytes.len()`.
    ///
    /// **Caller contract:**
    /// - `new_bytes.len() <= end - start` (must shrink or stay equal)
    /// - `end <= self.src_orig_len`
    /// - The scanner's current `cur_pos` must already be **past** `end`
    ///   (we don't fix up `cur_pos` here; the caller adjusts).  Typical
    ///   usage: caller is about to emit an event whose source span ended
    ///   at `end`; the cursor has just advanced past `end`; we mutate
    ///   `start..end`; the cursor needs no adjustment since the bytes
    ///   it now points at (past `end`) haven't moved relative to it.
    ///
    /// `true` if the scanner was constructed via
    /// [`new_in_place`](Self::new_in_place) and can therefore service
    /// [`compact_at`](Self::compact_at) calls.
    #[inline]
    pub fn is_in_place(&self) -> bool { self.in_place_mut.is_some() }

    /// Write `byte` at `offset` in the source buffer.  In-place only.
    /// Caller must guarantee `offset` is strictly behind `cur_pos` (we
    /// only ever write to bytes the scanner has already read past).
    ///
    /// # Panics
    ///
    /// Panics if the scanner is not in in-place mode.
    #[inline]
    #[allow(dead_code)] // reserved for in-place decoding paths
    pub fn write_byte_at(&mut self, offset: usize, byte: u8) {
        let mut_base = self.in_place_mut.expect(
            "Scanner::write_byte_at requires in-place mode",
        );
        debug_assert!(offset < self.src_orig_len);
        // SAFETY: bounds verified.  Exclusive write access via in_place_mut.
        unsafe { *mut_base.as_ptr().add(offset) = byte; }
    }

    /// Write `bytes` at `offset` in the source buffer.  In-place only.
    /// Like [`compact_at`](Self::compact_at) but without the
    /// "shrinks the logical length" semantics — used for byte-level
    /// streaming compaction where the slow path writes its decoded
    /// output directly into already-read source bytes.  Source bytes at
    /// `offset..offset+bytes.len()` are overwritten.
    ///
    /// # Panics
    ///
    /// Panics if the scanner is not in in-place mode.
    #[inline]
    #[allow(dead_code)] // reserved for in-place decoding paths
    pub fn write_bytes_at(&mut self, offset: usize, bytes: &[u8]) {
        if bytes.is_empty() { return; }
        let mut_base = self.in_place_mut.expect(
            "Scanner::write_bytes_at requires in-place mode",
        );
        debug_assert!(offset + bytes.len() <= self.src_orig_len);
        // SAFETY: bounds verified.  Exclusive write access via in_place_mut.
        // `copy` (not `copy_nonoverlapping`) — the source-buffer regions
        // may overlap when copying earlier-read literal bytes forward.
        unsafe {
            std::ptr::copy(
                bytes.as_ptr(),
                mut_base.as_ptr().add(offset),
                bytes.len(),
            );
        }
    }

    /// Get a source-bytes slice with the scanner's `'src` lifetime.
    /// Used by callers of `compact_at` to retrieve the post-write
    /// payload as a `&'src [u8]` (the bytes are stable in the buffer;
    /// the scanner never relocates them).
    #[inline]
    pub fn src_slice(&self, start: usize, end: usize) -> &'src [u8] {
        &self.src_with_src_lifetime()[start..end]
    }

    /// # Panics
    ///
    /// Panics if the scanner is not in in-place mode (built via
    /// [`new`](Self::new) rather than [`new_in_place`](Self::new_in_place)).
    pub fn compact_at(&mut self, start: usize, end: usize, new_bytes: &[u8]) {
        let mut_base = self.in_place_mut.expect(
            "Scanner::compact_at requires in-place mode (constructed via new_in_place)",
        );
        debug_assert!(start <= end);
        debug_assert!(end <= self.src_orig_len);
        debug_assert!(new_bytes.len() <= end - start);
        // SAFETY: bounds verified above.  We hold exclusive write access
        // via in_place_mut (the scanner is the sole live `&mut` borrower
        // of the buffer for its lifetime).  No re-read happens before we
        // return.
        unsafe {
            let dst = mut_base.as_ptr().add(start);
            std::ptr::copy_nonoverlapping(new_bytes.as_ptr(), dst, new_bytes.len());
        }
        // No tail memmove ("garbage tail" approach): bytes at
        // start + new_bytes.len() .. end remain in the buffer but are
        // never re-read by the scanner (cur_pos is already past `end`).
        // Strictly simpler than pugixml-style memmove and semantically
        // equivalent as long as nothing reads back into freed bytes.
    }

    /// Re-point the scanner's source view at a new buffer location.
    ///
    /// Used by the streaming reader wrapper after it has refilled,
    /// compacted, or grown its rolling buffer — the operations that
    /// invalidate the cached `cur_ptr`.  `new_cur_pos` is the
    /// cursor's new position within the new buffer (typically `0`
    /// right after a compaction that drops everything consumed so
    /// far, or the same as the old `cur_pos` after a same-allocation
    /// refill that only extended the tail).
    ///
    /// Also updates the `src_ptr` / `src_orig_len` view so accessors
    /// that surface "original source" bytes (e.g. for error context)
    /// see the current buffer rather than a stale allocation.
    ///
    /// # Safety
    ///
    /// The caller guarantees, for the lifetime of the scanner up to
    /// the next call to [`rebind`](Self::rebind):
    ///
    /// 1. `ptr..ptr+len` is a single allocated buffer the caller
    ///    holds exclusively (no concurrent writes).
    /// 2. The bytes `ptr..ptr+len` are valid UTF-8 — the scanner's
    ///    `from_utf8_unchecked` paths assume this.
    /// 3. `new_cur_pos <= len`.
    /// 4. `entity_streams.is_empty()` — the entity-stream stack must
    ///    be unwound before rebinding, because pushed frames' cached
    ///    `cur_ptr` would otherwise be silently clobbered.  Debug-
    ///    asserted; release builds rely on the caller respecting
    ///    "rebind only between events" (which is when the wrapper's
    ///    pre-fill check runs anyway).
    ///
    /// The wrapper is the only intended caller; this stays
    /// `pub(crate)` so external code can't reach it without first
    /// going through the streaming reader's safer surface.
    #[inline]
    pub(crate) unsafe fn rebind(&mut self, ptr: *const u8, len: usize, new_cur_pos: usize) {
        debug_assert!(new_cur_pos <= len, "rebind: cur_pos out of bounds");
        debug_assert!(self.entity_streams.is_empty(),
            "rebind: cannot rebind while an entity-replacement stream is active");
        self.cur_ptr      = ptr;
        self.cur_len      = len;
        self.cur_pos      = new_cur_pos;
        self.src_ptr      = ptr;
        self.src_orig_len = len;
    }

    // ── stream-stack API ─────────────────────────────────────────────────────

    /// Are we currently reading from an entity-expansion stream rather than
    /// the original source?
    #[inline]
    pub fn in_entity(&self) -> bool {
        !self.entity_streams.is_empty()
    }

    /// Total streams currently active (original source counts as 1).  Used
    /// by the per-element boundary check to detect start/end-tag pairs that
    /// straddle an entity boundary.
    #[inline]
    pub fn stream_depth(&self) -> usize {
        1 + self.entity_streams.len()
    }

    /// Push a new entity-replacement stream onto the stack.
    ///
    /// `name` is the entity name (for recursion detection and error messages),
    /// `bytes` is the replacement text (must be valid UTF-8), and
    /// `element_depth` is the current XML element-nesting depth at the call
    /// site (used to enforce balanced-markup at pop time).
    ///
    /// Returns an error if pushing this entity would create a reference cycle.
    pub fn push_entity_stream(
        &mut self,
        name: String,
        bytes: String,
        element_depth: u32,
        base_uri: Option<String>,
    ) -> Result<()> {
        // XML 1.0 § 4.1 WFC "No Recursion": reject if `name` is already on the
        // stack.
        if self.entity_streams.iter().any(|s| s.entity_name == name) {
            return Err(self.err(format!(
                "recursive entity reference: &{name}; — XML 1.0 WFC 'No Recursion' forbids \
                 an entity from being expanded inside its own replacement text"
            )));
        }
        // Save the current (parent) cursor position so pop can restore it.
        let saved_parent_pos = self.cur_pos;
        let bytes_vec: Vec<u8> = bytes.into_bytes();
        // The Vec's heap allocation is stable — it isn't moved when the
        // outer `entity_streams` Vec reallocates — so it's safe to take
        // the pointer now and rely on it remaining valid until this frame
        // is popped.
        let new_ptr = bytes_vec.as_ptr();
        let new_len = bytes_vec.len();
        self.entity_streams.push(InputStream {
            bytes:            Cow::Owned(bytes_vec),
            saved_parent_pos,
            entity_name:      name,
            depth_at_push:    element_depth,
            base_uri,
        });
        self.cur_ptr = new_ptr;
        self.cur_len = new_len;
        self.cur_pos = 0;
        Ok(())
    }

    /// Base URI of the innermost entity-stream frame that has one —
    /// i.e. the URL from which the bytes the parser is currently
    /// reading were originally fetched.  Returns `None` when no
    /// active frame has a base URI (e.g. parsing the original
    /// document source, or expanding an internal parameter entity).
    ///
    /// Used by the parser to compute the right base URI for nested
    /// SYSTEM identifiers in entity declarations (XML 1.0 § 4.2.2 +
    /// errata E18): a `<!ENTITY % x SYSTEM "rel">` encountered
    /// inside an external PE resolves `rel` against this URI, not
    /// against the document URL.
    pub fn current_base_uri(&self) -> Option<&str> {
        self.entity_streams.iter().rev()
            .find_map(|s| s.base_uri.as_deref())
    }

    /// Snapshot of the top entity frame's metadata, for boundary checks
    /// before popping.  `None` when no entity is active.
    pub fn top_entity_info(&self) -> Option<(&str, u32)> {
        self.entity_streams.last().map(|s| (s.entity_name.as_str(), s.depth_at_push))
    }

    /// Number of pushed entity-replacement frames stacked on top of
    /// the original source.  Zero = scanner is on the original
    /// source.  Used by DTD-context callers to enforce XML 1.0 § 2.8
    /// WFC: PE Between Declarations — the start and end of a markup
    /// declaration must come from the same frame, which is what this
    /// number lets the caller compare across the decl body.
    #[inline]
    pub fn entity_stream_depth(&self) -> usize {
        self.entity_streams.len()
    }

    // No auto-pop after a byte is consumed: doing so would silently break
    // callers that captured offsets like `start = cur_pos()` before reading
    // into a different (now-top) stream.  Explicit pop happens in the slow
    // paths (parse_char_data, parse_att_value, etc.) via
    // [`try_pop_entity_stream`](Self::try_pop_entity_stream).

    /// Bytes of the current input stream (the live active buffer).
    #[inline]
    pub fn cur_bytes(&self) -> &[u8] {
        // SAFETY: `cur_ptr`/`cur_len` are maintained by push/pop to point
        // at either `src` or the top entity frame's owned bytes; both live
        // at least as long as `&self` (see struct-level invariant).
        unsafe { std::slice::from_raw_parts(self.cur_ptr, self.cur_len) }
    }

    /// Bytes of the current stream from the current position to the end.
    /// The common `memchr3(..., &scan.src[scan.pos..])` pattern becomes
    /// `memchr3(..., scan.cur_tail())`.
    #[inline]
    pub fn cur_tail(&self) -> &[u8] {
        // SAFETY: `cur_pos <= cur_len` is maintained by all advance/set
        // methods; the resulting subslice is valid for the same reason as
        // `cur_bytes`.
        unsafe {
            std::slice::from_raw_parts(
                self.cur_ptr.add(self.cur_pos),
                self.cur_len - self.cur_pos,
            )
        }
    }

    /// Current byte position within the current stream.
    #[inline]
    pub fn cur_pos(&self) -> usize { self.cur_pos }

    /// Advance the current stream's position by `n` bytes (no auto-pop).
    #[inline]
    pub fn cur_advance_pos(&mut self, n: usize) { self.cur_pos += n; }

    /// Set the current stream's position to `p`.
    #[inline]
    pub fn cur_set_pos(&mut self, p: usize) { self.cur_pos = p; }

    /// Length of the current stream's byte buffer.
    #[inline]
    pub fn cur_len(&self) -> usize { self.cur_len }

    /// If the current stream is the original borrowed source, return its
    /// bytes with the longer `'src` lifetime; otherwise `None`.  Entity
    /// frames carry `Cow::Owned` bytes that don't live `'src`, so they
    /// can't be returned with that lifetime.
    ///
    /// Used by [`cur_str`](Self::cur_str) to decide whether the resulting
    /// slice can be returned as `Cow::Borrowed` (lifetime `'src`) or must be
    /// allocated to detach from the stream's lifetime.
    #[inline]
    pub fn current_borrowed_bytes(&self) -> Option<&'src [u8]> {
        if self.entity_streams.is_empty() { Some(self.src_with_src_lifetime()) } else { None }
    }

    /// Original source bytes — never reallocated, never reassigned.
    /// Use only when the caller already knows its byte offsets are
    /// relative to the original source (e.g., a name scanned at a point
    /// where `entity_streams` was empty).  Skips the `is_empty` branch
    /// that [`current_borrowed_bytes`](Self::current_borrowed_bytes)
    /// would do — meant for the per-event hot path in the reader.
    #[inline]
    pub fn src_bytes(&self) -> &'src [u8] { self.src_with_src_lifetime() }

    /// Return the source bytes with the `'src` lifetime.  Used by the
    /// hot-path accessors that produce `Cow::Borrowed(&'src str)` slices.
    /// In in-place mode the bytes are still backed by the same heap
    /// allocation (we never relocate the buffer) so the `'src` lifetime
    /// remains sound; mutations performed via [`compact_at`] update the
    /// bytes-at-a-position but never move them.
    #[inline]
    fn src_with_src_lifetime(&self) -> &'src [u8] {
        // SAFETY: `src_ptr` points at a buffer the scanner holds for its
        // `'src` lifetime (via PhantomData).  Even in in-place mode the
        // pointer is stable for `'src` — only the bytes change, not the
        // allocation address.
        unsafe { std::slice::from_raw_parts(self.src_ptr, self.src_orig_len) }
    }

    /// `true` if the active stream is the original source — i.e. no
    /// entity-replacement frame has been pushed.  Reader hot-paths
    /// that hold the cursor in stack locals (`bytes = src_bytes()`,
    /// `p = cur_pos()`) MUST gate themselves on this — when an
    /// entity stream is active, `cur_pos` is relative to the entity
    /// bytes, not the original source, and indexing `src_bytes()` by
    /// `cur_pos()` reads from the wrong buffer.
    #[inline]
    pub fn on_original_source(&self) -> bool {
        self.entity_streams.is_empty()
    }

    /// Slice of the original source between two byte offsets, as `&str`.
    /// Callers must pass offsets obtained from [`cur_pos`](Self::cur_pos)
    /// while on the original source; used to capture raw declaration
    /// spans from the internal subset.
    pub fn original_slice(&self, start: usize, end: usize) -> &str {
        let s = start.min(self.src_orig_len);
        let e = end.min(self.src_orig_len).max(s);
        // SAFETY: src_ptr/src_orig_len describe the original buffer,
        // which `transcode_and_validate` guarantees is valid UTF-8 and
        // which outlives `&self`; [s, e] is bounds-clamped above.
        unsafe {
            let bytes = std::slice::from_raw_parts(self.src_ptr.add(s), e - s);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    // ── cursor primitives ────────────────────────────────────────────────────

    /// Peek the next byte in the **current (top) stream only**.  Returns
    /// `None` if the top stream is exhausted, *even if* a stream below has
    /// more bytes — syntactic reads (names, tags, entity references) must be
    /// fully contained within a single stream per XML 1.0 § 4.3.2.  The
    /// slow paths in `parse_char_data` / `parse_att_value` handle the
    /// transition between streams explicitly via
    /// [`try_pop_entity_stream`](Self::try_pop_entity_stream).
    #[inline]
    pub fn peek(&self) -> Option<u8> {
        if self.cur_pos < self.cur_len {
            // SAFETY: bounds checked above; pointer/length invariant as for
            // `cur_bytes`.
            Some(unsafe { *self.cur_ptr.add(self.cur_pos) })
        } else {
            None
        }
    }

    /// Peek `off` bytes ahead in the **current** stream only.
    /// Returns `None` if `off` is past the current stream's end — does NOT
    /// look into streams below.
    #[inline]
    pub fn peek_at(&self, off: usize) -> Option<u8> {
        let p = self.cur_pos.checked_add(off)?;
        if p < self.cur_len {
            // SAFETY: bounds checked above.
            Some(unsafe { *self.cur_ptr.add(p) })
        } else {
            None
        }
    }

    /// `true` iff we've consumed every byte in every stream.
    #[inline]
    pub fn is_eof(&self) -> bool {
        // Top is exhausted *and* there's no stream below to fall back to.
        self.cur_pos >= self.cur_len && self.entity_streams.is_empty()
    }

    /// Does the **current** stream's remaining bytes start with `pat`?
    /// Does not look across streams.
    #[inline]
    pub fn starts_with(&self, pat: &[u8]) -> bool {
        self.cur_tail().starts_with(pat)
    }

    pub fn advance(&mut self) -> Option<u8> {
        // Top-only consume.  Returns `None` if the top stream is exhausted —
        // explicit pops at the slow-path boundaries handle the transition to
        // the stream below.  Crossing a stream boundary mid-token would
        // silently violate XML 1.0 § 4.3.2 (parsed entities must be
        // syntactically self-contained).
        //
        // We no longer track per-byte `(line, col)` here.  The bulk-content
        // scans (`memchr3` / `cur_advance_pos(n)`) never updated those
        // counters anyway, so they were already approximate.  Errors are
        // located by byte offset in `src` and the file:line:col coordinate
        // is computed lazily by `compute_line_col` in `err()` — accurate,
        // zero cost on the hot path, only a one-time scan of the input
        // when an error actually fires.
        if self.cur_pos >= self.cur_len { return None; }
        // SAFETY: bounds checked above.
        let b = unsafe { *self.cur_ptr.add(self.cur_pos) };
        self.cur_pos += 1;
        Some(b)
    }

    pub fn skip_n(&mut self, n: usize) {
        for _ in 0..n { self.advance(); }
    }

    pub fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.advance();
        }
    }

    /// Byte offset within the *original source* (`src`) at which the parser
    /// is currently positioned.  When parsing inside an entity replacement
    /// stream, this returns the position of the entity reference in the
    /// original document — i.e. the user-visible location, not the offset
    /// inside the synthetic replacement bytes.
    pub fn src_offset(&self) -> usize {
        if let Some(bottom) = self.entity_streams.first() {
            bottom.saved_parent_pos
        } else {
            self.cur_pos
        }
    }

    /// Construct a fatal-level parser error at the current source
    /// position.  Use this for unrecoverable problems (truncated
    /// input mid-construct, invalid UTF-8 in source bytes, resource
    /// exhaustion).  `recovery_mode` mode does NOT downgrade
    /// these — they always abort the parse.
    pub fn err(&self, msg: impl Into<String>) -> XmlError {
        self.err_with_level(ErrorLevel::Fatal, msg)
    }

    /// Construct a parser error at the current source position with
    /// an explicit severity level.  Use `ErrorLevel::Error` for
    /// well-formedness violations that recovery mode can repair
    /// past (mismatched end tag, undefined entity, bare `&` in text,
    /// etc.); use `ErrorLevel::Fatal` for things recovery cannot
    /// help with.  See `ParseOptions::recovery_mode` docs for
    /// the policy.
    pub fn err_with_level(&self, level: ErrorLevel, msg: impl Into<String>) -> XmlError {
        let offset = self.src_offset();
        let (line, col) = compute_line_col(self.src(), offset);
        // Attribute the error to the document's source URL when known (set
        // by the file/URL parse entry points), so consumers reporting the
        // error — lxml's `XMLSyntaxError.filename` — see the real path.
        // In-memory parses leave `base_url` unset and keep `<input>`.
        let source = self.opts.base_url.as_deref().unwrap_or("<input>");
        XmlError::new(ErrorDomain::Parser, level, msg)
            .at(source, line, col, offset as u64)
    }

    pub fn expect(&mut self, b: u8) -> Result<()> {
        match self.advance() {
            Some(c) if c == b => Ok(()),
            Some(c) => Err(self.err(format!("expected '{}', got '{}'", b as char, c as char))),
            None    => Err(self.err(format!("expected '{}', got EOF", b as char))),
        }
    }

    pub fn expect_str(&mut self, pat: &[u8]) -> Result<()> {
        for &b in pat { self.expect(b)?; }
        Ok(())
    }

    pub fn expect_ws(&mut self) -> Result<()> {
        if !matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            return Err(self.err("expected whitespace"));
        }
        Ok(())
    }

    /// Slice the current stream from `start` to `end` as a string.
    ///
    /// Returns `Cow::Borrowed` (lifetime `'src`) when the current stream is
    /// the original borrowed source — the common case, zero-copy.
    /// Returns `Cow::Owned` when the current stream is an entity expansion
    /// (the slice would otherwise dangle when the stream is popped).
    pub fn cur_str(&self, start: usize, end: usize) -> Cow<'src, str> {
        if self.entity_streams.is_empty() {
            // Hot path: bottom of stack, borrowed from `'src`.
            // SAFETY: Scanner invariant — bytes are valid UTF-8; cursor
            // operations respect UTF-8 character boundaries.
            let s = unsafe { std::str::from_utf8_unchecked(&self.src_with_src_lifetime()[start..end]) };
            Cow::Borrowed(s)
        } else {
            // SAFETY: cur_ptr/cur_len point at the top entity frame's bytes.
            let bytes = unsafe { std::slice::from_raw_parts(self.cur_ptr, self.cur_len) };
            let s = unsafe { std::str::from_utf8_unchecked(&bytes[start..end]) };
            Cow::Owned(s.to_string())
        }
    }

    /// Byte-output sibling of [`cur_str`](Self::cur_str).  Returns the same
    /// slice typed as `Cow<'src, [u8]>` — `Cow::Borrowed` when reading from
    /// the original source (zero-copy), `Cow::Owned` when reading from an
    /// entity expansion stream (the slice would dangle after the stream is
    /// popped).
    ///
    /// This is the primitive used by `XmlBytesReader`.  `cur_str` is now
    /// implemented in terms of this plus a no-op `from_utf8_unchecked`
    /// cast — the bytes are valid UTF-8 by the Scanner's construction-time
    /// invariant.
    pub fn cur_slice(&self, start: usize, end: usize) -> Cow<'src, [u8]> {
        if self.entity_streams.is_empty() {
            Cow::Borrowed(&self.src_with_src_lifetime()[start..end])
        } else {
            // SAFETY: cur_ptr/cur_len point at the top entity frame's bytes.
            let bytes = unsafe { std::slice::from_raw_parts(self.cur_ptr, self.cur_len) };
            Cow::Owned(bytes[start..end].to_vec())
        }
    }

    /// Append the slice `start..end` of the current stream to `buf`.  Works
    /// for both borrowed-source and entity-expansion streams, and avoids the
    /// allocation that `cur_str` would do in the entity case.
    #[allow(dead_code)] // mirror of `append_cur_bytes`, kept for the `String` payload path
    pub fn append_cur_str(&self, start: usize, end: usize, buf: &mut String) {
        // SAFETY: Scanner invariant — bytes are valid UTF-8.
        let bytes = self.cur_bytes();
        let s = unsafe { std::str::from_utf8_unchecked(&bytes[start..end]) };
        buf.push_str(s);
    }

    /// Byte-output sibling of [`append_cur_str`](Self::append_cur_str).
    /// Appends the slice `start..end` of the current stream to `buf` as
    /// raw bytes.  Public for symmetry with `append_cur_str`; the
    /// in-tree callers all use the normalizing variants
    /// (`append_text_segment`, `append_attr_segment` in
    /// `xml_bytes_reader`) so they can apply §2.11 / §3.3.3 on the fly.
    #[allow(dead_code)]
    pub fn append_cur_bytes(&self, start: usize, end: usize, buf: &mut Vec<u8>) {
        let bytes = self.cur_bytes();
        buf.extend_from_slice(&bytes[start..end]);
    }

    /// If we're currently reading from an entity-expansion stream, pop it.
    /// Returns `true` if a pop happened (caller should continue reading from
    /// the now-current stream), `false` if we're at the bottom of the stack
    /// (original source) — meaning real EOF for the document.
    pub fn try_pop_entity_stream(&mut self) -> bool {
        let Some(popped) = self.entity_streams.pop() else { return false; };
        // Restore the cursor to where the parent stream left off.
        self.cur_pos = popped.saved_parent_pos;
        if let Some(new_top) = self.entity_streams.last() {
            self.cur_ptr = new_top.bytes.as_ptr();
            self.cur_len = new_top.bytes.len();
        } else {
            self.cur_ptr = self.src_ptr;
            self.cur_len = self.src_orig_len;
        }
        true
    }

    // ── name scanning ────────────────────────────────────────────────────────

    /// Decode the next Unicode scalar value at the cursor without advancing.
    /// Reads from the **current** stream only (names cannot span streams per
    /// XML well-formedness).
    #[allow(dead_code)] // reserved for char-aware lookahead in name validators
    pub fn peek_char(&self) -> Option<(char, usize)> {
        if self.cur_pos >= self.cur_len { return None; }
        let bytes = self.cur_tail();
        let b = bytes[0];
        if b < 0x80 { return Some((b as char, 1)); }
        let len = utf8_seq_len(b)?;
        let slice = bytes.get(..len)?;
        let s = std::str::from_utf8(slice).ok()?;
        s.chars().next().map(|c| (c, len))
    }

    /// Scan an XML Name within the current stream and return its
    /// `(start, end)` byte offsets in that stream.
    ///
    /// # Performance
    ///
    /// The hot inner loop reads bytes directly through `cur_ptr` and the
    /// ASCII `NS|NC` table — bypassing `peek`/`advance` so we don't pay the
    /// per-byte newline check + `col`/`line` bump that those do for each
    /// step.  Names can't contain newlines (XML 1.0 § 2.3 NameChar), so
    /// `col` is bumped exactly once at the end by the total byte count.
    /// LLVM autovectorizes the ASCII run.
    pub fn scan_name_raw(&mut self) -> Result<(usize, usize)> {
        let lax = self.opts.skip_name_validation;
        let start = self.cur_pos;
        // SAFETY: cur_ptr/cur_len describe the live active buffer (see
        // struct-level invariants); the slice lives at least as long as
        // `&mut self` because `entity_streams` can only shrink under
        // `&mut self`.
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(self.cur_ptr, self.cur_len) };
        let end = self.cur_len;
        let mut i = start;

        // ── NameStartChar ───────────────────────────────────────────────────
        if i >= end {
            return Err(self.err("expected XML name, got EOF"));
        }
        let b0 = bytes[i];
        if b0 < 0x80 {
            if !lax && ASCII_NAME[b0 as usize] & NS == 0 {
                return Err(self.err(format!("invalid name-start character {:?}", b0 as char))
                    .with_code(crate::error::ErrorCode::NameRequired));
            }
            i += 1;
        } else {
            let len = match utf8_seq_len(b0) {
                Some(l) if i + l <= end => l,
                _ => return Err(self.err("invalid UTF-8 in name")),
            };
            let slice = &bytes[i..i + len];
            let c = std::str::from_utf8(slice).ok()
                .and_then(|s| s.chars().next())
                .ok_or_else(|| self.err("invalid UTF-8 in name"))?;
            if !lax && !self.name_start_non_ascii(c) {
                return Err(self.err(format!("invalid name-start character {:?}", c))
                    .with_code(crate::error::ErrorCode::NameRequired));
            }
            i += len;
        }

        // ── NameChar* (rest of the name) ────────────────────────────────────
        //
        // The ASCII NameChar table `ASCII_XML_NAME_CHAR` is `0` for every
        // byte that isn't an ASCII name char *or* has the high bit set —
        // a single lookup-and-compare doubles as the "is ASCII name char?"
        // *and* the "stop at non-ASCII" test, eliminating the branch on
        // `b < 0x80` per iteration.  LLVM autovectorizes the loop into
        // SIMD compares + pshufb on x86_64 / NEON on aarch64, so a 16-byte
        // name is found in ~2 SIMD steps instead of 16 scalar iterations.
        //
        // We tried an 8-byte u64 SWAR fast path for lax mode (`skip_name_
        // validation = true`).  Result: 4-5% regression on swiss_prot.
        // The SWAR per-call overhead (~12 cycles for chunk load + four
        // SWAR comparisons + trailing_zeros + branch) breaks even at
        // ~6-byte names and only wins for ~10+ bytes.  XML attribute
        // names average 3-6 bytes; the per-attribute cost is where the
        // regression came from.  See git history for the SWAR
        // implementation — kept here as a comment so the next person
        // tempted to try it knows what they're getting into.
        let _ = lax;  // kept in scope; SWAR path was its only consumer.
        // SAFETY: the `i < end` guard inside the while runs *before* the
        // body, and `end == bytes.len()` by construction earlier in this
        // function (`bytes = slice::from_raw_parts(self.cur_ptr, self.cur_len)`
        // and `end = self.cur_len`).  So `i < end` ⇒ `i < bytes.len()`,
        // which is exactly the precondition `get_unchecked` requires.
        // We replace the safe `bytes[i]` to drop the per-iteration bounds
        // check that LLVM doesn't always elide here; this loop is 14.6%
        // of swiss_prot parse time per profiling.  See CONTRIBUTING.md
        // § "Unsafe policy".
        while i < end && ASCII_XML_NAME_CHAR[unsafe { *bytes.get_unchecked(i) } as usize] != 0 {
            i += 1;
        }

        // Fell out of the ASCII fast loop — either real end, or a
        // non-ASCII byte that the lookup table conservatively zeroed.
        // For non-ASCII, decode and consult the Unicode NameChar tables.
        while i < end {
            // SAFETY: while-guard `i < end` and `end == bytes.len()`
            // (same argument as the fast loop above).
            let b = unsafe { *bytes.get_unchecked(i) };
            if b < 0x80 { break; }  // genuine ASCII non-namechar — stop.
            let len = match utf8_seq_len(b) {
                Some(l) if i + l <= end => l,
                _ => break,
            };
            if lax {
                i += len;
                continue;
            }
            let slice = &bytes[i..i + len];
            let c_opt = std::str::from_utf8(slice).ok().and_then(|s| s.chars().next());
            match c_opt {
                Some(c) if self.name_char_non_ascii(c) => i += len,
                _ => break,
            }
            // After a non-ASCII name char, we may follow with more ASCII —
            // re-enter the fast loop.
            // SAFETY: same argument as the primary fast loop above.
            while i < end && ASCII_XML_NAME_CHAR[unsafe { *bytes.get_unchecked(i) } as usize] != 0 {
                i += 1;
            }
        }

        self.cur_pos = i;
        Ok((start, i))
    }

    /// Scan a name and return it.  `Cow::Borrowed` when the current stream is
    /// the original source (zero-copy, lifetime `'src`); `Cow::Owned` when
    /// the name comes from an entity expansion stream.
    pub fn scan_name(&mut self) -> Result<Cow<'src, str>> {
        let (s, e) = self.scan_name_raw()?;
        Ok(self.cur_str(s, e))
    }

    /// Scan an XML 1.0 § 2.3 [7] Nmtoken — `(NameChar)+`.  Differs from
    /// `scan_name_raw` only in skipping the NameStartChar restriction
    /// on the first byte: an Nmtoken may start with a digit, `-`, `.`,
    /// or any other NameChar.  Used for ATTLIST enumerations, where
    /// the values are Nmtokens (`(0|35a|...)`), not Names.
    pub fn scan_nmtoken(&mut self) -> Result<Cow<'src, str>> {
        let lax = self.opts.skip_name_validation;
        let start = self.cur_pos;
        // SAFETY: cur_ptr/cur_len describe the live active buffer (see
        // struct-level invariants).
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(self.cur_ptr, self.cur_len) };
        let end = self.cur_len;
        let mut i = start;

        // ── NameChar+ (no NameStart distinction) ────────────────────
        // Match scan_name_raw's NameChar loop verbatim, minus the
        // leading NameStartChar check.
        while i < end && ASCII_XML_NAME_CHAR[unsafe { *bytes.get_unchecked(i) } as usize] != 0 {
            i += 1;
        }
        while i < end {
            let b = unsafe { *bytes.get_unchecked(i) };
            if b < 0x80 { break; }
            let len = match utf8_seq_len(b) {
                Some(l) if i + l <= end => l,
                _ => break,
            };
            if lax {
                i += len;
                continue;
            }
            let slice = &bytes[i..i + len];
            let c_opt = std::str::from_utf8(slice).ok().and_then(|s| s.chars().next());
            match c_opt {
                Some(c) if self.name_char_non_ascii(c) => i += len,
                _ => break,
            }
            while i < end && ASCII_XML_NAME_CHAR[unsafe { *bytes.get_unchecked(i) } as usize] != 0 {
                i += 1;
            }
        }

        if i == start {
            return Err(self.err("expected Nmtoken (one or more NameChar)"));
        }
        self.cur_pos = i;
        Ok(self.cur_str(start, i))
    }

    /// Byte-output sibling of [`scan_name`](Self::scan_name).  Returns the
    /// scanned name as a `Cow<'src, [u8]>` — same memory, no `from_utf8`
    /// cast.  Used by `XmlBytesReader`.
    pub fn scan_name_bytes(&mut self) -> Result<Cow<'src, [u8]>> {
        let (s, e) = self.scan_name_raw()?;
        Ok(self.cur_slice(s, e))
    }

    pub fn skip_name(&mut self) -> Result<()> {
        self.scan_name_raw().map(|_| ())
    }

    #[inline]
    fn name_start_non_ascii(&self, c: char) -> bool {
        if self.opts.xml10_fourth_edition { is_name_start_char_4e(c) } else { is_name_start_char(c) }
    }

    #[inline]
    fn name_char_non_ascii(&self, c: char) -> bool {
        if self.opts.xml10_fourth_edition { is_name_char_4e(c) } else { is_name_char_unicode(c) }
    }
}

// ── error-location helper ────────────────────────────────────────────────────

/// Compute 1-based `(line, col)` for the given byte offset in `src`.
///
/// Called only when an error is being constructed.  Uses `memchr_iter` to
/// count newlines and `memrchr` to find the start of the offending line, so
/// the scan is SIMD-accelerated even though it runs O(n).  Total cost is
/// roughly equivalent to one extra pass over the prefix — fine in an error
/// path that fires at most once per parse.
///
/// The hot parsing path no longer updates a per-byte `(line, col)`; instead
/// the parser tracks a single byte cursor (the offset in `src`) and we
/// translate to the human-friendly coordinate only when constructing the
/// error.
pub fn compute_line_col(src: &[u8], offset: usize) -> (u32, u32) {
    let end = offset.min(src.len());
    let prefix = &src[..end];
    let line = memchr_iter(b'\n', prefix).count() as u32 + 1;
    let col_start = match memrchr(b'\n', prefix) {
        Some(n) => n + 1, // first byte after the newline
        None => 0,
    };
    let col = (end - col_start) as u32 + 1;
    (line, col)
}

// ── document-level character validation ──────────────────────────────────────

/// Validate every byte in `bytes` against the XML 1.0 § 2.2 Char production.
///
/// `bytes` must already be valid UTF-8.  This scan rejects:
/// * ASCII control characters 0x00–0x08, 0x0B, 0x0C, 0x0E–0x1F
///   (everything below 0x20 except TAB, LF, CR)
/// * U+FFFE and U+FFFF (encoded as the 3-byte UTF-8 sequences `EF BF BE`
///   and `EF BF BF`)
///
/// UTF-8 itself already rejects surrogate code points (U+D800–U+DFFF) at
/// decode time, so we don't need a separate check for those.
///
/// # Hot path
///
/// 99% of XML bytes are >= 0x20 and != 0xEF — printable ASCII or non-prefix
/// UTF-8 continuation bytes.  The SWAR loop reduces that case to one 8-byte
/// `wrapping_sub`/AND/OR pipeline per chunk (no branches per byte), falling
/// back to byte-level inspection only on chunks that touch a control byte
/// or a `0xEF` prefix.  Called inline at content boundaries (text, attr
/// values, comments, CDATA, PI bodies) so the bytes are cache-warm.
pub fn validate_xml_chars(bytes: &[u8]) -> Result<()> {
    // SWAR fast path: 8 bytes per iter, branchless detection of:
    //   - any byte < 0x20 *and* ≠ TAB/LF/CR (forbidden control)
    //   - any byte == 0xEF (potential 0xEF 0xBF 0xBE/BF — U+FFFE/FFFF)
    //
    // All reduced to the classic "has-zero-byte" trick:
    //   hasZero(x) = (x - 0x0101…) & !x & 0x8080… != 0
    //
    // Filtering TAB/LF/CR out of the SWAR mask is the win over the naïve
    // "any byte < 0x20" check: documents like swiss_prot have a newline on
    // every line (~1–2% of bytes), so without this filter ~16% of 8-byte
    // chunks would unnecessarily drop to the byte-level slow path.
    //
    // We *don't* further refine `0xEF` here — a lone `0xEF` is the legitimate
    // UTF-8 lead byte for codepoints U+F000–U+FFFF (CJK / PUA), so chunks
    // containing those legitimately drop to the byte-level slow path, which
    // is the right behaviour for any docs that actually contain such
    // codepoints (rare in practice).  An earlier attempt to add an
    // `0xEF`→`0xBF` SWAR follower-check did speed CJK-heavy docs by ~10%
    // but added enough per-chunk overhead to regress small ASCII fixtures
    // by 15–25%, so the trade-off wasn't worth it.
    const LSB: u64 = 0x0101_0101_0101_0101;
    const MSB: u64 = 0x8080_8080_8080_8080;
    const HI3: u64 = 0xE0E0_E0E0_E0E0_E0E0;
    const LO5: u64 = 0x1F1F_1F1F_1F1F_1F1F;
    const TAB: u64 = 0x0909_0909_0909_0909;
    const LF:  u64 = 0x0A0A_0A0A_0A0A_0A0A;
    const CR:  u64 = 0x0D0D_0D0D_0D0D_0D0D;
    const EF:  u64 = 0xEFEF_EFEF_EFEF_EFEF;

    #[inline(always)]
    fn has_zero(x: u64) -> u64 {
        x.wrapping_sub(LSB) & !x & MSB
    }

    let mut i = 0;
    while i + 8 <= bytes.len() {
        let chunk = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());

        let any_lt20 = has_zero(chunk & HI3);
        let any_ef   = has_zero(chunk ^ EF);

        // Cheap first-line filter: most chunks are printable ASCII with
        // no `< 0x20` and no `0xEF`, and skip the more expensive TAB/LF/CR
        // discrimination entirely.
        if (any_lt20 | any_ef) == 0 {
            i += 8;
            continue;
        }

        // Chunk has either a low byte or a `0xEF`.  For the low-byte case
        // we still want to *exclude* legal TAB/LF/CR before falling into
        // the byte-level scalar path — otherwise every chunk containing a
        // newline (i.e. most chunks in indented XML) pays scalar cost.
        let bad_ctrl = if any_lt20 != 0 {
            let low5 = chunk & LO5;
            let allowed = has_zero(low5 ^ TAB)
                        | has_zero(low5 ^ LF)
                        | has_zero(low5 ^ CR);
            any_lt20 & !allowed
        } else {
            0
        };

        if (bad_ctrl | any_ef) != 0 {
            validate_xml_chars_slow(bytes, i, i + 8)?;
        }
        i += 8;
    }
    validate_xml_chars_slow(bytes, i, bytes.len())
}

/// Scalar validation for a `[from..to]` window — used by both the SWAR
/// fallback (when a chunk fails the SWAR test) and the tail (last < 8 bytes).
#[cold]
#[inline(never)]
fn validate_xml_chars_slow(bytes: &[u8], from: usize, to: usize) -> Result<()> {
    let mut i = from;
    while i < to {
        let b = bytes[i];
        if b < 0x20 {
            if !matches!(b, 0x09 | 0x0A | 0x0D) {
                return Err(invalid_char_at(bytes, i, format!("U+{b:04X}")));
            }
        } else if b == 0xEF && i + 2 < bytes.len() && bytes[i + 1] == 0xBF {
            match bytes[i + 2] {
                0xBE => return Err(invalid_char_at(bytes, i, "U+FFFE".into())),
                0xBF => return Err(invalid_char_at(bytes, i, "U+FFFF".into())),
                _ => {}
            }
        }
        i += 1;
    }
    Ok(())
}

/// Build the structured "invalid XML character" error with line/col
/// derived from the same byte index that goes into the message.
fn invalid_char_at(bytes: &[u8], i: usize, codepoint: String) -> XmlError {
    let (line, col) = compute_line_col(bytes, i);
    XmlError::new(
        ErrorDomain::Parser,
        ErrorLevel::Fatal,
        format!("invalid XML character {codepoint} at byte {i} (XML 1.0 § 2.2)"),
    )
    .with_code(ErrorCode::InvalidChar)
    .at("<input>", line, col, i as u64)
}

// ── character classification ──────────────────────────────────────────────────

/// XML 1.0 § 2.2 legal character set.
pub fn is_xml_char(c: char) -> bool {
    matches!(c as u32,
        0x09 | 0x0A | 0x0D |
        0x20..=0xD7FF |
        0xE000..=0xFFFD |
        0x10000..=0x10FFFF
    )
}

/// XML 1.1 § 2.2 legal character set — strictly broader than
/// [`is_xml_char`].  XML 1.1 added the C0 control range (`#x1-#x1F`,
/// excluding `#x0`) and the C1 controls (`#x7F-#x9F`) to the Char
/// production, but most of those are *restricted chars* per § 2.11
/// that MUST appear only as character references (never as raw
/// bytes).  This function returns true for any code point a `&#...;`
/// reference may legally expand to under XML 1.1; the raw-byte
/// rejection still happens in [`validate_xml_chars`].
pub fn is_xml_11_char(c: char) -> bool {
    matches!(c as u32,
        0x01..=0xD7FF |
        0xE000..=0xFFFD |
        0x10000..=0x10FFFF
    )
}

/// XML 1.0 § 2.3 production [13]: PubidChar.
pub fn is_pubid_char(b: u8) -> bool {
    matches!(b,
        0x20 | 0x0D | 0x0A |
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' |
        b'-' | b'\'' | b'(' | b')' | b'+' | b',' | b'.' | b'/' |
        b':' | b'=' | b'?' | b';' | b'!' | b'*' | b'#' | b'@' | b'$' | b'_' | b'%'
    )
}

/// Length of the UTF-8 sequence starting with `lead`, or `None` for invalid lead.
pub fn utf8_seq_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC0..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF7 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::ParseOptions;

    fn fresh<'src>(src: &'src [u8]) -> Scanner<'src, 'static> {
        Scanner::new(src, Cow::Owned(ParseOptions::default()))
    }

    #[test]
    fn current_base_uri_none_on_original_source() {
        let s = fresh(b"<r/>");
        assert_eq!(s.current_base_uri(), None);
    }

    #[test]
    fn current_base_uri_returns_top_frames_uri() {
        let mut s = fresh(b"<r/>");
        s.push_entity_stream(
            "outer".to_string(),
            "abc".to_string(),
            0,
            Some("file:///docs/outer.ent".to_string()),
        ).unwrap();
        assert_eq!(s.current_base_uri(), Some("file:///docs/outer.ent"));
    }

    #[test]
    fn current_base_uri_falls_through_a_none_frame() {
        // An internal PE (None base_uri) shadowed over an external
        // PE (Some) must still expose the external one's URL —
        // E18 expects "innermost frame with a base_uri", not
        // "innermost frame, even if its base_uri is None".
        let mut s = fresh(b"<r/>");
        s.push_entity_stream(
            "outer".to_string(),
            "abc".to_string(),
            0,
            Some("file:///docs/outer.ent".to_string()),
        ).unwrap();
        s.push_entity_stream(
            "inner".to_string(),
            "def".to_string(),
            0,
            None,
        ).unwrap();
        assert_eq!(s.current_base_uri(), Some("file:///docs/outer.ent"));
    }

    #[test]
    fn current_base_uri_uses_innermost_external_frame() {
        let mut s = fresh(b"<r/>");
        s.push_entity_stream(
            "outer".to_string(),
            "abc".to_string(),
            0,
            Some("file:///outer.ent".to_string()),
        ).unwrap();
        s.push_entity_stream(
            "inner".to_string(),
            "def".to_string(),
            0,
            Some("file:///docs/sub/inner.ent".to_string()),
        ).unwrap();
        // Innermost wins — that's where the bytes we're currently
        // reading from came from.
        assert_eq!(s.current_base_uri(), Some("file:///docs/sub/inner.ent"));
    }

    /// `rebind` swaps the scanner's source view to a fresh buffer
    /// with the cursor pointing at a new position.  Used by the
    /// streaming reader after refill / compaction / growth.
    #[test]
    fn rebind_swings_cursor_to_new_buffer() {
        let a = b"<a/>";
        let b = b"<bbbb/>";
        let mut s = fresh(a);
        // Advance partway into `a` so we can verify cur_pos is
        // overwritten (not preserved) by rebind.
        s.advance(); s.advance();
        assert_eq!(s.cur_pos(), 2);
        // Swing onto a fresh buffer, asking for cur_pos at the
        // second byte of `b`.
        // SAFETY: `b` outlives the assertions below, is valid UTF-8,
        // and no entity stream is active.
        unsafe { s.rebind(b.as_ptr(), b.len(), 1); }
        assert_eq!(s.cur_pos(), 1);
        assert_eq!(s.cur_len(), b.len());
        // The next byte we read should be `b[1]`, i.e. `b'b'`.
        assert_eq!(s.peek(), Some(b'b'));
    }

    #[test]
    fn rebind_lets_subsequent_advances_walk_new_buffer() {
        let a = b"xxxx";
        let b = b"yyy";
        let mut s = fresh(a);
        // Drain `a` entirely.
        while s.advance().is_some() {}
        // SAFETY: `b` is valid UTF-8, outlives the loop, no entity
        // stream active.
        unsafe { s.rebind(b.as_ptr(), b.len(), 0); }
        let mut bytes = Vec::new();
        while let Some(byte) = s.advance() { bytes.push(byte); }
        assert_eq!(&bytes[..], b"yyy");
    }
}
