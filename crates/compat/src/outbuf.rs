//! `xmlBuffer` + `xmlOutputBuffer` — the byte-target that
//! `xmlNodeDumpOutput` and lxml's serializer write into.
//!
//! Both structs are byte-exact mirrors of libxml2's `_xmlBuffer` and
//! `_xmlOutputBuffer` so callers reading `buf->content` (offset 0)
//! and `buf->use` (offset 8) land on the right bytes.  The internal
//! storage is a `Vec<u8>` that we keep in a parallel `XmlBufferInner`
//! allocation; the `content` and `use` slots on the public struct
//! are eagerly synced after every write.

use std::io::Write;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

use sup_xml_core::output::OutputCharset;

/// Map an output-encoding name to the charset the serializer should
/// escape against.  `None` (no encoding requested) defaults to ASCII —
/// libxml2's serializer escapes non-ASCII when no encoding is set.
/// Encodings whose full Unicode repertoire is representable (UTF-8/16/32)
/// map to `Utf8` (the output buffer handles the byte transcoding); only
/// the narrow charsets need character-reference escaping during
/// serialization.
pub(crate) fn charset_for_encoding(name: Option<&str>) -> OutputCharset {
    let Some(name) = name else { return OutputCharset::Ascii };
    let n = name.trim().to_ascii_uppercase();
    match n.as_str() {
        "UTF-8" | "UTF8" => OutputCharset::Utf8,
        "UTF-16" | "UTF16" | "UTF-16LE" | "UTF-16BE"
        | "UTF-32" | "UTF32" | "UTF-32LE" | "UTF-32BE"
        | "UCS-2" | "UCS-4" | "UCS-4LE" | "UCS-4BE" => OutputCharset::Utf8,
        "ASCII" | "US-ASCII" | "USASCII" | "ANSI_X3.4-1968" => OutputCharset::Ascii,
        "ISO-8859-1" | "ISO8859-1" | "ISO_8859-1" | "LATIN1" | "LATIN-1"
        | "L1" | "CP819" | "IBM819" => OutputCharset::Latin1,
        // Unknown-but-accepted names are rejected up front by
        // `xmlFindCharEncodingHandler`, so they never reach here; treat
        // anything else as full-Unicode and let the buffer transcode.
        _ => OutputCharset::Utf8,
    }
}

// ── xmlBuffer (32 bytes) ──────────────────────────────────────────────────

#[repr(C)]
pub struct xmlBuffer {
    pub content:    *mut c_char,  //  0
    pub use_:       c_uint,       //  8  (field `use` is a Rust keyword)
    pub size:       c_uint,       // 12
    pub alloc:      c_int,        // 16  (xmlBufferAllocationScheme)
    _pad_alloc:     u32,          // 20
    pub content_io: *mut c_char,  // 24
}
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlBuffer, content)    ==  0);
    assert!(offset_of!(xmlBuffer, use_)       ==  8);
    assert!(offset_of!(xmlBuffer, size)       == 12);
    assert!(offset_of!(xmlBuffer, content_io) == 24);
    assert!(std::mem::size_of::<xmlBuffer>() == 32);
};

// ── xmlOutputBuffer (56 bytes) ────────────────────────────────────────────

#[repr(C)]
pub struct xmlOutputBuffer {
    pub context:       *mut c_void,    //  0
    pub writecallback: *mut c_void,    //  8
    pub closecallback: *mut c_void,    // 16
    pub encoder:       *mut c_void,    // 24
    pub buffer:        *mut xmlBuffer, // 32
    pub conv:          *mut xmlBuffer, // 40
    pub written:       c_int,          // 48
    pub error:         c_int,          // 52
}
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlOutputBuffer, buffer)  == 32);
    assert!(offset_of!(xmlOutputBuffer, conv)    == 40);
    assert!(offset_of!(xmlOutputBuffer, written) == 48);
    assert!(offset_of!(xmlOutputBuffer, error)   == 52);
    assert!(std::mem::size_of::<xmlOutputBuffer>() == 56);
};

// ── allocations ───────────────────────────────────────────────────────────

/// `xmlBufferCreate()` — allocate an empty buffer.  Caller releases
/// via [`xmlBufferFree`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferCreate() -> *mut xmlBuffer {
    Box::into_raw(Box::new(xmlBuffer {
        content:    ptr::null_mut(),
        use_:       0,
        size:       0,
        alloc:      1, // XML_BUFFER_ALLOC_DOUBLEIT
        _pad_alloc: 0,
        content_io: ptr::null_mut(),
    }))
}

/// `xmlBufferFree` — release a buffer + its content.
///
/// Buffers built via [`xmlBufferCreateStatic`] are marked
/// `XML_BUFFER_ALLOC_IMMUTABLE` (alloc == 4); for those the wrapped
/// bytes are caller-owned, so we drop the wrapper only and leave the
/// content alone.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferFree(buf: *mut xmlBuffer) {
    if buf.is_null() { return; }
    // SAFETY: buf came from a Box allocation by us.
    let b = unsafe { Box::from_raw(buf) };
    const XML_BUFFER_ALLOC_IMMUTABLE: c_int = 4;
    if b.alloc == XML_BUFFER_ALLOC_IMMUTABLE {
        return;
    }
    if !b.content.is_null() && b.size > 0 {
        // Content was a Vec<u8> we leaked via Vec::into_raw_parts.
        unsafe {
            let _ = Vec::from_raw_parts(b.content as *mut u8, b.use_ as usize, b.size as usize);
        }
    }
}

/// `xmlBufferContent` — return the current bytes.  Pointer is owned
/// by the buffer; caller must NOT free.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferContent(buf: *const xmlBuffer) -> *const c_char {
    if buf.is_null() { return ptr::null(); }
    unsafe { (*buf).content }
}

/// `xmlBufferLength` — bytes in use (excluding any terminator).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferLength(buf: *const xmlBuffer) -> c_int {
    if buf.is_null() { return 0; }
    unsafe { (*buf).use_ as c_int }
}

/// `xmlBufContent(buf)` — accessor used by newer libxml2 APIs.
/// Same semantics as `xmlBufferContent`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufContent(buf: *const xmlBuffer) -> *const c_char {
    unsafe { xmlBufferContent(buf) }
}

/// `xmlBufUse(buf)` — same as `xmlBufferLength` for newer API.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufUse(buf: *const xmlBuffer) -> usize {
    if buf.is_null() { return 0; }
    unsafe { (*buf).use_ as usize }
}

/// `xmlBufferWriteChar(buf, str)` — append a NUL-terminated string.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferWriteChar(buf: *mut xmlBuffer, s: *const c_char) {
    if buf.is_null() || s.is_null() { return; }
    let bytes = unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes();
    unsafe { buffer_append(buf, bytes); }
}

/// `xmlBufferAdd(buf, str, len)` — append `len` bytes from `str`.
/// If `len < 0`, falls back to NUL-terminated copy of `str`.  Returns
/// 0 on success, -1 on error (NULL buffer / NULL str when len < 0).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferAdd(
    buf: *mut xmlBuffer,
    s:   *const c_char,
    len: c_int,
) -> c_int {
    if buf.is_null() {
        return -1;
    }
    if s.is_null() {
        return if len <= 0 { 0 } else { -1 };
    }
    let bytes: &[u8] = if len < 0 {
        unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes()
    } else {
        unsafe { std::slice::from_raw_parts(s as *const u8, len as usize) }
    };
    unsafe { buffer_append(buf, bytes); }
    0
}

/// `xmlBufferCat(buf, str)` — append NUL-terminated `xmlChar*`.
/// Synonym for [`xmlBufferWriteChar`] at this level; returns 0 on
/// success, -1 on NULL buffer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferCat(buf: *mut xmlBuffer, s: *const c_char) -> c_int {
    if buf.is_null() { return -1; }
    unsafe { xmlBufferWriteChar(buf, s); }
    0
}

/// `xmlBufferCCat(buf, str)` — append NUL-terminated C string.
/// libxml2 distinguishes `xmlChar*` vs `char*` at the type level but
/// both are byte-sequences underneath; behaviour is identical.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferCCat(buf: *mut xmlBuffer, s: *const c_char) -> c_int {
    unsafe { xmlBufferCat(buf, s) }
}

/// `xmlBufferCreateSize(size)` — allocate a buffer with `size` bytes
/// of pre-reserved capacity.  Pre-reservation is a hint; the buffer
/// grows on demand regardless.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferCreateSize(size: usize) -> *mut xmlBuffer {
    let buf = unsafe { xmlBufferCreate() };
    if buf.is_null() || size == 0 {
        return buf;
    }
    // Pre-allocate a Vec with the requested capacity (+1 for the
    // trailing NUL libxml2 buffers always carry).  All-safe Rust.
    let mut v: Vec<u8> = Vec::with_capacity(size + 1);
    v.push(0);
    let cap = v.capacity();
    let p   = v.as_mut_ptr();
    std::mem::forget(v);
    // SAFETY: caller asserts `buf` came from xmlBufferCreate, so it's
    // a live exclusive reference.
    let b: &mut xmlBuffer = unsafe { &mut *buf };
    b.content = p as *mut c_char;
    b.use_    = 0;
    b.size    = cap as c_uint;
    buf
}

