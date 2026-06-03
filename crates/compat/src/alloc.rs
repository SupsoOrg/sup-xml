//! Allocator registry for `xmlFree` dual-pointer detection.
//!
//! Pointers that this crate returns via `xmlGetProp`/`xmlNodeGetContent`/etc.
//! are libc-heap allocated and must be released via
//! [`crate::parse::xml_free_impl`] → [`registry_free`] (libc `free`).
//! Pointers reachable through struct field reads (e.g. `node->name`,
//! `attr->value`) live in the document's bumpalo arena and are
//! reclaimed wholesale by `xmlFreeDoc`.
//!
//! libxml2 historically tolerated `xmlFree` on arena-resident pointers
//! as a silent no-op (matching real-world consumer code that doesn't
//! always know which kind of pointer it holds).  We preserve that
//! contract by keeping a global registry of every pointer we malloc'd:
//!
//!   - [`register_alloc`] records a libc-allocated pointer.
//!   - [`take_alloc`] removes-and-returns a pointer when xmlFree is
//!     called; returns `true` iff the pointer was registered (and is
//!     safe to release via libc `free`).
//!
//! Every registry allocator (`alloc_registered_cstring`,
//! [`alloc_registered_zeroed`], `impl_xml_malloc`/`impl_xml_realloc`)
//! uses libc `malloc`/`realloc`, and the free path uses libc `free`, so
//! the C-ABI allocator stays consistent regardless of any custom Rust
//! `#[global_allocator]` the host process installs.
//!
//! Reading 8 bytes BEFORE an arena pointer (magic-prefix approach)
//! would be unsafe — the byte may fall before the bumpalo chunk
//! boundary and access uninitialized or unmapped memory.  A registry
//! avoids that hazard entirely.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

fn registry() -> &'static Mutex<HashSet<usize>> {
    static REG: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Binary-safe registry: pointer → allocation size (NUL-terminator
/// included).  Used by allocators that copy literal bytes (including
/// interior NULs, e.g. UTF-16 content) where `strlen` can't recover
/// the original length.
fn binary_registry() -> &'static Mutex<HashMap<usize, usize>> {
    static REG: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a heap allocation we're about to return to a C caller.
///
/// NULL is silently ignored — convenient when chaining off a
/// `CString::into_raw` that may itself have failed.
pub fn register_alloc(p: *const u8) {
    if !p.is_null() {
        registry().lock().expect("alloc registry poisoned").insert(p as usize);
    }
}

/// Remove a pointer from the registry.  Returns `true` iff it was
/// present — i.e. this is a pointer we'd handed out via an allocating
/// API and the caller is now safe to free it as a CString.  `false`
/// means either (a) the pointer is arena-resident (silent no-op
/// expected) or (b) the caller is double-freeing (also silent no-op,
/// defensively).
pub fn take_alloc(p: *const u8) -> bool {
    if p.is_null() {
        return false;
    }
    registry()
        .lock()
        .expect("alloc registry poisoned")
        .remove(&(p as usize))
}

/// Allocate a NUL-terminated C string copy of `s` and register the
/// pointer.  Returns NULL on allocation failure.
///
/// The copy is libc-allocated (not `CString::into_raw`) so it shares
/// one allocator with `impl_xml_malloc` and is freed symmetrically via
/// [`registry_free`] — see that function for why allocator symmetry
/// matters here.  An interior NUL truncates the copy, matching the C
/// string contract (and libxml2's own behaviour); well-formed XML
/// content never contains U+0000.
pub fn alloc_registered_cstring(s: &[u8]) -> *mut std::os::raw::c_char {
    let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    let body = &s[..end];
    // SAFETY: libc malloc of `body.len() + 1` bytes; we write exactly
    // that many (the body followed by a trailing NUL) before returning
    // the pointer, and register it so `xmlFree` reclaims it.
    let raw = unsafe { malloc(body.len() + 1) } as *mut u8;
    if raw.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        std::ptr::copy_nonoverlapping(body.as_ptr(), raw, body.len());
        *raw.add(body.len()) = 0;
    }
    register_alloc(raw as *const u8);
    raw as *mut std::os::raw::c_char
}

