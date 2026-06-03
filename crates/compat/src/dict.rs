//! `xmlDict*` ABI surface, wired to [`sup_xml_tree::dict::Dict`].
//!
//! libxml2 exposes a refcounted string-intern dictionary that
//! parsers, documents, and consumers all co-own.  We expose the
//! same surface; storage lives in the shared `Dict` type defined in
//! the tree crate.
//!
//! # Ownership contract
//!
//! * [`xmlDictCreate`] returns a fresh dict at refcount 1 (the
//!   caller's reference).
//! * [`xmlDictReference`] bumps; [`xmlDictFree`] decrements.  The
//!   last decrement frees the dict and every interned string.
//! * [`xmlDictLookup`] / [`xmlDictExists`] return canonical pointers
//!   owned by the dict; their lifetime is the dict's, NOT the
//!   caller's.  Callers MUST NOT `xmlFree` these pointers.
//! * [`xmlDictOwns`] tells callers whether a pointer originated
//!   here — the standard "do I free this or not?" probe.

use std::cell::Cell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::sync::Arc;

use bumpalo::Bump;
use sup_xml_tree::dict::Dict;

// ── thread-local shared dict ────────────────────────────────────────────────
//
// libxml2's per-thread parser context owns one `xmlDict*` that every
// document parsed on that thread shares.  Names interned via any of
// those parses end up pointer-equal across all docs in the thread;
// the dict outlives any individual doc and is released only when the
// last consumer (last doc + the thread context itself) drops its ref.
//
// We mirror that model so cross-document operations — chiefly lxml's
// `_appendChild` / `moveNodeToDocument`, which directly read names
// out of one document's tree and store them in another's — never see
// a name pointer that became invalid when its source doc was freed.
// All documents created through this compat layer adopt this single
// thread-local dict; name pointers stay valid as long as any
// participant in the thread holds a reference.
//
// The dict is lazily created on first use, lives until the thread
// dies, and surrenders one reference at thread-local drop time.
// Each call to [`thread_dict`] returns the same raw pointer; callers
// that want their own ownership stake call [`xmlDictReference`] to
// bump.

struct ThreadDictSlot {
    /// The shared `Dict*`.  NULL means uninitialized; first access
    /// allocates.  Cell so we can fill it from `&self` on first use.
    ptr: Cell<*mut Dict>,
}

impl Drop for ThreadDictSlot {
    fn drop(&mut self) {
        let p = self.ptr.get();
        if !p.is_null() {
            // Release the thread's own reference.  Other holders
            // (docs that haven't been freed yet, dangling
            // `xmlDictReference`'d handles) keep the dict alive
            // until they release; this is just the thread context
            // letting go of its stake.
            unsafe { Dict::release(p); }
        }
    }
}

thread_local! {
    static THREAD_DICT: ThreadDictSlot = ThreadDictSlot {
        ptr: Cell::new(std::ptr::null_mut()),
    };
}

thread_local! {
    /// Records the most recent `xmlDictOwns(src, name) -> true` so the
    /// `xmlDictLookup(dst, name)` that lxml issues immediately after can
    /// recognise a cross-thread name re-intern and pin the origin arena
    /// onto the destination dict.
    ///
    /// lxml's `moveNodeToDocument` calls `_fixThreadDictPtr` when the
    /// source and destination dicts differ (i.e. a node moved between
    /// threads); that helper does exactly
    /// `if xmlDictOwns(src, s): s = xmlDictLookup(dst, s)`.  The moved
    /// node's name is re-homed into `dst`, but the node *memory* stays
    /// in the origin arena — which the origin document would otherwise
    /// free.  Capturing the `(src dict, name)` pair here lets the
    /// following lookup retain that arena.  See [`xmlDictLookup`].
    static PENDING_REINTERN: Cell<(*const Dict, *const c_char)> =
        const { Cell::new((std::ptr::null(), std::ptr::null())) };
}

thread_local! {
    /// The origin arena of the node most recently passed to
    /// `xmlUnlinkNode`.  With per-document arenas, a cross-thread move
    /// must pin the moved node's origin arena onto the destination, but
    /// lxml relinks the node by direct pointer writes — the only ABI
    /// call on its move path that still names the origin is the
    /// `xmlUnlinkNode` it issues first.  We stash that node's arena here
    /// so the subsequent cross-thread `xmlDictLookup` (the name
    /// re-intern) can retain it.  See `mutate::xmlUnlinkNode`.
    static PENDING_GRAFT_ARENA: std::cell::RefCell<Option<Arc<Bump>>> =
        const { std::cell::RefCell::new(None) };
}