/// `xmlBufferDetach(buf)` — take ownership of the buffer's content
/// pointer.  The returned `xmlChar*` belongs to the caller (who
/// releases via `xmlFree`); the buffer is reset to empty.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferDetach(buf: *mut xmlBuffer) -> *mut c_char {
    if buf.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `buf` came from xmlBufferCreate*, so the
    // pointer is a live, exclusive reference.
    let b: &mut xmlBuffer = unsafe { &mut *buf };
    if b.content.is_null() {
        // libxml2 returns an empty (NUL-only) heap allocation here.
        return crate::alloc::alloc_registered_cstring(b"");
    }
    // Copy current contents into a registered heap allocation that
    // can be freed by xmlFree.
    // SAFETY: `buffer_append` keeps `content`/`use_`/`size` in sync
    // with the backing Vec — `use_` is the live byte count.
    let bytes = unsafe {
        std::slice::from_raw_parts(b.content as *const u8, b.use_ as usize)
    };
    let out = crate::alloc::alloc_registered_cstring(bytes);
    // Reclaim the Vec that owns the buffer's storage.
    // SAFETY: same invariants — content was allocated as a Vec by
    // `buffer_append` with len=use_ and cap=size.
    unsafe {
        let _ = Vec::from_raw_parts(b.content as *mut u8, b.use_ as usize, b.size as usize);
    }
    b.content = ptr::null_mut();
    b.use_    = 0;
    b.size    = 0;
    out
}

/// `xmlBufferSetAllocationScheme(buf, scheme)` — set the growth
/// policy.  We store the value for ABI fidelity but don't honour
/// the choice — our Vec grows by doubling regardless.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferSetAllocationScheme(
    buf: *mut xmlBuffer,
    scheme: c_int,
) {
    if buf.is_null() { return; }
    unsafe { (*buf).alloc = scheme; }
}

pub(crate) unsafe fn buffer_append(buf: *mut xmlBuffer, data: &[u8]) {
    if data.is_empty() { return; }
    // SAFETY: caller asserts `buf` is a live xmlBuffer (its only
    // sources are xmlBufferCreate / xmlBufferCreateSize → Box::leak'd
    // by us).
    let b: &mut xmlBuffer = unsafe { &mut *buf };
    // Pull the existing Vec out of the raw pointer; extend; put back.
    // SAFETY: when content is non-null the (ptr, use_, size) triple
    // was always produced by Vec::into_raw_parts via `forget(v)` in a
    // sibling buffer fn — round-trip back through from_raw_parts is
    // sound (`size` is the original capacity).  This is the *only*
    // unsafe op in this function.
    let mut v: Vec<u8> = if b.content.is_null() {
        Vec::new()
    } else {
        unsafe {
            Vec::from_raw_parts(b.content as *mut u8, b.use_ as usize, b.size as usize)
        }
    };
    v.extend_from_slice(data);
    v.reserve(1); // ensure room for trailing NUL if needed
    let len = v.len();
    let cap = v.capacity();
    let p = v.as_mut_ptr();
    // Trailing NUL — libxml2's buffers are NUL-terminated.
    if cap > len {
        // SAFETY: `cap > len` guarantees `p.add(len)` is in-bounds of
        // the allocation Vec gave us; the byte is uninitialised but
        // we're writing a single byte (no read).
        unsafe { *p.add(len) = 0; }
    }
    std::mem::forget(v);
    b.content = p as *mut c_char;
    b.use_  = len as c_uint;
    b.size  = cap as c_uint;
}

/// Empty an `xmlBuffer` in place — drop its used length to zero (keeping
/// the allocation for reuse) and re-terminate.  Used after a flush has
/// shipped the staged bytes to their destination.
unsafe fn buffer_clear(buf: *mut xmlBuffer) {
    if buf.is_null() { return; }
    let b: &mut xmlBuffer = unsafe { &mut *buf };
    b.use_ = 0;
    if !b.content.is_null() {
        // SAFETY: content points at a live allocation of `size` bytes.
        unsafe { *b.content = 0; }
    }
}

/// Ship a user-callback output buffer's staged bytes to its write
/// callback and empty the staging buffers.  Returns `false` on a
/// callback error (and sets `o.error`); a no-op when nothing is staged.
///
/// libxml2 buffers writes through an IO callback rather than invoking it
/// per `xmlOutputBufferWrite`: the callback fires on `xmlOutputBufferFlush`
/// and at close.  lxml's `xmlfile(..., buffered=True)` relies on exactly
/// that — the underlying file stays empty until `xf.flush()`.
///
/// # Safety
/// `o.writecallback` must be a real [`IoWriteCb`] (checked by the caller
/// via [`is_user_write_cb`]); `o.conv` / `o.buffer` are live or NULL.
unsafe fn flush_user_callback(o: &mut xmlOutputBuffer) -> bool {
    // The encoder path stages transcoded bytes in `conv` and raw UTF-8 in
    // `buffer`; the consumer wants the transcoded copy when present.
    let src = if !o.conv.is_null() { o.conv } else { o.buffer };
    if src.is_null() { return true; }
    let b = unsafe { &*src };
    if b.content.is_null() || b.use_ == 0 { return true; }
    let bytes = unsafe { std::slice::from_raw_parts(b.content as *const u8, b.use_ as usize) };
    let f: IoWriteCb = unsafe { std::mem::transmute(o.writecallback) };
    let written = unsafe { f(o.context, bytes.as_ptr() as *const c_char, bytes.len() as c_int) };
    unsafe { buffer_clear(o.conv); buffer_clear(o.buffer); }
    if written < 0 {
        o.error = 1;
        return false;
    }
    true
}

// ── xmlOutputBuffer ───────────────────────────────────────────────────────