/// Binary-safe variant of [`alloc_registered_cstring`]: copies the
/// input verbatim (including any interior NUL bytes), appends a
/// trailing NUL, and registers the pointer so `xmlFree` releases
/// the full allocation.
///
/// Required for buffers where the byte length is the authoritative
/// size — UTF-16 output, raw memory dumps — because `CString::from_raw`
/// uses `strlen` to size the Drop, which would silently leak past the
/// first interior NUL.
pub fn alloc_registered_buffer(s: &[u8]) -> *mut std::os::raw::c_char {
    let len = s.len();
    let mut v: Vec<u8> = Vec::with_capacity(len + 1);
    v.extend_from_slice(s);
    v.push(0); // trailing NUL — libxml2's xmlChar* contract
    let boxed: Box<[u8]> = v.into_boxed_slice();
    let total = boxed.len();
    let raw = Box::into_raw(boxed) as *mut u8;
    binary_registry()
        .lock()
        .expect("binary alloc registry poisoned")
        .insert(raw as usize, total);
    raw as *mut std::os::raw::c_char
}

/// If `p` was registered as a binary-safe allocation, return its
/// total size (including the trailing NUL) and remove it from the
/// registry.  Used by `xmlFree` to reconstruct the original Box<[u8]>
/// for proper Drop.
pub fn take_binary_alloc(p: *const u8) -> Option<usize> {
    if p.is_null() {
        return None;
    }
    binary_registry()
        .lock()
        .expect("binary alloc registry poisoned")
        .remove(&(p as usize))
}

// ── libxml2 allocator family ──────────────────────────────────────────────
//
// libxml2 exposes its own malloc/realloc/free entry points as
// *function-pointer globals* (not functions), so callers can swap the
// allocator at runtime via `xmlMemSetup`.  In the threaded build
// path, headers macro-expand `xmlMalloc(size)` to `(*__xmlMalloc())(size)`,
// but consumers compiled without that path see the symbol as a bare
// `xmlMallocFunc` variable and emit `LDR/BLR` to dispatch through it.
// libxslt is one of those consumers; treating the symbol as a function
// (BL straight to it) and treating it as a variable (LDR the slot, BLR
// the loaded value) produce incompatible machine code.  Match libxml2:
// expose these as `static mut` fn-ptr slots, initialised to point at
// our internal implementations.
//
// `xmlMallocAtomic` historically allocated memory that wouldn't be
// scanned by a (hypothetical) garbage collector; in modern libxml2
// it's identical to `xmlMalloc`.  We follow suit.

use std::os::raw::{c_int, c_void};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// Release a pointer held in the main allocation registry.
///
/// Every pointer that reaches [`take_alloc`] is produced by libc
/// `malloc`/`realloc` (see [`alloc_registered_cstring`],
/// [`alloc_registered_zeroed`], and `impl_xml_malloc`), so it must be
/// freed with libc `free` — not the Rust global allocator, which is a
/// *different* heap whenever the host installs a custom
/// `#[global_allocator]` (mimalloc, jemalloc, …).  Mixing the two is
/// heap corruption; keeping the whole registry libc-symmetric makes
/// the C-ABI allocator independent of whatever Rust allocator the
/// surrounding process chose.
///
/// # Safety
///
/// `ptr` must have been returned by one of the registry allocators and
/// not yet freed.
pub unsafe fn registry_free(ptr: *mut c_void) {
    unsafe { free(ptr) }
}

/// Allocate `size` zeroed bytes via libc and register the pointer so
/// `xmlFree` reclaims it through [`registry_free`].  Used by the
/// `xmlParserInputBuffer*` constructors, which hand libxml2 an opaque
/// zeroed context block.  Returns NULL on allocation failure.
pub fn alloc_registered_zeroed(size: usize) -> *mut c_void {
    // SAFETY: libc malloc of `size` bytes; we zero exactly `size`
    // bytes before the pointer is observed.
    let raw = unsafe { malloc(size) } as *mut u8;
    if raw.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { std::ptr::write_bytes(raw, 0, size); }
    register_alloc(raw as *const u8);
    raw as *mut c_void
}