/// Record the origin arena of a node about to be moved (called from
/// `xmlUnlinkNode`).  Overwrites any previous pending arena — only the
/// most recent unlink, which immediately precedes the move's name
/// re-intern, is relevant.
pub(crate) fn stash_graft_source_arena(arena: Arc<Bump>) {
    PENDING_GRAFT_ARENA.with(|p| *p.borrow_mut() = Some(arena));
}

/// The thread-local shared `Dict*`.  Lazily initialized; returns the
/// same raw pointer for every call on the same thread.  Refcount
/// already accounts for the thread's own outstanding reference;
/// callers that want their own stake must call [`Dict::add_ref`] (or
/// [`xmlDictReference`]) explicitly.
pub(crate) fn thread_dict() -> *mut Dict {
    THREAD_DICT.with(|s| {
        let p = s.ptr.get();
        if !p.is_null() {
            return p;
        }
        // Lazy init: refcount=1 for the thread's own ref.  Drops
        // when ThreadDictSlot drops at thread exit.
        let fresh = Dict::new_refcounted();
        s.ptr.set(fresh);
        fresh
    })
}

// ── per-document node arenas ────────────────────────────────────────────────
//
// libxml2 mallocs each node individually, so a node's memory is
// independent of any single document — moving a node between trees
// just relinks pointers.  Our nodes live in a bump `Arena`, which
// couples their lifetime to the bump's.  Two invariants reconcile that
// with libxml2's contract:
//
// 1. Each document gets its OWN arena ([`new_doc_arena`]).  Concurrent
//    work on different documents therefore never allocates into the
//    same `Bump` — `Bump` is `!Sync`, and the shared-per-thread arena
//    this replaced raced when one thread parsed (allocating) while
//    another mutated a node it had produced.
//
// 2. Every arena is registered in a thread-local keep-alive list and
//    held until the thread exits.  A node grafted between two documents
//    on the SAME thread (lxml's `_appendChild` does this via
//    Cython-internal `_linkChild` + direct field writes, bypassing
//    every ABI symbol, so we get no graft hook) stays in its origin
//    arena; the keep-alive list guarantees that arena outlives every
//    document on the thread.  Cross-THREAD grafts, where the origin
//    thread may exit first, are covered separately by retaining the
//    origin arena onto the destination dict (see `mutate.rs`).
//
// Memory profile matches the previous shared-arena model: arenas are
// reclaimed at thread exit, not per-document.
//
// XML names live in [`thread_dict`]; their bytes are independent of
// these arenas.

struct ThreadArenaKeepAlive {
    arenas: std::cell::RefCell<Vec<Arc<Bump>>>,
}

thread_local! {
    static THREAD_ARENAS: ThreadArenaKeepAlive = ThreadArenaKeepAlive {
        arenas: std::cell::RefCell::new(Vec::new()),
    };
}

/// Allocate a fresh node arena for one document and register it to stay
/// alive until the thread exits.  Returns the caller's owning clone;
/// the thread-local keep-alive holds an independent clone, so a node
/// grafted into another same-thread document never dangles when its
/// origin document drops.
pub(crate) fn new_doc_arena() -> Arc<Bump> {
    let arena = Arc::new(Bump::new());
    THREAD_ARENAS.with(|k| k.arenas.borrow_mut().push(Arc::clone(&arena)));
    arena
}

/// `xmlDictCreate()` — fresh dict at refcount 1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictCreate() -> *mut std::os::raw::c_void {
    Dict::new_refcounted() as *mut std::os::raw::c_void
}

/// `xmlDictCreateSub(parent)` — sub-dictionary chained to `parent`.
///
/// The sub-dict shares the parent's interned strings: a lookup the
/// parent already holds returns the parent's canonical pointer, so a
/// string interned in the parent is pointer-equal when re-looked-up
/// through the child.  This is load-bearing for libxslt — its
/// transform-context dict is a sub-dict of the stylesheet dict, and
/// `xsltXPathVariableLookup` interns the looked-up name in the
/// transform dict then compares it *by pointer* against the variable's
/// name (interned in the parent at compile time).  Without the shared
/// chain those pointers differ and every local-variable reference made
/// while a user parameter is in play resolves to "undefined variable".
///
/// Takes one reference to `parent` (released when the sub-dict is
/// freed).  A NULL parent yields a plain root dict.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictCreateSub(
    parent: *mut std::os::raw::c_void,
) -> *mut std::os::raw::c_void {
    unsafe { Dict::new_sub(parent as *mut Dict) as *mut std::os::raw::c_void }
}