/// `xmlAllocOutputBuffer(encoder)` — alloc an empty in-memory output
/// buffer.  `encoder` is accepted but ignored (we always emit UTF-8).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlAllocOutputBuffer(
    encoder: *mut c_void,
) -> *mut xmlOutputBuffer {
    let buffer = unsafe { xmlBufferCreate() };
    Box::into_raw(Box::new(xmlOutputBuffer {
        context:       ptr::null_mut(),
        writecallback: ptr::null_mut(),
        closecallback: ptr::null_mut(),
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// Map a writer/reader target to a local filesystem path, mirroring the
/// two-step handling libxml2 applies to a filename:
///
/// 1. `xmlOutputBufferCreateFilename` / `xmlParserInputBufferCreateFilename`
///    percent-**unescape** the whole string when it parses as a URI with
///    no scheme or the `file` scheme (see
///    [`crate::uri::filename_should_unescape`]).
/// 2. The file-open callback (`xmlFileOpenW`) then strips a `file://`
///    scheme from the result, so both `file:///abs` and
///    `file://localhost/abs` resolve to `/abs`.
///
/// Doing the unescape before the strip matters: a bare path such as
/// `dir/a%2520b.xml` (how lxml escapes a literal `%` to survive the
/// round-trip) is decoded to `dir/a%20b.xml`, while a name that isn't a
/// valid URI — an embedded space, raw non-ASCII — is used verbatim.
pub(crate) fn local_path_from_file_uri(uri: &str) -> std::borrow::Cow<'_, str> {
    if crate::uri::filename_should_unescape(uri) {
        // Decode the whole string first, then strip the scheme off the
        // decoded path (libxml2's order — unescape, then open).
        let decoded = percent_decode(uri).into_owned();
        std::borrow::Cow::Owned(strip_file_scheme(&decoded).to_string())
    } else {
        // Not a valid URI: open verbatim, but the open callback still
        // strips a literal `file://` prefix.
        std::borrow::Cow::Borrowed(strip_file_scheme(uri))
    }
}

/// Strip a `file://` (optionally `file://localhost`) scheme prefix,
/// leaving the local filesystem path.  Anything without the scheme is
/// returned unchanged.
fn strip_file_scheme(s: &str) -> &str {
    if let Some(rest) = s.strip_prefix("file://localhost").filter(|r| r.starts_with('/')) {
        rest
    } else if let Some(rest) = s.strip_prefix("file://") {
        rest
    } else {
        s
    }
}

/// Decode `%XX` escapes in a URI path component.  Borrows when there is
/// nothing to decode; on a malformed escape sequence the byte is copied
/// through verbatim.  Falls back to the original string if the decoded
/// bytes aren't valid UTF-8.
fn percent_decode(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('%') {
        return std::borrow::Cow::Borrowed(s);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    match String::from_utf8(out) {
        Ok(decoded) => std::borrow::Cow::Owned(decoded),
        Err(_)      => std::borrow::Cow::Borrowed(s),
    }
}

/// `xmlOutputBufferCreateFilename(uri, encoder, compression)` —
/// create an output buffer that writes to a local file.  The file
/// is opened immediately; the contents accumulate in-memory and are
/// flushed to disk on [`xmlOutputBufferClose`].  `compression` is
/// ignored (we don't support gzip output).
///
/// Returns NULL on NULL filename or open failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateFilename(
    uri:          *const c_char,
    encoder:      *mut c_void,
    _compression: c_int,
) -> *mut xmlOutputBuffer {
    if uri.is_null() { return ptr::null_mut(); }
    let path = match unsafe { std::ffi::CStr::from_ptr(uri) }.to_str() {
        Ok(s) => local_path_from_file_uri(s).to_string(),
        Err(_) => return ptr::null_mut(),
    };
    // Validate the file is openable up front — if the path is bad
    // libxml2 returns NULL here so consumers detect the error early.
    if std::fs::File::create(&path).is_err() {
        return ptr::null_mut();
    }
    // Stash the path in `context` so the close handler knows where
    // to flush.  We Box::leak a CString to keep the bytes alive for
    // the buffer's lifetime; xmlOutputBufferClose recovers and drops it.
    let path_cs = std::ffi::CString::new(path).unwrap_or_default();
    let path_ptr = path_cs.into_raw();
    let buffer = unsafe { xmlBufferCreate() };
    let sentinel = &FILE_FLUSH_SENTINEL as *const u8 as *mut c_void;
    Box::into_raw(Box::new(xmlOutputBuffer {
        context:       path_ptr as *mut c_void,
        writecallback: sentinel,
        closecallback: sentinel,
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// Sentinel value stored in `writecallback`/`closecallback` to mark
/// a buffer as file-backed (so [`xmlOutputBufferClose`] knows to
/// flush before freeing).  The address of a static is unique within
/// the process and won't collide with any user-supplied callback.
static FILE_FLUSH_SENTINEL: u8 = 0;

/// `xmlOutputBufferCreateIO(iowrite, ioclose, context, encoder)` —
/// create an output buffer wired to caller-supplied I/O callbacks.
///
/// The buffer routes every [`xmlOutputBufferWrite`] (and friends)
/// straight through `iowrite(context, buf, len)` — no in-memory
/// staging, no compression.  On [`xmlOutputBufferClose`] the final
/// `ioclose(context)` runs before the buffer struct is freed.
///
/// The libxml2 contract: callbacks return bytes-written on success,
/// negative on error.  An error short-circuits the buffer's error
/// flag and propagates upward as -1 from subsequent writes.
///
/// Returns NULL only on NULL `iowrite` (no writer = pointless buffer)
/// — the rest of the args may be NULL (callback decides what to do
/// with a NULL context, etc.).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateIO(
    iowrite:  *mut c_void,
    ioclose:  *mut c_void,
    context:  *mut c_void,
    encoder:  *mut c_void,
) -> *mut xmlOutputBuffer {
    if iowrite.is_null() {
        return ptr::null_mut();
    }
    // Still allocate the in-memory buffer slot — lxml inspects it
    // even when unused (peek at offsets 32-40), and keeping it
    // non-null avoids surprise NULL-deref.
    let buffer = unsafe { xmlBufferCreate() };
    Box::into_raw(Box::new(xmlOutputBuffer {
        context,
        writecallback: iowrite,
        closecallback: ioclose,
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// Signature for caller-supplied write callbacks.
type IoWriteCb = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
/// Signature for caller-supplied close callbacks.
type IoCloseCb = unsafe extern "C" fn(*mut c_void) -> c_int;

/// Helper: is this writecallback slot a user-supplied callback (vs
/// the file-flush sentinel or NULL)?
#[inline]
fn is_user_write_cb(cb: *mut c_void) -> bool {
    if cb.is_null() { return false; }
    let sentinels: [*mut c_void; 4] = [
        &FILE_FLUSH_SENTINEL   as *const u8 as *mut c_void,
        &FD_FLUSH_SENTINEL     as *const u8 as *mut c_void,
        &STDIO_FLUSH_SENTINEL  as *const u8 as *mut c_void,
        &BUFFER_FLUSH_SENTINEL as *const u8 as *mut c_void,
    ];
    !sentinels.contains(&cb)
}

/// `xmlOutputBufferClose` — release the buffer + any encoded copy.
/// Returns total bytes written (libxml2's contract).  For file-backed
/// buffers (created via [`xmlOutputBufferCreateFilename`]), flushes
/// the accumulated bytes to disk before freeing.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferClose(out: *mut xmlOutputBuffer) -> c_int {
    if out.is_null() { return -1; }
    let mut o = unsafe { Box::from_raw(out) };
    let written = o.written;

    // File-backed buffers stash the path CString pointer in `context`
    // and use a sentinel callback to mark themselves.
    let is_file_backed = o.writecallback == &FILE_FLUSH_SENTINEL as *const u8 as *mut c_void;
    let is_fd_backed   = o.writecallback == &FD_FLUSH_SENTINEL    as *const u8 as *mut c_void;
    let is_stdio_backed= o.writecallback == &STDIO_FLUSH_SENTINEL as *const u8 as *mut c_void;
    // Buffer-backed: caller owns the xmlBuffer (`xmlOutputBufferCreateBuffer`).
    // We must NOT free it; the caller will via xmlBufferFree.
    let is_buffer_backed = o.writecallback == &BUFFER_FLUSH_SENTINEL as *const u8 as *mut c_void;
    // When an encoder is active the transcoded bytes live in `conv`;
    // that — not the raw UTF-8 in `buffer` — is what reaches the file.
    let src_buf = if !o.conv.is_null() { o.conv } else { o.buffer };
    if is_file_backed && !o.context.is_null() {
        // SAFETY: context was into_raw'd from a CString in
        // xmlOutputBufferCreateFilename; we reclaim it now.
        let path_cs = unsafe { std::ffi::CString::from_raw(o.context as *mut c_char) };
        if let Ok(path) = path_cs.to_str() {
            if !src_buf.is_null() {
                // SAFETY: src_buf is a live xmlBuffer with our
                // Vec-of-bytes content; read its bytes and write to file.
                let buf = unsafe { &*src_buf };
                if !buf.content.is_null() && buf.use_ > 0 {
                    let bytes = unsafe {
                        std::slice::from_raw_parts(buf.content as *const u8, buf.use_ as usize)
                    };
                    let _ = std::fs::write(path, bytes);
                }
            }
        }
        // path_cs dropped here.
    } else if is_fd_backed && !src_buf.is_null() {
        // fd-backed: context holds the raw fd as a sentinel-tagged
        // integer.  Borrow it as a File (without taking ownership) and
        // write the accumulated bytes.
        let fd = o.context as isize as c_int;
        let buf = unsafe { &*src_buf };
        if !buf.content.is_null() && buf.use_ > 0 {
            if let Some(mut f) = crate::rawfd::borrow_fd(fd) {
                // SAFETY: content/use_ describe a valid byte range owned
                // by the buffer for this call.
                let bytes = unsafe {
                    std::slice::from_raw_parts(buf.content as *const u8, buf.use_ as usize)
                };
                // Errors are ignored per libxml2's "log + continue"
                // output-callback contract.
                let _ = f.write_all(bytes);
            }
        }
    } else if is_stdio_backed && !o.context.is_null() && !src_buf.is_null() {
        // FILE*-backed: context holds the FILE* directly.
        let buf = unsafe { &*src_buf };
        if !buf.content.is_null() && buf.use_ > 0 {
            unsafe extern "C" {
                fn fwrite(
                    ptr: *const c_void, sz: usize, n: usize, f: *mut c_void,
                ) -> usize;
            }
            // SAFETY: caller asserted FILE* is open when handed in.
            unsafe {
                let _ = fwrite(buf.content as *const c_void, 1, buf.use_ as usize, o.context);
            }
        }
        // libxml2's contract: xmlOutputBufferCreateFile does NOT
        // own the FILE*; the caller fclose()s it.  So we don't close
        // it here.
    } else if is_user_write_cb(o.writecallback) {
        // User-callback buffer: ship any bytes still staged from writes
        // since the last flush, then (if a close handler was registered)
        // invoke it so the caller can release whatever the context
        // referenced (Python file-like wrapper, socket, etc.).  Errors
        // fold into our return value, mirroring libxml2.
        unsafe { flush_user_callback(&mut o); }
        if is_user_write_cb(o.closecallback) {
            // SAFETY: closecallback was stored as a function pointer in
            // xmlOutputBufferCreateIO; the caller asserted the canonical
            // signature.
            let f: IoCloseCb = unsafe { std::mem::transmute(o.closecallback) };
            let rc = unsafe { f(o.context) };
            if rc < 0 && o.error == 0 {
                // Surface the close error if no earlier write error.
            }
        }
    }

    if !is_buffer_backed && !o.buffer.is_null() {
        unsafe { xmlBufferFree(o.buffer); }
    }
    if !o.conv.is_null() { unsafe { xmlBufferFree(o.conv); } }
    written
}

/// `xmlOutputBufferFlush` — ship any staged bytes to the destination.
/// For a user IO-callback buffer this fires the write callback with the
/// pending bytes (libxml2's buffered-write contract, which lxml's
/// `xmlfile(buffered=True)` depends on).  In-memory and file/fd/stdio
/// buffers stay staged until close.  Returns the number of bytes
/// flushed, or -1 on a callback error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferFlush(out: *mut xmlOutputBuffer) -> c_int {
    if out.is_null() { return -1; }
    let o = unsafe { &mut *out };
    if !is_user_write_cb(o.writecallback) {
        return 0;
    }
    let pending = unsafe {
        let src = if !o.conv.is_null() { o.conv } else { o.buffer };
        if src.is_null() { 0 } else { (*src).use_ as c_int }
    };
    if !unsafe { flush_user_callback(o) } {
        return -1;
    }
    pending
}

/// `xmlOutputBufferWrite(out, len, buf)` — append `len` bytes from
/// `buf` to the output buffer.  Returns number of bytes written, or
/// -1 on error.
///
/// When the buffer was constructed with a non-NULL `encoder`, the
/// input bytes (UTF-8) are transcoded to the encoder's target
/// charset before being written.  Currently honours UTF-16 (LE/BE
/// variants); other named encodings pass through verbatim.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferWrite(
    out: *mut xmlOutputBuffer,
    len: c_int,
    buf: *const c_char,
) -> c_int {
    if out.is_null() || buf.is_null() || len <= 0 {
        return 0;
    }
    let o = unsafe { &mut *out };
    let input_bytes = unsafe { std::slice::from_raw_parts(buf as *const u8, len as usize) };
    // Compute the transcoded bytes (if any).  Real libxml2 stores
    // raw UTF-8 in `buffer` and the transcoded output in `conv`;
    // consumers that want encoded bytes (lxml's `bytes(res)`, the
    // user IO callback) read `conv`.  We follow the same split.
    let mut encoded: Option<Vec<u8>> =
        unsafe { transcode_for_encoder(o.encoder, input_bytes) };
    // UTF-16 (no LE/BE suffix) consumers expect a BOM at the
    // start of the stream — emit once on the first write.
    if o.written == 0 && encoded.is_some() {
        if let Some(name) = unsafe { encoder_name(o.encoder) } {
            // A bare endianness-less name (UTF-16 / UTF-32) carries its
            // byte order in a leading BOM; we default both to LE.
            let bom: &[u8] = match name.as_str() {
                "UTF-16" => &[0xFF, 0xFE],
                "UTF-32" => &[0xFF, 0xFE, 0x00, 0x00],
                _ => &[],
            };
            if !bom.is_empty() {
                let mut v = bom.to_vec();
                v.extend_from_slice(&encoded.take().unwrap());
                encoded = Some(v);
            }
        }
    }
    // Pick what gets fed to the consumer:
    //   * `conv_bytes` — the transcoded output (or raw input when
    //     no encoder is set).  Goes into buf.conv AND the user
    //     callback if one is registered.
    //   * `buffer` (raw UTF-8 input) is *also* appended to
    //     buf.buffer so consumers that want pre-transcode bytes
    //     can still read it.
    let conv_bytes: &[u8] = match &encoded {
        Some(v) => v,
        None    => input_bytes,
    };
    // Stage raw UTF-8 in buf.buffer, transcoded in buf.conv (when an
    // encoder is active).  In-memory consumers (lxml's `bytes(res)`)
    // read buf.conv first; user-callback destinations are drained from
    // these buffers on flush/close so the callback only fires when
    // libxml2 would (see [`flush_user_callback`]).
    if encoded.is_some() {
        // Allocate buf.conv lazily on first use.
        if o.conv.is_null() {
            o.conv = unsafe { xmlBufferCreate() };
        }
        unsafe { buffer_append(o.conv, conv_bytes); }
        unsafe { buffer_append(o.buffer, input_bytes); }
    } else {
        unsafe { buffer_append(o.buffer, input_bytes); }
    }
    o.written = o.written.saturating_add(len);
    len
}

/// If `encoder` is non-NULL and identifies a transcoding target we
/// support, return the transcoded bytes.  Otherwise None — caller
/// writes the input unchanged.
///
/// SAFETY: `encoder`, when non-NULL, must point at a valid
/// `xmlCharEncodingHandler` allocated by
/// [`xmlFindCharEncodingHandler`] (which is how libxslt obtains
/// the pointer it stores on the buffer).
/// Read the `name` field of an xmlCharEncodingHandler.  Returns
/// None on NULL pointer or non-UTF-8 name; an uppercased copy of
/// the name otherwise.
unsafe fn encoder_name(encoder: *mut c_void) -> Option<String> {
    if encoder.is_null() { return None; }
    let h = unsafe { &*(encoder as *const xmlCharEncodingHandler) };
    if h.name.is_null() { return None; }
    unsafe { std::ffi::CStr::from_ptr(h.name) }.to_str().ok()
        .map(|s| s.to_ascii_uppercase())
}

unsafe fn transcode_for_encoder(
    encoder: *mut c_void,
    input:   &[u8],
) -> Option<Vec<u8>> {
    if encoder.is_null() { return None; }
    let h = unsafe { &*(encoder as *const xmlCharEncodingHandler) };
    if h.name.is_null() { return None; }
    let name = match unsafe { std::ffi::CStr::from_ptr(h.name) }.to_str() {
        Ok(s) => s.to_ascii_uppercase(),
        Err(_) => return None,
    };
    // Common case: input is UTF-8 (libxml2 / libxslt's internal
    // form).  Decode to chars, re-encode to target.
    let text = match std::str::from_utf8(input) {
        Ok(s) => s,
        Err(_) => return None,
    };
    match name.as_str() {
        "UTF-8" => None, // pass-through — represents every scalar
        // ASCII can't represent non-ASCII scalars; libxml2 emits numeric
        // character references for them.  lxml's incremental writer
        // streams UTF-8 attribute/text bytes straight through this
        // encoder (bypassing our serializer's own charset escaping), so
        // the escaping has to happen here.  Always return `Some` — even
        // for pure-ASCII input — so the buffer's `conv` copy stays a
        // complete record across a mix of ASCII-only and escaped writes.
        "ASCII" | "US-ASCII" => {
            let mut out = Vec::with_capacity(input.len());
            for c in text.chars() {
                if c.is_ascii() {
                    out.push(c as u8);
                } else {
                    out.extend_from_slice(format!("&#{};", c as u32).as_bytes());
                }
            }
            Some(out)
        }
        "UTF-16" | "UTF-16LE" => {
            let mut out = Vec::with_capacity(input.len() * 2);
            for u in text.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            Some(out)
        }
        "UTF-16BE" => {
            let mut out = Vec::with_capacity(input.len() * 2);
            for u in text.encode_utf16() {
                out.extend_from_slice(&u.to_be_bytes());
            }
            Some(out)
        }
        // UTF-32 emits one 4-byte code unit per Unicode scalar value.
        // "UTF-32" (no endianness suffix) defaults to little-endian, with
        // a BOM prepended on the first write (see xmlOutputBufferWrite).
        "UTF-32" | "UTF-32LE" | "UCS-4LE" => {
            let mut out = Vec::with_capacity(input.len() * 4);
            for c in text.chars() {
                out.extend_from_slice(&(c as u32).to_le_bytes());
            }
            Some(out)
        }
        "UTF-32BE" | "UCS-4BE" => {
            let mut out = Vec::with_capacity(input.len() * 4);
            for c in text.chars() {
                out.extend_from_slice(&(c as u32).to_be_bytes());
            }
            Some(out)
        }
        // ISO-8859-1: one byte per code point ≤ 0xFF.  Code points above
        // that are escaped as numeric character references by the
        // serializer before they reach here (the output charset is
        // Latin1); a stray one substitutes to '?' as libxml2 does.
        "ISO-8859-1" | "ISO8859-1" | "ISO_8859-1" | "LATIN1" | "LATIN-1"
        | "L1" | "CP819" | "IBM819" => {
            let mut out = Vec::with_capacity(text.len());
            for c in text.chars() {
                let cp = c as u32;
                out.push(if cp <= 0xFF { cp as u8 } else { b'?' });
            }
            Some(out)
        }
        _ => None,
    }
}

/// `xmlOutputBufferWriteString(out, str)` — append a NUL-terminated
/// string.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferWriteString(
    out: *mut xmlOutputBuffer,
    s:   *const c_char,
) -> c_int {
    if s.is_null() { return 0; }
    let len = unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes().len() as c_int;
    unsafe { xmlOutputBufferWrite(out, len, s) }
}

/// `xmlOutputBufferWriteEscape(out, str, escape)` — append `str`
/// with XML attribute/text-content escaping.  `escape` is a callback
/// the caller can supply; in v0.1 we ignore it and apply default
/// escaping (the same rules libxml2's default escaper uses).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferWriteEscape(
    out:    *mut xmlOutputBuffer,
    s:      *const c_char,
    _esc:   *mut c_void,
) -> c_int {
    if out.is_null() || s.is_null() { return 0; }
    let bytes = unsafe { std::ffi::CStr::from_ptr(s) }.to_bytes();
    // Escape &, <, > (text content rules).  Attribute escaping
    // additionally handles "; we don't distinguish in v0.1 — the
    // result is correct for both contexts because attribute output
    // also escapes those three.
    let mut escaped = Vec::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'&'  => escaped.extend_from_slice(b"&amp;"),
            b'<'  => escaped.extend_from_slice(b"&lt;"),
            b'>'  => escaped.extend_from_slice(b"&gt;"),
            b'"'  => escaped.extend_from_slice(b"&quot;"),
            _     => escaped.push(b),
        }
    }
    // Re-route through xmlOutputBufferWrite so the user-callback /
    // file-sentinel / in-memory dispatch is centralised in one
    // place (no need to duplicate it here).
    unsafe {
        xmlOutputBufferWrite(out, escaped.len() as c_int, escaped.as_ptr() as *const c_char)
    }
}

// ── xmlOutputBufferCreateFd / CreateFile ──────────────────────────────────

/// `xmlOutputBufferCreateFd(fd, encoder)` — output buffer that writes
/// to a Unix file descriptor on close.  v0.1 stores the fd in the
/// `context` slot (cast through a sentinel) and accumulates writes
/// in-memory; `xmlOutputBufferClose` flushes the buffer to the fd.
/// `encoder` is ignored.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateFd(
    fd:      c_int,
    encoder: *mut c_void,
) -> *mut xmlOutputBuffer {
    if fd < 0 { return ptr::null_mut(); }
    let buffer = unsafe { xmlBufferCreate() };
    // We encode the fd as a usize-pointer (with the FD_FLUSH_SENTINEL
    // marker on the close callback so xmlOutputBufferClose knows to
    // write(fd, ...) the accumulated bytes).
    let sentinel = &FD_FLUSH_SENTINEL as *const u8 as *mut c_void;
    Box::into_raw(Box::new(xmlOutputBuffer {
        context:       fd as isize as *mut c_void,
        writecallback: sentinel,
        closecallback: sentinel,
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// `xmlOutputBufferCreateFile(file, encoder)` — output buffer that
/// writes to a stdio `FILE*` on close.  Identical shape to the fd
/// variant; we use a different sentinel so `xmlOutputBufferClose`
/// dispatches to `fwrite` instead of `write`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateFile(
    file:    *mut c_void,
    encoder: *mut c_void,
) -> *mut xmlOutputBuffer {
    if file.is_null() { return ptr::null_mut(); }
    let buffer = unsafe { xmlBufferCreate() };
    let sentinel = &STDIO_FLUSH_SENTINEL as *const u8 as *mut c_void;
    Box::into_raw(Box::new(xmlOutputBuffer {
        context:       file,
        writecallback: sentinel,
        closecallback: sentinel,
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// `xmlOutputBufferCreateBuffer(buffer, encoder)` — wrap an existing
/// `xmlBuffer` as an `xmlOutputBuffer`.  Writes accumulate directly
/// in `buffer`.  This is the canonical setup for PHP's
/// `XMLWriter::openMemory()` (create a buffer, wrap it, hand the
/// wrapped output buffer to `xmlNewTextWriter`).
///
/// The caller retains ownership of `buffer` — `xmlOutputBufferClose`
/// will NOT free it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferCreateBuffer(
    buffer:  *mut xmlBuffer,
    encoder: *mut c_void,
) -> *mut xmlOutputBuffer {
    if buffer.is_null() { return ptr::null_mut(); }
    let sentinel = &BUFFER_FLUSH_SENTINEL as *const u8 as *mut c_void;
    Box::into_raw(Box::new(xmlOutputBuffer {
        context:       ptr::null_mut(),
        writecallback: sentinel,   // no-op on flush
        closecallback: sentinel,   // no-op on close (caller owns buffer)
        encoder,
        buffer,
        conv:          ptr::null_mut(),
        written:       0,
        error:         0,
    }))
}

/// `xmlOutputBufferGetContent(out)` — return a pointer to the bytes
/// currently in the output buffer.  Pointer is owned by the buffer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferGetContent(out: *const xmlOutputBuffer) -> *const c_char {
    if out.is_null() { return ptr::null(); }
    let buf = unsafe { (*out).buffer };
    if buf.is_null() { return ptr::null(); }
    unsafe { (*buf).content }
}

/// `xmlOutputBufferGetSize(out)` — bytes used in the output buffer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOutputBufferGetSize(out: *const xmlOutputBuffer) -> usize {
    if out.is_null() { return 0; }
    let buf = unsafe { (*out).buffer };
    if buf.is_null() { return 0; }
    unsafe { (*buf).use_ as usize }
}

/// Sentinel markers — addresses-of-globals we set into the
/// callback slots to disambiguate the "where do we flush?" path.
/// Real libxml2 stores actual function pointers; we store these
/// markers and dispatch on them inside [`xmlOutputBufferClose`].
static FD_FLUSH_SENTINEL: u8     = 0;
static STDIO_FLUSH_SENTINEL: u8  = 0;
static BUFFER_FLUSH_SENTINEL: u8 = 0;

// ── xmlCharEncodingHandler bridge ─────────────────────────────────────────

/// A modern (libxml2 2.14+) character-encoding conversion function.
///
/// Transcodes `inlen` bytes at `input` into the `outlen`-byte buffer at
/// `out`.  On return `*inlen` is the number of bytes consumed and
/// `*outlen` the number produced.  `flush` is non-zero when no further
/// input will follow.  Returns an `xmlCharEncError` (0 = success,
/// negative on error — see [`XML_ENC_ERR_SPACE`]).
pub type XmlCharEncConvFunc = unsafe extern "C" fn(
    vctxt:  *mut c_void,
    out:    *mut u8,
    outlen: *mut c_int,
    input:  *const u8,
    inlen:  *mut c_int,
    flush:  c_int,
) -> c_int;

/// Destructor for a custom converter's context, invoked from
/// [`xmlCharEncCloseFunc`] when the owning handler is freed.
pub type XmlCharEncConvCtxtDtor = unsafe extern "C" fn(vctxt: *mut c_void);

/// `xmlCharEncError` — not enough room in the output buffer; the caller
/// grows it and calls the conversion function again.
pub const XML_ENC_ERR_SPACE: c_int = -3;

/// libxml2 `xmlCharEncodingHandler` — byte-exact mirror of the modern
/// (2.14+) layout.  `name` (offset 0) is the only field older consumers
/// like lxml read; built-in pass-through handlers leave the converter
/// fields NULL.  Custom handlers built via [`xmlCharEncNewCustomHandler`]
/// carry the caller's conversion functions and per-direction contexts,
/// which the parser drives to transcode input into UTF-8.  Total 56 bytes.
#[repr(C)]
pub struct xmlCharEncodingHandler {
    pub name:        *mut c_char,                    // 0
    pub input:       Option<XmlCharEncConvFunc>,     // 8
    pub output:      Option<XmlCharEncConvFunc>,     // 16
    pub input_ctxt:  *mut c_void,                    // 24
    pub output_ctxt: *mut c_void,                    // 32
    pub ctxt_dtor:   Option<XmlCharEncConvCtxtDtor>, // 40
    pub flags:       c_int,                          // 48
}

/// `xmlFindCharEncodingHandler(name)` — look up a handler by name.
///
/// We don't actually transcode (the output buffer always writes the
/// UTF-8 source bytes verbatim), but lxml's serializer raises
/// `LookupError("unknown encoding: '<name>'")` if this returns NULL.
/// So we allocate a real-shaped handler struct whose `name` field
/// points to a strdup of the requested name; lxml stores it on the
/// output buffer and later frees it via `xmlCharEncCloseFunc`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFindCharEncodingHandler(
    name: *const c_char,
) -> *mut xmlCharEncodingHandler {
    if name.is_null() { return std::ptr::null_mut(); }
    // SAFETY: caller asserts name is null-terminated.
    let s = match unsafe { std::ffi::CStr::from_ptr(name) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    // Reject names that aren't valid charset tokens (a stray space, etc.).
    // libxml2 returns NULL for unrecognised encodings, which lxml turns
    // into `LookupError("unknown encoding: ...")` — see its serializer.
    let s = s.trim();
    if s.is_empty()
        || !s.bytes().all(|b| b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b':' | b'+' | b'(' | b')'))
    {
        return std::ptr::null_mut();
    }
    let cname = match std::ffi::CString::new(s) {
        Ok(c)  => c.into_raw(),
        Err(_) => return std::ptr::null_mut(),
    };
    Box::into_raw(Box::new(xmlCharEncodingHandler {
        name:        cname,
        input:       None,
        output:      None,
        input_ctxt:  std::ptr::null_mut(),
        output_ctxt: std::ptr::null_mut(),
        ctxt_dtor:   None,
        flags:       0,
    }))
}

/// `xmlGetCharEncodingHandler(enc)` — look up a handler by enum.
///
/// We return NULL so consumers like lxml's `_find_PyUCS4EncodingName`
/// fall through to `xmlGetCharEncodingName(enc)`, which DOES return
/// a real `'static` C string.  Returning a fake handle here with
/// a NULL `name` would otherwise leave lxml unable to determine the
/// encoding name.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetCharEncodingHandler(_enc: c_int) -> *mut xmlCharEncodingHandler {
    std::ptr::null_mut()
}

// ── modern (libxml2 2.14+) char-encoding-handler factory API ──────────────
//
// These return `xmlParserErrors` (an int) and deliver the handler via an
// out-pointer, replacing the older `xml{Find,Get}CharEncodingHandler`
// return-the-pointer forms.  They delegate to `xmlFindCharEncodingHandler`,
// which allocates a name-carrying handler (the shim does not transcode in
// the handler itself).  `XML_ERR_OK` is 0; `XML_ERR_UNSUPPORTED_ENCODING`
// is 32.

/// libxml2 `xmlOpenCharEncodingHandler(name, output, out)` — modern
/// replacement for [`xmlFindCharEncodingHandler`].  Writes a handler for
/// `name` through `*out` and returns 0; writes NULL and returns 32 when
/// `name` is NULL/unresolvable.  `output` (encode vs decode direction)
/// does not change which handler is returned.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlOpenCharEncodingHandler(
    name:    *const c_char,
    _output: c_int,
    out:     *mut *mut xmlCharEncodingHandler,
) -> c_int {
    if out.is_null() {
        return 32;
    }
    let h = unsafe { xmlFindCharEncodingHandler(name) };
    unsafe { *out = h; }
    if h.is_null() { 32 } else { 0 }
}

/// libxml2 `xmlLookupCharEncodingHandler(enc, out)` — modern replacement
/// for [`xmlGetCharEncodingHandler`].  Encodings needing no conversion
/// (NONE = 0, UTF-8 = 1) yield `*out = NULL` with status 0; a negative
/// `enc` is the error sentinel (status 32).  Otherwise the enum is mapped
/// to a name via `xmlGetCharEncodingName` and a handler is returned.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLookupCharEncodingHandler(
    enc: c_int,
    out: *mut *mut xmlCharEncodingHandler,
) -> c_int {
    if out.is_null() {
        return 32;
    }
    if enc <= 1 {
        unsafe { *out = std::ptr::null_mut(); }
        return if enc < 0 { 32 } else { 0 };
    }
    let name = crate::encoding::xmlGetCharEncodingName(enc);
    if name.is_null() {
        unsafe { *out = std::ptr::null_mut(); }
        return 32;
    }
    let h = unsafe { xmlFindCharEncodingHandler(name) };
    unsafe { *out = h; }
    if h.is_null() { 32 } else { 0 }
}

/// libxml2 `xmlCreateCharEncodingHandler(name, flags, impl, vctxt, out)`
/// — the most general factory.  A non-NULL `impl` is a caller-supplied
/// converter factory: it is invoked as `impl(vctxt, name, flags, out)` and
/// its result returned, letting the caller obtain a custom transcoder
/// (e.g. ICU) for `name`.  With a NULL `impl` it resolves the built-in
/// handler for `name`.  Returns `XML_ERR_OK` (0) on success;
/// `XML_ERR_UNSUPPORTED_ENCODING` (32) when no handler can be produced.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreateCharEncodingHandler(
    name:   *const c_char,
    flags:  c_int,
    imp:    Option<crate::parsectx::XmlCharEncConvImpl>,
    vctxt:  *mut c_void,
    out:    *mut *mut xmlCharEncodingHandler,
) -> c_int {
    if out.is_null() {
        return 32;
    }
    if let Some(factory) = imp {
        return unsafe { factory(vctxt, name, flags, out) };
    }
    let h = unsafe { xmlFindCharEncodingHandler(name) };
    unsafe { *out = h; }
    if h.is_null() { 32 } else { 0 }
}

/// `xmlCharEncInFunc(handler, out, in)` — transcode bytes from `in`
/// into `out` using the handler's input-direction converter.
/// Returns the byte count appended to `out` on success, a negative
/// value on error.
///
/// Our handlers don't carry transcode functions (see
/// [`xmlFindCharEncodingHandler`]).  When the requested encoding is
/// UTF-8 / ASCII (already our internal form) or unknown, we pass the
/// input bytes through verbatim — matching libxml2's "identity
/// converter" behaviour on the same input.  For genuinely different
/// encodings we route through the same conversion table the output
/// buffer's write path uses (UTF-16 LE/BE today).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharEncInFunc(
    handler: *mut xmlCharEncodingHandler,
    out:     *mut xmlBuffer,
    input:   *mut xmlBuffer,
) -> c_int {
    unsafe { transcode_buffer(handler, out, input, /*outbound=*/ false) }
}