unsafe extern "C" fn impl_xml_malloc(size: usize) -> *mut c_void {
    let p = unsafe { malloc(size) };
    // Mark the allocation so xmlRealloc / xmlFree can discriminate
    // pointers we own from caller-supplied / arena-resident pointers.
    // Without this, libxslt (which sometimes passes our arena
    // pointers to xmlRealloc) hits the macOS libmalloc abort
    // `pointer being realloc'd was not allocated`.
    register_alloc(p as *const u8);
    p
}

unsafe extern "C" fn impl_xml_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    // NULL → behave like malloc per the C standard.
    if ptr.is_null() {
        return unsafe { impl_xml_malloc(size) };
    }
    // Try the registered-pointer path first — real heap allocations
    // we previously handed out via xmlMalloc / xmlStrdup / etc.
    if take_alloc(ptr as *const u8) {
        let new = unsafe { realloc(ptr, size) };
        register_alloc(new as *const u8);
        return new;
    }
    // Arena-resident pointer.  Common path: libxslt's
    // `xsltAddTextString` reallocs `target->content` to grow a text
    // node in the result tree.  Our content lives in bumpalo, not
    // libc heap.  Bridge by copying the old NUL-terminated bytes
    // into a fresh malloc'd buffer of the requested size, then hand
    // back the new buffer (registered so subsequent xmlRealloc /
    // xmlFree calls go through the normal heap path).
    //
    // The original arena bytes stay live until the doc's bump arena
    // is dropped — wasted memory until then, but the leak is
    // bounded (one text-node payload per merge in the result
    // tree, the doc itself is freed at xmlFreeDoc).
    let new = unsafe { impl_xml_malloc(size) };
    if new.is_null() { return std::ptr::null_mut(); }
    // SAFETY: ptr is a NUL-terminated C string per libxml2's content
    // contract (every xmlChar* we expose for `node->content` ends
    // with a 0).  strlen finds its length without UB.
    unsafe extern "C" {
        fn strlen(s: *const std::os::raw::c_char) -> usize;
    }
    let old_len = unsafe { strlen(ptr as *const std::os::raw::c_char) };
    let copy_len = old_len.min(size.saturating_sub(1));
    unsafe {
        std::ptr::copy_nonoverlapping(ptr as *const u8, new as *mut u8, copy_len);
        *(new as *mut u8).add(copy_len) = 0;
    }
    new
}

// `#[allow(non_upper_case_globals)]`: the names are intentionally
// camelCase to mirror libxml2's `xmlMalloc` / `xmlRealloc` / `xmlFree`
// — that's the whole point.  The `#[no_mangle]` attribute used to
// suppress this lint implicitly; now that `no_mangle` is feature-
// gated, we have to allow it explicitly so the default (rlib) build
// stays warning-clean.
#[allow(non_upper_case_globals)]
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static mut xmlMalloc: unsafe extern "C" fn(usize) -> *mut c_void = impl_xml_malloc;

#[allow(non_upper_case_globals)]
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static mut xmlMallocAtomic: unsafe extern "C" fn(usize) -> *mut c_void = impl_xml_malloc;

#[allow(non_upper_case_globals)]
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub static mut xmlRealloc: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void = impl_xml_realloc;

// ── xmlMem* family ──────────────────────────────────────────────────────────
//
// libxml2's documented "allocator override" entry points.  Memory-
// tracking consumers (Python's lxml, valgrind harnesses, custom
// pool allocators) call `xmlMemSetup` at startup to intercept every
// future malloc / realloc / free through their own functions.
//
// Implementation notes:
//
//   - `xmlMemSetup` writes to the four `static mut` function-pointer
//     globals (`xmlMalloc`, `xmlMallocAtomic`, `xmlRealloc`,
//     `xmlFree`).  After the swap, every libxml2-style allocation
//     goes through the caller's hooks instead of our `impl_xml_*`
//     functions.  The arena-vs-heap discrimination our default
//     `impl_xml_*` does (registering each pointer so `xmlFree` can
//     tell ours from bumpalo-resident ones) is bypassed too — the
//     caller's malloc/free are assumed to be self-consistent.
//
//   - `xmlMemSetup` must be called BEFORE any allocations.  Mixing
//     pre-swap and post-swap pointers will free/realloc through the
//     wrong function and corrupt the heap.  Matches libxml2's
//     documented constraint exactly.
//
//   - The `strdupFn` argument is accepted but currently ignored.
//     `xmlStrdup` doesn't dispatch through a function pointer in our
//     build (it directly mallocs via `xmlMalloc` and copies).  When
//     `xmlMemSetup` is called, the `xmlStrdup` path automatically
//     uses the caller's new malloc; no separate hook is needed.
//
//   - `xmlMemSize` returns 0 unconditionally.  Real libxml2 only
//     tracks sizes when built with `WITH_MEM_DEBUG`; otherwise it
//     returns 0.  Consumers that branch on this (xmllint's
//     `--memory` flag) degrade to "no size info" rather than
//     misreport.