/// `xmlDictReference(dict)` — bump refcount.  Returns 0 / -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictReference(
    dict: *mut std::os::raw::c_void,
) -> c_int {
    if dict.is_null() { return -1; }
    // SAFETY: caller asserts dict is live with positive refcount.
    unsafe { (*(dict as *const Dict)).add_ref(); }
    0
}

/// `xmlDictFree(dict)` — decrement refcount; free at zero.
/// NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictFree(dict: *mut std::os::raw::c_void) {
    if dict.is_null() { return; }
    // SAFETY: caller asserts dict was returned by xmlDictCreate
    // (or has a positive refcount obtained via xmlDictReference).
    unsafe { Dict::release(dict as *mut Dict); }
}

/// `xmlDictLookup(dict, name, len)` — intern `name`, returning the
/// canonical NUL-terminated pointer.  `len < 0` means "strlen".
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictLookup(
    dict: *mut std::os::raw::c_void,
    name: *const c_char,
    len:  c_int,
) -> *const c_char {
    if dict.is_null() || name.is_null() {
        return std::ptr::null();
    }
    // SAFETY: caller asserts `name` is readable for `len` bytes
    // (NUL-terminated when len < 0).
    let bytes: &[u8] = unsafe {
        if len < 0 {
            CStr::from_ptr(name).to_bytes()
        } else {
            std::slice::from_raw_parts(name as *const u8, len as usize)
        }
    };
    let d: &Dict = unsafe { &*(dict as *const Dict) };
    // Cross-thread re-intern: if `name` is the string a just-preceding
    // `xmlDictOwns(src, name)` reported as owned by a *different* dict,
    // this is lxml re-homing a moved node's name (`_fixThreadDictPtr`).
    // Pin the moved node's origin arena (stashed by the `xmlUnlinkNode`
    // that opened the move) onto this destination dict so the node's
    // memory outlives a drop of its origin document.  Consume both
    // pairings unconditionally so they can't leak into a later lookup.
    let (pending_src, pending_name) =
        PENDING_REINTERN.with(|p| p.replace((std::ptr::null(), std::ptr::null())));
    let graft_arena = PENDING_GRAFT_ARENA.with(|p| p.borrow_mut().take());
    if !pending_src.is_null()
        && pending_name == name
        && pending_src != dict as *const Dict
    {
        if let Some(arena) = graft_arena {
            d.retain_arena(arena);
        }
    }
    d.intern(bytes) as *const c_char
}

/// `xmlDictQLookup(dict, prefix, name)` — intern a QName composed of
/// `prefix:name`, returning the canonical pointer.  When `prefix` is
/// NULL or empty, behaves like [`xmlDictLookup`] on `name` alone.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictQLookup(
    dict:   *mut std::os::raw::c_void,
    prefix: *const c_char,
    name:   *const c_char,
) -> *const c_char {
    if dict.is_null() || name.is_null() {
        return std::ptr::null();
    }
    // SAFETY: caller asserts both pointers are NUL-terminated when non-NULL.
    let name_bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let prefix_bytes: &[u8] = if prefix.is_null() {
        &[]
    } else {
        unsafe { CStr::from_ptr(prefix) }.to_bytes()
    };
    let d: &Dict = unsafe { &*(dict as *const Dict) };
    if prefix_bytes.is_empty() {
        return d.intern(name_bytes) as *const c_char;
    }
    let mut combined = Vec::with_capacity(prefix_bytes.len() + 1 + name_bytes.len());
    combined.extend_from_slice(prefix_bytes);
    combined.push(b':');
    combined.extend_from_slice(name_bytes);
    d.intern(&combined) as *const c_char
}

/// `xmlDictExists(dict, name, len)` — non-allocating lookup.
/// Returns the canonical pointer on hit, NULL on miss.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictExists(
    dict: *mut std::os::raw::c_void,
    name: *const c_char,
    len:  c_int,
) -> *const c_char {
    if dict.is_null() || name.is_null() {
        return std::ptr::null();
    }
    let bytes: &[u8] = unsafe {
        if len < 0 {
            CStr::from_ptr(name).to_bytes()
        } else {
            std::slice::from_raw_parts(name as *const u8, len as usize)
        }
    };
    let d: &Dict = unsafe { &*(dict as *const Dict) };
    d.lookup(bytes) as *const c_char
}