/// `xmlCharEncOutFunc(handler, out, in)` — output-direction
/// counterpart of [`xmlCharEncInFunc`].  Same semantics; the
/// direction flag selects which UTF-16 endianness is emitted (BE for
/// out, mirroring libxml2's serializer default).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharEncOutFunc(
    handler: *mut xmlCharEncodingHandler,
    out:     *mut xmlBuffer,
    input:   *mut xmlBuffer,
) -> c_int {
    unsafe { transcode_buffer(handler, out, input, /*outbound=*/ true) }
}

/// Shared engine for [`xmlCharEncInFunc`] / [`xmlCharEncOutFunc`].
/// Returns the number of bytes appended to `out`, or -1 on error.
unsafe fn transcode_buffer(
    handler: *mut xmlCharEncodingHandler,
    out:     *mut xmlBuffer,
    input:   *mut xmlBuffer,
    outbound: bool,
) -> c_int {
    if out.is_null() || input.is_null() { return -1; }
    // Read input contents.
    let src_ptr = unsafe { (*input).content };
    let src_len = unsafe { (*input).use_ } as usize;
    if src_ptr.is_null() || src_len == 0 { return 0; }
    // SAFETY: input.content is valid for `use_` bytes per xmlBuffer contract.
    let bytes = unsafe { std::slice::from_raw_parts(src_ptr as *const u8, src_len) };
    // Pick a transcoded form.  For non-UTF-16 handlers (and for the
    // NULL-handler case) we pass through verbatim.
    let encoded: Option<Vec<u8>> = if handler.is_null() {
        None
    } else {
        unsafe { transcode_for_encoder(handler as *mut c_void, bytes) }
    };
    let _ = outbound; // both directions land on the same table today
    let final_bytes: &[u8] = match &encoded {
        Some(v) => v,
        None    => bytes,
    };
    unsafe {
        buffer_append(out, final_bytes);
        // Clear the input buffer to match libxml2 semantics: the bytes
        // have been "consumed" — input.use becomes 0 so a follow-up
        // call doesn't re-emit them.
        (*input).use_ = 0;
        if !(*input).content.is_null() {
            *(*input).content = 0;
        }
    }
    final_bytes.len() as c_int
}