/// Function-pointer types for the four allocator hooks (libxml2's
/// `xmlFreeFunc` / `xmlMallocFunc` / `xmlReallocFunc` / `xmlStrdupFunc`).
/// `Option`-wrapped so the FFI null-check happens at the type level.
pub type XmlFreeFunc    = Option<unsafe extern "C" fn(*mut c_void)>;
pub type XmlMallocFunc  = Option<unsafe extern "C" fn(usize) -> *mut c_void>;
pub type XmlReallocFunc = Option<unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void>;
pub type XmlStrdupFunc  = Option<unsafe extern "C" fn(*const std::os::raw::c_char) -> *mut std::os::raw::c_char>;

/// libxml2 `xmlMemSetup(freeFn, mallocFn, reallocFn, strdupFn)` —
/// install caller-provided allocator hooks.  Subsequent allocations
/// and frees go through the caller's functions.  Returns 0 on
/// success, -1 if any of the three required hooks (free/malloc/
/// realloc) is NULL.  `strdupFn` is accepted but currently unused.
///
/// # Safety
///
/// Must be called before any sup-xml allocation; mixing pre-swap and
/// post-swap pointers is undefined behaviour.  Caller is responsible
/// for ensuring no other thread is concurrently allocating or freeing
/// when the swap happens.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemSetup(
    free_fn:    XmlFreeFunc,
    malloc_fn:  XmlMallocFunc,
    realloc_fn: XmlReallocFunc,
    _strdup_fn: XmlStrdupFunc,
) -> c_int {
    let (Some(f), Some(m), Some(r)) = (free_fn, malloc_fn, realloc_fn) else { return -1; };
    // SAFETY: writing to `static mut` fn-pointer globals.  The
    // documented contract is that no other thread is allocating
    // through these during the swap; caller's responsibility.
    unsafe {
        xmlMalloc        = m;
        xmlMallocAtomic  = m;
        xmlRealloc       = r;
        crate::parse::xmlFree = f;
    }
    0
}

/// libxml2 `xmlMemGet(freePP, mallocPP, reallocPP, strdupPP)` —
/// retrieve the currently-installed allocator hooks.  Each `*PP`
/// argument may be NULL (that slot is simply not written).
///
/// Returns 0 always (matches libxml2; documented as cannot fail).
///
/// # Safety
///
/// Non-NULL pointers must reference writable storage of the
/// corresponding function-pointer-Option type.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemGet(
    free_pp:    *mut XmlFreeFunc,
    malloc_pp:  *mut XmlMallocFunc,
    realloc_pp: *mut XmlReallocFunc,
    strdup_pp:  *mut XmlStrdupFunc,
) -> c_int {
    // SAFETY: caller-supplied out-pointers; we null-check each one
    // before writing.  Reads from `static mut` globals are race-free
    // under the same single-threaded-init contract as xmlMemSetup.
    unsafe {
        if !free_pp.is_null()    { *free_pp    = Some(crate::parse::xmlFree); }
        if !malloc_pp.is_null()  { *malloc_pp  = Some(xmlMalloc); }
        if !realloc_pp.is_null() { *realloc_pp = Some(xmlRealloc); }
        // We never set xmlStrdup as a hook; report None so callers
        // that round-trip xmlMemGet → xmlMemSetup don't accidentally
        // install whatever garbage was in their stack slot.
        if !strdup_pp.is_null()  { *strdup_pp  = None; }
    }
    0
}