/// `xmlDictSize(dict)` — number of distinct strings interned.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictSize(dict: *mut std::os::raw::c_void) -> c_int {
    if dict.is_null() { return 0; }
    let d: &Dict = unsafe { &*(dict as *const Dict) };
    d.len() as c_int
}

/// `xmlDictOwns(dict, str)` — does this dict own `str`?  Returns 1
/// for canonicals previously returned by lookup / exists, 0
/// otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlDictOwns(
    dict: *mut std::os::raw::c_void,
    s:    *const c_char,
) -> c_int {
    if dict.is_null() || s.is_null() { return 0; }
    let d: &Dict = unsafe { &*(dict as *const Dict) };
    if d.owns(s as *const u8) {
        // Arm the cross-thread re-intern hook: lxml follows an owning
        // `xmlDictOwns(src, s)` with `xmlDictLookup(dst, s)` to re-home
        // a moved node's name.  Record the pair so that lookup can
        // retain the origin arena.
        PENDING_REINTERN.with(|p| p.set((dict as *const Dict, s)));
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refcount_balance() {
        let d = unsafe { xmlDictCreate() };
        assert_eq!(unsafe { xmlDictReference(d) }, 0); // rc=2
        unsafe { xmlDictFree(d); }                     // rc=1
        unsafe { xmlDictFree(d); }                     // rc=0 → free
    }

    #[test]
    fn lookup_returns_stable_pointer() {
        let d = unsafe { xmlDictCreate() };
        let n1 = std::ffi::CString::new("div").unwrap();
        let n2 = std::ffi::CString::new("div").unwrap();
        let p1 = unsafe { xmlDictLookup(d, n1.as_ptr(), -1) };
        let p2 = unsafe { xmlDictLookup(d, n2.as_ptr(), -1) };
        assert_eq!(p1, p2);
        assert_ne!(p1, n1.as_ptr());
        let s = unsafe { CStr::from_ptr(p1) }.to_str().unwrap();
        assert_eq!(s, "div");
        unsafe { xmlDictFree(d); }
    }

    #[test]
    fn owns_recognises_canonicals_only() {
        let d = unsafe { xmlDictCreate() };
        let n = std::ffi::CString::new("strong").unwrap();
        let p = unsafe { xmlDictLookup(d, n.as_ptr(), -1) };
        assert_eq!(unsafe { xmlDictOwns(d, p) }, 1);
        assert_eq!(unsafe { xmlDictOwns(d, n.as_ptr()) }, 0);
        unsafe { xmlDictFree(d); }
    }

    #[test]
    fn explicit_length_respected() {
        let d = unsafe { xmlDictCreate() };
        let buf = std::ffi::CString::new("divxxx").unwrap();
        let p = unsafe { xmlDictLookup(d, buf.as_ptr(), 3) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "div");
        unsafe { xmlDictFree(d); }
    }

    #[test]
    fn null_safety() {
        unsafe { xmlDictFree(std::ptr::null_mut()); }
        assert_eq!(unsafe { xmlDictReference(std::ptr::null_mut()) }, -1);
        assert_eq!(unsafe { xmlDictSize(std::ptr::null_mut()) }, 0);
        assert!(unsafe { xmlDictLookup(std::ptr::null_mut(), std::ptr::null(), 0) }.is_null());
    }

    #[test]
    fn qlookup_combines_prefix_and_local() {
        let d = unsafe { xmlDictCreate() };
        let prefix = std::ffi::CString::new("dc").unwrap();
        let name   = std::ffi::CString::new("title").unwrap();
        let p = unsafe { xmlDictQLookup(d, prefix.as_ptr(), name.as_ptr()) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "dc:title");
        // NULL prefix → same as plain xmlDictLookup(name).
        let p2 = unsafe { xmlDictQLookup(d, std::ptr::null(), name.as_ptr()) };
        let p3 = unsafe { xmlDictLookup(d, name.as_ptr(), -1) };
        assert_eq!(p2, p3, "interned pointers for the same key must collapse");
        unsafe { xmlDictFree(d); }
    }
}