/// libxml2 `xmlBufferCreateStatic(mem, size)` — wrap a *read-only*
/// static byte range as an xmlBuffer.  The buffer's `content` points
/// at the caller's memory and its `use`/`size` slots reflect `size`;
/// the buffer is marked `XML_BUFFER_ALLOC_IMMUTABLE` so consumers
/// know not to grow it.
///
/// Note that since the underlying bytes are caller-owned, mutating
/// helpers (`xmlBufferAdd`, etc.) are not safe to call on the result.
/// Use `xmlBufferFree` to release the wrapper; the wrapped memory is
/// not freed.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBufferCreateStatic(
    mem:  *const c_void,
    size: usize,
) -> *mut xmlBuffer {
    if mem.is_null() { return std::ptr::null_mut(); }
    let len = size as c_uint;
    Box::into_raw(Box::new(xmlBuffer {
        content:    mem as *mut c_char,
        use_:       len,
        size:       len,
        alloc:      4,             // XML_BUFFER_ALLOC_IMMUTABLE
        _pad_alloc: 0,
        content_io: std::ptr::null_mut(),
    }))
}

/// libxml2 `xmlCharEncNewCustomHandler(name, input, output, ctxtDtor,
/// inputCtxt, outputCtxt, out)` — construct a handler that carries the
/// caller's own conversion functions.  This is the documented way for an
/// [`xmlCharEncConvImpl`](crate::parsectx::XmlCharEncConvImpl) factory to
/// produce the handler it returns: the parser later drives `input` to
/// transcode a document's bytes into UTF-8, and frees the handler (calling
/// `ctxtDtor` on the per-direction contexts) via [`xmlCharEncCloseFunc`].
///
/// Returns `XML_ERR_OK` (0) on success, writing the new handler through
/// `out`.  Returns a non-zero error when `out` is NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharEncNewCustomHandler(
    name:        *const c_char,
    input:       Option<XmlCharEncConvFunc>,
    output:      Option<XmlCharEncConvFunc>,
    ctxt_dtor:   Option<XmlCharEncConvCtxtDtor>,
    input_ctxt:  *mut c_void,
    output_ctxt: *mut c_void,
    out:         *mut *mut xmlCharEncodingHandler,
) -> c_int {
    if out.is_null() {
        return 1; // XML_ERR_INTERNAL_ERROR — nothing to write the handler into.
    }
    let cname = if name.is_null() {
        ptr::null_mut()
    } else {
        // SAFETY: caller asserts name is null-terminated when non-null.
        match unsafe { std::ffi::CStr::from_ptr(name) }
            .to_str()
            .ok()
            .and_then(|s| std::ffi::CString::new(s).ok())
        {
            Some(c) => c.into_raw(),
            None => ptr::null_mut(),
        }
    };
    let h = Box::into_raw(Box::new(xmlCharEncodingHandler {
        name: cname,
        input,
        output,
        input_ctxt,
        output_ctxt,
        ctxt_dtor,
        flags: 0,
    }));
    unsafe { *out = h; }
    0
}