/// libxml2 `xmlMemMalloc(size)` — explicit entry point that goes
/// through the currently-installed `xmlMalloc` hook.  Equivalent to
/// the inline `xmlMalloc(size)` macro expansion in headers without
/// the threaded macro form.
///
/// # Safety
///
/// Caller owns the returned pointer; release via [`xmlMemFree`] or
/// the C-side `xmlFree`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemMalloc(size: usize) -> *mut c_void {
    // SAFETY: dispatching through the active fn-pointer global.
    unsafe { xmlMalloc(size) }
}

/// libxml2 `xmlMemRealloc(ptr, size)` — equivalent of
/// `xmlRealloc(ptr, size)`.
///
/// # Safety
///
/// `ptr` must be NULL or a pointer previously returned by an
/// `xmlMalloc`-family call (under the currently-installed allocator).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemRealloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: dispatching through the active fn-pointer global.
    unsafe { xmlRealloc(ptr, size) }
}

/// libxml2 `xmlMemFree(ptr)` — equivalent of `xmlFree(ptr)`.
///
/// # Safety
///
/// `ptr` must be NULL or a pointer previously returned by an
/// `xmlMalloc`-family call.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemFree(ptr: *mut c_void) {
    // SAFETY: dispatching through the active fn-pointer global.
    unsafe { (crate::parse::xmlFree)(ptr) }
}

/// libxml2 `xmlMemSize(ptr)` — allocation size lookup.  Always
/// returns 0 in our build: matches libxml2 compiled without
/// `WITH_MEM_DEBUG`, where the same function returns 0.  Consumers
/// that branch on this degrade to "no size info" rather than misreport.
///
/// (`xmlMemUsed` and `xmlMemBlocks` follow the same convention and
/// live in [`crate::misc`] alongside other always-zero introspection
/// stubs; we don't duplicate them here.)
///
/// # Safety
///
/// `ptr` unused; safe to call on any value.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemSize(_ptr: *mut c_void) -> usize { 0 }

/// libxml2 `xmlCheckUTF8(utf)` — validate a NUL-terminated string as
/// UTF-8.  Returns 1 if valid, 0 if invalid or NULL.  libxslt uses
/// this on values it round-trips through `xsl:value-of`, so a stub
/// returning 0 on valid input would make every non-ASCII transform
/// look invalid.
///
/// The libxml2 docstring is misleading ("length of the string in
/// characters") — actual behaviour in `xmlstring.c` is a 1/0
/// boolean.  Consumers that branch on the return value treat it as
/// boolean, and the comparison harness in
/// `tests/abi-system/comparison/` pins this contract.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCheckUTF8(utf: *const u8) -> c_int {
    if utf.is_null() {
        return 0;
    }
    // SAFETY: caller asserts NUL-terminated.  We walk to the NUL,
    // then run std's UTF-8 validator over the slice.
    let cstr = unsafe { std::ffi::CStr::from_ptr(utf as *const std::os::raw::c_char) };
    match std::str::from_utf8(cstr.to_bytes()) {
        Ok(_)  => 1,
        Err(_) => 0,
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn alloc_then_take_then_free() {
        let p = alloc_registered_cstring(b"hello");
        assert!(!p.is_null());
        // Registered → take_alloc should succeed.
        assert!(take_alloc(p as *const u8));
        // Second take_alloc should fail (already taken).
        assert!(!take_alloc(p as *const u8));
        // Registry pointers are libc-allocated; release via libc free.
        unsafe { libc_free(p as *mut c_void); }
    }

    #[test]
    fn take_unregistered_returns_false() {
        // A stack address is definitely not in our registry.
        let local = 0u8;
        assert!(!take_alloc(&local as *const u8));
    }

    #[test]
    fn null_safe() {
        register_alloc(std::ptr::null());      // no-op
        assert!(!take_alloc(std::ptr::null())); // false
    }

    #[test]
    fn content_round_trip() {
        let p = alloc_registered_cstring(b"the quick brown fox");
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "the quick brown fox");
        assert!(take_alloc(p as *const u8));
        unsafe { libc_free(p as *mut c_void); }
    }

    #[test]
    fn cstring_reallocs_through_one_allocator() {
        // A registered cstring and `xmlRealloc` must share one
        // allocator: the cstring is libc-malloc'd, so realloc takes the
        // registered (libc `realloc`) path and free is libc `free`.
        // The bug this guards was a Rust-allocated CString being
        // realloc'd through libc — corruption under a non-default
        // global allocator.
        let p = alloc_registered_cstring(b"hello world");
        assert!(!p.is_null());
        let grown = unsafe { impl_xml_realloc(p as *mut c_void, 64) };
        assert!(!grown.is_null());
        let s = unsafe { CStr::from_ptr(grown as *const std::os::raw::c_char) }
            .to_str()
            .unwrap();
        assert_eq!(s, "hello world");
        assert!(take_alloc(grown as *const u8));
        unsafe { libc_free(grown); }
    }

    #[test]
    fn xml_realloc_round_trips_through_registry() {
        // Allocate via xmlMalloc → registered → realloc → updated
        // registry → free.
        let p1 = unsafe { impl_xml_malloc(16) };
        assert!(!p1.is_null());
        let p2 = unsafe { impl_xml_realloc(p1, 64) };
        assert!(!p2.is_null());
        // p1 is no longer registered (take_alloc consumed it).
        // p2 should be.
        assert!(take_alloc(p2 as *const u8), "p2 should be registered");
        // Free via the same path xmlFree would use.
        unsafe { libc_free(p2); }
    }

    #[test]
    fn xml_realloc_copies_arena_pointer_to_heap() {
        // Earlier version of this test asserted that
        // `impl_xml_realloc` returns NULL for any unregistered
        // pointer, on the theory that libxslt's xsltAddTextString
        // would interpret NULL as "allocation failed" and bail
        // cleanly.  That turned out to be wrong: xsltAddTextString
        // reads `target->content` (an arena pointer for text nodes
        // we built) and realloc's it to grow the buffer, then
        // propagates a NULL realloc result up as "Failed to copy
        // the string."
        //
        // Updated contract: when given a non-registered pointer
        // that looks like a NUL-terminated xmlChar* (our arena
        // text-node content), we copy the bytes into a fresh heap
        // buffer of the requested size and return that.  Subsequent
        // realloc/free calls follow the normal heap path.
        let arena: &[u8] = b"hello\0";
        let p = arena.as_ptr() as *mut c_void;
        let got = unsafe { impl_xml_realloc(p, 32) };
        assert!(!got.is_null());
        // Verify the new buffer contains the original NUL-terminated
        // bytes (NUL terminator is included).
        let copied = unsafe {
            std::slice::from_raw_parts(got as *const u8, 6)
        };
        assert_eq!(copied, arena);
        // Registered, so xmlFree releases it cleanly.
        assert!(take_alloc(got as *const u8));
        unsafe { libc_free(got); }
    }

    #[test]
    fn xml_realloc_null_acts_like_malloc() {
        let p = unsafe { impl_xml_realloc(std::ptr::null_mut(), 32) };
        assert!(!p.is_null());
        assert!(take_alloc(p as *const u8));
        unsafe { libc_free(p); }
    }

    // Direct libc free for test cleanup — we just want to release
    // the allocation without going through xmlFree's CString path.
    unsafe extern "C" {
        #[link_name = "free"]
        fn libc_free(ptr: *mut c_void);
    }

    // ── xmlMem* family tests ──────────────────────────────────────────
    //
    // These tests swap the global allocator hooks via xmlMemSetup,
    // exercise them, then restore the defaults.  Because the globals
    // are shared process-wide and cargo runs unit tests on multiple
    // threads by default, we serialise via a Mutex so two tests can't
    // race each other on the swap.

    use std::sync::Mutex;

    static MEM_SWAP_LOCK: Mutex<()> = Mutex::new(());

    // Counters bumped by the test allocator hooks below.  Reset at
    // the start of each test that uses them.
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEST_MALLOC_CALLS:  AtomicU64 = AtomicU64::new(0);
    static TEST_FREE_CALLS:    AtomicU64 = AtomicU64::new(0);
    static TEST_REALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn test_malloc(size: usize) -> *mut c_void {
        TEST_MALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
        unsafe { malloc(size) }
    }
    unsafe extern "C" fn test_realloc(p: *mut c_void, size: usize) -> *mut c_void {
        TEST_REALLOC_CALLS.fetch_add(1, Ordering::SeqCst);
        unsafe { realloc(p, size) }
    }
    unsafe extern "C" fn test_free(p: *mut c_void) {
        TEST_FREE_CALLS.fetch_add(1, Ordering::SeqCst);
        unsafe { libc_free(p); }
    }

    /// Take the current allocator hooks, install the test ones, run
    /// the closure, then restore.  Returns whatever the closure
    /// returned.
    fn with_test_allocator<F: FnOnce() -> T, T>(f: F) -> T {
        let _guard = MEM_SWAP_LOCK.lock().unwrap();
        TEST_MALLOC_CALLS.store(0,  Ordering::SeqCst);
        TEST_FREE_CALLS.store(0,    Ordering::SeqCst);
        TEST_REALLOC_CALLS.store(0, Ordering::SeqCst);
        // Snapshot the defaults so we can restore.
        let mut prev_free:    XmlFreeFunc    = None;
        let mut prev_malloc:  XmlMallocFunc  = None;
        let mut prev_realloc: XmlReallocFunc = None;
        unsafe {
            xmlMemGet(&mut prev_free, &mut prev_malloc, &mut prev_realloc, std::ptr::null_mut());
            let rc = xmlMemSetup(Some(test_free), Some(test_malloc), Some(test_realloc), None);
            assert_eq!(rc, 0);
        }
        let out = f();
        // Restore.  We assume the original three were non-NULL; this
        // is true because the module's static initialisers point at
        // impl_xml_* / xml_free_impl.
        unsafe {
            let rc = xmlMemSetup(prev_free, prev_malloc, prev_realloc, None);
            assert_eq!(rc, 0);
        }
        out
    }

    #[test]
    fn mem_setup_routes_xml_malloc_through_caller() {
        with_test_allocator(|| {
            // Allocate through the dispatch global; should hit
            // test_malloc, not impl_xml_malloc.
            let p = unsafe { (xmlMalloc)(32) };
            assert!(!p.is_null());
            assert_eq!(TEST_MALLOC_CALLS.load(Ordering::SeqCst), 1);
            unsafe { (crate::parse::xmlFree)(p); }
            assert_eq!(TEST_FREE_CALLS.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn mem_malloc_and_mem_free_go_through_hooks() {
        with_test_allocator(|| {
            // xmlMemMalloc / xmlMemFree are explicit entry points
            // that dispatch through the same fn-pointer globals.
            let p = unsafe { xmlMemMalloc(64) };
            assert!(!p.is_null());
            unsafe { xmlMemFree(p); }
            assert_eq!(TEST_MALLOC_CALLS.load(Ordering::SeqCst), 1);
            assert_eq!(TEST_FREE_CALLS.load(Ordering::SeqCst),   1);
        });
    }

    #[test]
    fn mem_setup_rejects_null_required_hooks() {
        // free/malloc/realloc all required; NULL → -1, no swap.
        let rc = unsafe {
            xmlMemSetup(None, Some(test_malloc), Some(test_realloc), None)
        };
        assert_eq!(rc, -1);
    }

    #[test]
    fn mem_get_reports_current_hooks() {
        let mut f:  XmlFreeFunc    = None;
        let mut m:  XmlMallocFunc  = None;
        let mut r:  XmlReallocFunc = None;
        unsafe { xmlMemGet(&mut f, &mut m, &mut r, std::ptr::null_mut()); }
        assert!(f.is_some());
        assert!(m.is_some());
        assert!(r.is_some());
    }

    #[test]
    fn mem_size_returns_zero() {
        // Documented behaviour for libxml2 builds without WITH_MEM_DEBUG.
        assert_eq!(unsafe { xmlMemSize(std::ptr::null_mut()) }, 0);
    }
}