/// `xmlCharEncCloseFunc(handler)` — free a handler allocated by
/// `xmlFindCharEncodingHandler` or [`xmlCharEncNewCustomHandler`].  Drops
/// the inner `name` CString and, for custom handlers, invokes `ctxtDtor`
/// on each non-null converter context before freeing the box.  Returns 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharEncCloseFunc(h: *mut xmlCharEncodingHandler) -> c_int {
    if h.is_null() { return 0; }
    // SAFETY: caller asserts h was returned from one of our handler factories.
    unsafe {
        let boxed = Box::from_raw(h);
        if !boxed.name.is_null() {
            drop(std::ffi::CString::from_raw(boxed.name));
        }
        if let Some(dtor) = boxed.ctxt_dtor {
            if !boxed.input_ctxt.is_null() {
                dtor(boxed.input_ctxt);
            }
            // Guard against a double-free when both directions share a context.
            if !boxed.output_ctxt.is_null() && boxed.output_ctxt != boxed.input_ctxt {
                dtor(boxed.output_ctxt);
            }
        }
        drop(boxed);
    }
    0
}

/// libxml2 `xmlIsXHTML`: a document is XHTML when its internal subset's
/// public or system identifier is one of the three XHTML 1.0 DTDs.
/// libxml2 routes such documents through `xhtmlNodeDumpOutput`, which we
/// mirror via [`SerializeOptions::xhtml`].  `doc->intSubset` is at offset
/// 80; the DTD's `ExternalID`/`SystemID` at offsets 104/112.
pub(crate) unsafe fn doc_is_xhtml(doc: *const sup_xml_tree::dom::XmlDoc) -> bool {
    if doc.is_null() {
        return false;
    }
    let dtd = unsafe { *((doc as *const u8).add(80) as *const *const u8) };
    if dtd.is_null() {
        return false;
    }
    let read = |off: usize| -> Option<&'static str> {
        let p = unsafe { *((dtd as *const u8).add(off) as *const *const c_char) };
        if p.is_null() { None } else { unsafe { std::ffi::CStr::from_ptr(p) }.to_str().ok() }
    };
    const XHTML_PUBLIC: [&str; 3] = [
        "-//W3C//DTD XHTML 1.0 Strict//EN",
        "-//W3C//DTD XHTML 1.0 Transitional//EN",
        "-//W3C//DTD XHTML 1.0 Frameset//EN",
    ];
    const XHTML_SYSTEM: [&str; 3] = [
        "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd",
        "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd",
        "http://www.w3.org/TR/xhtml1/DTD/xhtml1-frameset.dtd",
    ];
    read(104).is_some_and(|id| XHTML_PUBLIC.contains(&id))
        || read(112).is_some_and(|id| XHTML_SYSTEM.contains(&id))
}

// ── xmlNodeDumpOutput ─────────────────────────────────────────────────────

/// `xmlNodeDumpOutput(buf, doc, cur, level, format, encoding)` —
/// serialize a node into the given output buffer.  This is the
/// path lxml's tostring uses.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeDumpOutput(
    buf:       *mut xmlOutputBuffer,
    _doc:      *const sup_xml_tree::dom::XmlDoc,
    cur:       *const sup_xml_tree::dom::Node<'static>,
    _level:    c_int,
    format:    c_int,
    encoding:  *const c_char,
) {
    if buf.is_null() || cur.is_null() { return; }
    // DTD declaration nodes (`XML_ELEMENT_DECL`=15, `XML_ATTRIBUTE_DECL`
    // =16, `XML_ENTITY_DECL`=17) are not `sup_xml_tree` `Node`s — they
    // are the libxml2-shaped decl structs in `crate::dtddecl`, which the
    // core serializer can't read.  lxml's `_writeDtdToBuffer` calls us
    // once per such child to build the `<!DOCTYPE … [ … ]>` body; emit
    // the declaration text materialized for the node.  (Unregistered
    // decl nodes — none in practice — emit nothing rather than crash.)
    let node_type = unsafe { *((cur as *const u8).add(8) as *const c_int) };
    if matches!(node_type, 15 | 16 | 17) {
        if let Some(text) = crate::dtddecl::decl_source(cur as *const c_void) {
            unsafe {
                xmlOutputBufferWrite(buf, text.len() as c_int, text.as_ptr() as *const c_char);
            }
        }
        return;
    }
    // The target encoding governs character-reference escaping: a
    // narrow output charset (ASCII, Latin-1) escapes anything it cannot
    // represent.  NULL encoding defaults to ASCII, matching libxml2.
    let enc_name = if encoding.is_null() {
        None
    } else {
        unsafe { std::ffi::CStr::from_ptr(encoding) }.to_str().ok()
    };
    let opts = sup_xml_core::serializer::SerializeOptions {
        write_xml_decl: false,
        format:         format != 0,
        indent:         if format != 0 { "  ".to_string() } else { String::new() },
        html_mode:      false,
        xhtml:          unsafe { doc_is_xhtml(_doc) },
        out_charset:    charset_for_encoding(enc_name),
    };
    // SAFETY: cur is non-null per the check.
    let n = unsafe { &*cur };
    let out = sup_xml_core::serializer::serialize_node_to_string(n, &opts);
    // Route through xmlOutputBufferWrite so the buffer's dispatch
    // (in-memory accumulation, file-flush sentinel, or user
    // callback) all work the same way — lxml's `tree.write(file)`
    // path wires a callback that we MUST invoke for the bytes to
    // reach the destination.
    unsafe {
        xmlOutputBufferWrite(buf, out.len() as c_int, out.as_ptr() as *const c_char);
    }
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn local_path_mapping_matches_libxml2() {
        // Plain paths without a `%` are used verbatim.
        assert_eq!(local_path_from_file_uri("/tmp/a.xml"), "/tmp/a.xml");
        assert_eq!(local_path_from_file_uri("rel/dir/a.xml"), "rel/dir/a.xml");

        // A bare path with a literal `%` is URI-unescaped (libxml2 treats
        // filenames as URIs).  This is why lxml escapes `%`→`%25` before
        // serializing to a path: it round-trips back to the real name.
        assert_eq!(local_path_from_file_uri("/tmp/p%2520p.xml"), "/tmp/p%20p.xml");
        assert_eq!(local_path_from_file_uri("/tmp/a%2Fb.xml"), "/tmp/a/b.xml");

        // `file://` and `file://localhost` schemes are stripped, then the
        // remaining path is unescaped.
        assert_eq!(local_path_from_file_uri("file:///tmp/a.xml"), "/tmp/a.xml");
        assert_eq!(local_path_from_file_uri("file://localhost/tmp/a.xml"), "/tmp/a.xml");
        assert_eq!(local_path_from_file_uri("file:///tmp/p%2520p.xml"), "/tmp/p%20p.xml");

        // Names that aren't valid URIs (embedded space) are NOT unescaped —
        // libxml2's "limit the damage" guard — so a real space survives.
        assert_eq!(local_path_from_file_uri("/tmp/a b.xml"), "/tmp/a b.xml");
        assert_eq!(local_path_from_file_uri("file:///tmp/a b.xml"), "/tmp/a b.xml");
    }

    #[test]
    fn open_char_encoding_handler_returns_named_handler() {
        let name = b"ISO-8859-1\0";
        let mut out: *mut xmlCharEncodingHandler = ptr::null_mut();
        let rc = unsafe {
            xmlOpenCharEncodingHandler(name.as_ptr() as *const c_char, 0, &mut out)
        };
        assert_eq!(rc, 0, "valid name should return XML_ERR_OK");
        assert!(!out.is_null());
        let got = unsafe { CStr::from_ptr((*out).name) }.to_str().unwrap();
        assert_eq!(got, "ISO-8859-1");
        unsafe { xmlCharEncCloseFunc(out); }

        // NULL out-pointer is rejected, not a crash.
        let rc = unsafe { xmlOpenCharEncodingHandler(name.as_ptr() as *const c_char, 0, ptr::null_mut()) };
        assert_eq!(rc, 32);
    }

    #[test]
    fn lookup_char_encoding_handler_by_enum() {
        // UTF-8 (1) and NONE (0) need no handler → NULL, OK.
        for enc in [0, 1] {
            let mut out: *mut xmlCharEncodingHandler = (-1isize) as *mut _;
            let rc = unsafe { xmlLookupCharEncodingHandler(enc, &mut out) };
            assert_eq!(rc, 0);
            assert!(out.is_null(), "enc {enc} should need no handler");
        }
        // ISO-8859-1 (10) → a real handler.
        let mut out: *mut xmlCharEncodingHandler = ptr::null_mut();
        let rc = unsafe { xmlLookupCharEncodingHandler(10, &mut out) };
        assert_eq!(rc, 0);
        assert!(!out.is_null());
        unsafe { xmlCharEncCloseFunc(out); }
        // Negative enc is the error sentinel.
        let mut out2: *mut xmlCharEncodingHandler = ptr::null_mut();
        assert_eq!(unsafe { xmlLookupCharEncodingHandler(-1, &mut out2) }, 32);
    }

    #[test]
    fn create_char_encoding_handler_null_impl_uses_builtin() {
        let name = b"UTF-16\0";
        // NULL impl → built-in handler.
        let mut out: *mut xmlCharEncodingHandler = ptr::null_mut();
        let rc = unsafe {
            xmlCreateCharEncodingHandler(
                name.as_ptr() as *const c_char, 0, None, ptr::null_mut(), &mut out)
        };
        assert_eq!(rc, 0);
        assert!(!out.is_null());
        unsafe { xmlCharEncCloseFunc(out); }
    }

    #[test]
    fn create_char_encoding_handler_delegates_to_custom_impl() {
        // A caller-supplied factory builds a name-carrying handler and the
        // call returns its result verbatim.
        unsafe extern "C" fn factory(
            vctxt: *mut c_void,
            name:  *const c_char,
            _flags: c_int,
            out:   *mut *mut xmlCharEncodingHandler,
        ) -> c_int {
            // The vctxt threads straight through from the caller.
            assert_eq!(vctxt, 0x42 as *mut c_void);
            unsafe {
                xmlCharEncNewCustomHandler(
                    name, None, None, None,
                    ptr::null_mut(), ptr::null_mut(), out)
            }
        }
        let name = b"x-custom\0";
        let mut out: *mut xmlCharEncodingHandler = ptr::null_mut();
        let rc = unsafe {
            xmlCreateCharEncodingHandler(
                name.as_ptr() as *const c_char, 0, Some(factory), 0x42 as *mut c_void, &mut out)
        };
        assert_eq!(rc, 0);
        assert!(!out.is_null());
        let nm = unsafe { CStr::from_ptr((*out).name) }.to_str().unwrap();
        assert_eq!(nm, "x-custom");
        unsafe { xmlCharEncCloseFunc(out); }
    }

    #[test]
    fn write_and_read_buffer() {
        let out = unsafe { xmlAllocOutputBuffer(ptr::null_mut()) };
        let s = b"hello\0";
        let n = unsafe {
            xmlOutputBufferWrite(out, 5, s.as_ptr() as *const c_char)
        };
        assert_eq!(n, 5);
        unsafe {
            let o = &*out;
            let content = (*o.buffer).content;
            assert!(!content.is_null());
            let bytes = std::slice::from_raw_parts(content as *const u8, (*o.buffer).use_ as usize);
            assert_eq!(bytes, b"hello");
            xmlOutputBufferClose(out);
        }
    }

    #[test]
    fn output_buffer_utf32le_transcodes() {
        // An output buffer with a UTF-32LE encoder must emit one 4-byte
        // little-endian code unit per character into buf.conv — not pass
        // the UTF-8 bytes through (which produced an encoding/byte-content
        // mismatch that broke serialize→reparse round-trips).
        let enc = unsafe {
            xmlFindCharEncodingHandler(b"UTF-32LE\0".as_ptr() as *const c_char)
        };
        assert!(!enc.is_null());
        let out = unsafe { xmlAllocOutputBuffer(enc as *mut c_void) };
        let s = b"AB";
        unsafe { xmlOutputBufferWrite(out, 2, s.as_ptr() as *const c_char); }
        unsafe {
            let o = &*out;
            assert!(!o.conv.is_null(), "transcoded bytes should land in buf.conv");
            let n = (*o.conv).use_ as usize;
            let bytes = std::slice::from_raw_parts((*o.conv).content as *const u8, n);
            assert_eq!(bytes, &[b'A', 0, 0, 0, b'B', 0, 0, 0],
                "UTF-32LE encodes each char as 4 little-endian bytes");
            xmlOutputBufferClose(out);
        }
    }

    #[test]
    fn write_escape() {
        let out = unsafe { xmlAllocOutputBuffer(ptr::null_mut()) };
        let s = CStr::from_bytes_with_nul(b"a<b&c>\"d\0").unwrap();
        unsafe {
            xmlOutputBufferWriteEscape(out, s.as_ptr(), ptr::null_mut());
            let o = &*out;
            let content = (*o.buffer).content;
            let len = (*o.buffer).use_ as usize;
            let bytes = std::slice::from_raw_parts(content as *const u8, len);
            assert_eq!(bytes, b"a&lt;b&amp;c&gt;&quot;d");
            xmlOutputBufferClose(out);
        }
    }

    #[test]
    fn close_returns_written_count() {
        let out = unsafe { xmlAllocOutputBuffer(ptr::null_mut()) };
        unsafe {
            xmlOutputBufferWrite(out, 5, b"hello".as_ptr() as *const _);
            xmlOutputBufferWrite(out, 6, b" world".as_ptr() as *const _);
            let n = xmlOutputBufferClose(out);
            assert_eq!(n, 11);
        }
    }

    #[test]
    fn create_filename_flushes_on_close() {
        let tmp = std::env::temp_dir().join("sup-xml-outbuf-test.xml");
        let _ = std::fs::remove_file(&tmp);
        let path_cs = std::ffi::CString::new(tmp.to_str().unwrap()).unwrap();
        let out = unsafe {
            xmlOutputBufferCreateFilename(path_cs.as_ptr(), ptr::null_mut(), 0)
        };
        assert!(!out.is_null(), "open should succeed");
        unsafe {
            xmlOutputBufferWriteString(out, b"<r>hello</r>\0".as_ptr() as *const _);
            xmlOutputBufferClose(out);
        }
        let read_back = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(read_back, "<r>hello</r>");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn create_filename_returns_null_on_bad_path() {
        let path_cs = std::ffi::CString::new("/no/such/dir/cannot/write.xml").unwrap();
        let out = unsafe {
            xmlOutputBufferCreateFilename(path_cs.as_ptr(), ptr::null_mut(), 0)
        };
        assert!(out.is_null());
    }

    // ── new xmlBuffer fns ────────────────────────────────────────────────

    fn cs(s: &str) -> std::ffi::CString { std::ffi::CString::new(s).unwrap() }

    #[test]
    fn xml_buffer_add_appends_len_bytes() {
        let buf = unsafe { xmlBufferCreate() };
        let s = cs("hello world");
        let rc = unsafe { xmlBufferAdd(buf, s.as_ptr(), 5) };
        assert_eq!(rc, 0);
        let content = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(buf)) };
        assert_eq!(content.to_bytes(), b"hello");
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_buffer_add_with_negative_len_uses_nul_terminated() {
        let buf = unsafe { xmlBufferCreate() };
        let s = cs("abc");
        let rc = unsafe { xmlBufferAdd(buf, s.as_ptr(), -1) };
        assert_eq!(rc, 0);
        let content = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(buf)) };
        assert_eq!(content.to_bytes(), b"abc");
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_buffer_cat_and_ccat_append_strings() {
        let buf = unsafe { xmlBufferCreate() };
        let a = cs("foo");
        let b = cs("bar");
        assert_eq!(unsafe { xmlBufferCat(buf, a.as_ptr()) }, 0);
        assert_eq!(unsafe { xmlBufferCCat(buf, b.as_ptr()) }, 0);
        let content = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(buf)) };
        assert_eq!(content.to_bytes(), b"foobar");
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_buffer_create_size_pre_reserves() {
        let buf = unsafe { xmlBufferCreateSize(128) };
        assert!(!buf.is_null());
        // We still need a NUL after content; size includes that slot.
        assert!(unsafe { (*buf).size } >= 128);
        // Use should be 0 (no content yet).
        assert_eq!(unsafe { (*buf).use_ }, 0);
        // Appending should still work and not re-grow if within cap.
        let s = cs("hi");
        unsafe { xmlBufferCat(buf, s.as_ptr()); }
        let content = unsafe { std::ffi::CStr::from_ptr(xmlBufferContent(buf)) };
        assert_eq!(content.to_bytes(), b"hi");
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_buffer_detach_transfers_ownership() {
        let buf = unsafe { xmlBufferCreate() };
        let s = cs("payload");
        unsafe { xmlBufferCat(buf, s.as_ptr()); }
        let detached = unsafe { xmlBufferDetach(buf) };
        assert!(!detached.is_null());
        // The buffer is now empty.
        assert_eq!(unsafe { xmlBufferLength(buf) }, 0);
        assert!(unsafe { xmlBufferContent(buf) }.is_null());
        // Detached pointer holds the data and is freeable via xmlFree.
        let bytes = unsafe { std::ffi::CStr::from_ptr(detached) };
        assert_eq!(bytes.to_bytes(), b"payload");
        unsafe { crate::parse::xml_free_impl(detached as *mut std::os::raw::c_void); }
        unsafe { xmlBufferFree(buf); }
    }

    #[test]
    fn xml_buffer_set_allocation_scheme_stores_value() {
        let buf = unsafe { xmlBufferCreate() };
        unsafe { xmlBufferSetAllocationScheme(buf, 3); }
        assert_eq!(unsafe { (*buf).alloc }, 3);
        unsafe { xmlBufferFree(buf); }
    }
}
