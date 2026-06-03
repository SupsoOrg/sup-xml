//! Per-document string-interning dict with refcounted ownership.
//!
//! A [`Dict`] gives every distinct byte sequence a stable, dict-owned,
//! NUL-terminated pointer.  Looking up the same bytes twice returns
//! the same pointer; consumers may rely on pointer equality to test
//! "is this the same name?" without a `strcmp`.
//!
//! # Refcount model
//!
//! Each `Dict` carries an internal atomic refcount, mirroring
//! libxml2's `xmlDict` ownership convention.  This lets multiple
//! independent consumers — a parser context, a built document, the
//! C-ABI consumer's own handle — co-own the same dict without
//! coordination beyond `xmlDictReference` / `xmlDictFree`.
//!
//! * [`Dict::new_refcounted`] returns a freshly heap-allocated dict
//!   at refcount 1 (one outstanding reference, owned by the caller).
//! * [`Dict::add_ref`] bumps the count for an additional borrow.
//! * [`Dict::release`] decrements; the last release drops the dict
//!   and frees every interned string.
//!
//! Atomic ordering uses Release on decrement and an Acquire fence on
//! the last release — the standard pattern (cf. `std::sync::Arc`).
//!
//! # Why interning instead of arena allocation
//!
//! Element / attribute / namespace names in XML repeat heavily.  An
//! HTML page has thousands of `<p>` / `<a>` / `<div>` tags; an OSM
//! dump has millions of `<node>` records.  Bumpalo-arena allocation
//! is fast but stores each occurrence as a separate copy — the same
//! name string ends up at thousands of distinct heap addresses,
//! defeating pointer equality and wasting space.
//!
//! The dict trades a hashmap lookup per *unique* name for one
//! allocation per unique name and constant-cost equality checks
//! across all occurrences.  For docs with high name repetition the
//! total bytes-stored often drops by 10×.
//!
//! Content strings (text node bodies, attribute values) are NOT
//! interned — they're typically unique per occurrence and would
//! waste memory hashing things that never collide.  They stay in
//! the document's bumpalo arena.
//!
//! # Performance notes
//!
//! * The hashmap uses `std`'s default `RandomState` (SipHash).  For
//!   XML names (typically ≤16 bytes) that's a few-ns hash; the more
//!   expensive path is the hashmap probe + alloc on miss.  A fast
//!   non-DoS-resistant hasher (FxHash / ahash) would shave ~20-30%
//!   off the hash cost — worth doing if profiling identifies it.
//! * Storage uses one `Box<[u8]>` per unique name.  An alternative
//!   would back the strings with a bumpalo-style arena owned by the
//!   dict itself, paying only one allocation per insert and freeing
//!   everything wholesale on Dict drop.  The cost of the doubled
//!   allocation is amortised across all *occurrences* of the name,
//!   which is usually many — switch only if profiling demands.
//! * The hot path (lookup of an already-interned name) is one hash
//!   + one byte comparison.  No allocation.  Designed to be cheap
//!   enough to call on every element / attribute name during parse.

#![allow(unsafe_code)] // see module docs

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use bumpalo::Bump;

/// A refcounted per-document string interner.
///
/// Never construct directly with `Dict { ... }` — use
/// [`Dict::new_refcounted`] which guarantees the heap allocation and
/// initial refcount of 1.  Internally not `Send`/`Sync` despite the
/// atomic refcount: the `RefCell` guarding the table is single-
/// threaded.  C-ABI consumers (libxml2) are GIL-protected on the
/// Python side and single-thread the dict accordingly.
pub struct Dict {
    refcount: AtomicUsize,
    /// Parent dict for a sub-dictionary (libxml2 `xmlDictCreateSub`),
    /// or NULL for a root dict.  A sub-dict shares the parent's
    /// interned strings: a lookup that the parent already holds
    /// returns the *parent's* canonical pointer, so pointer-equality
    /// holds across the boundary.  This is load-bearing for libxslt,
    /// whose transform-context dict is a sub-dict of the stylesheet
    /// dict and which compares interned variable-name pointers.
    ///
    /// The sub-dict owns one reference to its parent (taken at
    /// creation, released when the sub-dict is freed).
    parent:   *mut Dict,
    /// `Mutex`-guarded because lxml's threaded operations (e.g.
    /// `moveNodeToDocument` called from one thread on a doc parsed
    /// in another) cross the GIL boundary — lxml releases the GIL
    /// inside its `cdef` parser paths.  Two threads concurrently
    /// calling `xmlDictLookup` on the same dict is therefore a
    /// realistic occurrence; a `RefCell` panics in that case.
    /// `Mutex` serialises the access correctly with negligible
    /// overhead on the contended path (one atomic CAS per call).
    inner:    Mutex<DictInner>,
}

struct DictInner {
    /// Key: input bytes (no NUL).  Value: NUL-terminated canonical.
    table: HashMap<Vec<u8>, Box<[u8]>>,
    /// Side set keyed by canonical pointer address for O(1) `owns`.
    owned: HashSet<usize>,
    /// Origin document arenas retained on behalf of a cross-thread node
    /// move.  When lxml moves a node to a document on another thread it
    /// re-interns the node's name out of the origin dict into this
    /// (destination-thread) dict; the node *memory*, however, stays in
    /// the origin document's arena.  Holding an `Arc` clone of that
    /// arena keeps the moved node alive for this dict's lifetime — which
    /// spans the destination thread, hence every destination document.
    /// Deduped by arena pointer; empty until the first cross-thread
    /// graft.
    retained_arenas: Vec<Arc<Bump>>,
}

impl Dict {
    /// Allocate a fresh dict on the heap.  Refcount starts at 1
    /// (the caller's reference).  Returns a raw pointer; callers
    /// must eventually balance with [`Dict::release`].
    pub fn new_refcounted() -> *mut Dict {
        Self::new_with_parent(std::ptr::null_mut())
    }

    /// Allocate a sub-dictionary chained to `parent` (libxml2
    /// `xmlDictCreateSub`).  The sub-dict shares the parent's interned
    /// strings — see the [`parent`](Self::parent) field — and takes one
    /// reference to it, released when the sub-dict is freed.  A NULL
    /// `parent` is equivalent to [`new_refcounted`](Self::new_refcounted).
    ///
    /// # Safety
    ///
    /// `parent` must be NULL or a live `Dict*` on which the caller can
    /// take a reference.
    pub unsafe fn new_sub(parent: *mut Dict) -> *mut Dict {
        if !parent.is_null() {
            // SAFETY: caller asserts `parent` is live.
            unsafe { (*parent).add_ref(); }
        }
        Self::new_with_parent(parent)
    }

    fn new_with_parent(parent: *mut Dict) -> *mut Dict {
        let boxed = Box::new(Self {
            refcount: AtomicUsize::new(1),
            parent,
            inner:    Mutex::new(DictInner {
                table: HashMap::new(),
                owned: HashSet::new(),
                retained_arenas: Vec::new(),
            }),
        });
        Box::into_raw(boxed)
    }

    /// Canonical pointer for `input` if any ancestor dict already
    /// interned it, NULL otherwise.  Walks the parent chain only —
    /// the local table is checked by the callers.
    fn ancestor_lookup(&self, input: &[u8]) -> *const u8 {
        let mut cur = self.parent;
        while !cur.is_null() {
            // SAFETY: a sub-dict holds a reference to its parent for its
            // whole lifetime, so the chain stays live while `self` does.
            let d = unsafe { &*cur };
            let hit = d.inner.lock().expect("Dict mutex poisoned")
                .table.get(input).map(|c| c.as_ptr());
            if let Some(p) = hit { return p; }
            cur = d.parent;
        }
        std::ptr::null()
    }

    /// Retain `arena` (an origin document's node arena) so a node moved
    /// out of it — whose name re-interned into this dict during a
    /// cross-thread graft — outlives a drop of its origin document.
    /// Deduped by pointer, so repeated grafts from one source arena keep
    /// a single entry.  The caller guarantees `arena` is not the
    /// destination's own (a same-arena move needs no retention).
    pub fn retain_arena(&self, arena: Arc<Bump>) {
        let mut g = self.inner.lock().unwrap();
        if g.retained_arenas.iter().any(|a| Arc::ptr_eq(a, &arena)) {
            return;
        }
        g.retained_arenas.push(arena);
    }

    /// Increment the refcount.  Use when copying a `*mut Dict` to a
    /// new owner.  Cheap atomic op.
    pub fn add_ref(&self) {
        self.refcount.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the refcount.  When the last reference goes away,
    /// drops every interned string and the dict itself.  Returns
    /// `true` if this call was the one that freed.
    ///
    /// # Safety
    ///
    /// `ptr` must be a non-null pointer previously returned by
    /// [`Dict::new_refcounted`], and the caller must own one
    /// reference (i.e. exactly one outstanding `add_ref` not yet
    /// paired with a `release`).
    pub unsafe fn release(ptr: *mut Dict) -> bool {
        // SAFETY: caller asserts `ptr` is live and they own a ref.
        let prev = unsafe { (*ptr).refcount.fetch_sub(1, Ordering::Release) };
        if prev == 1 {
            // Last reference — synchronise with previous releases so
            // we observe their pre-release writes, then drop.
            std::sync::atomic::fence(Ordering::Acquire);
            // A sub-dict holds one reference to its parent; release it
            // after the child is gone.  Read it out before the box drop.
            let parent = unsafe { (*ptr).parent };
            // SAFETY: refcount went 1 → 0; we hold the only reference;
            // no other thread can observe the dict after this point.
            unsafe { drop(Box::from_raw(ptr)); }
            if !parent.is_null() {
                // SAFETY: the sub-dict owned this reference for its lifetime.
                unsafe { Dict::release(parent); }
            }
            true
        } else {
            false
        }
    }

    /// Current refcount.  For tests + debugging; not load-bearing.
    pub fn refcount(&self) -> usize {
        self.refcount.load(Ordering::Relaxed)
    }

    /// Look up `input`, inserting if absent.  Returns the canonical
    /// NUL-terminated pointer.
    pub fn intern(&self, input: &[u8]) -> *const u8 {
        // Reuse an ancestor's canonical pointer when present so interned
        // pointers are equal across the parent/child boundary.
        let ancestor = self.ancestor_lookup(input);
        if !ancestor.is_null() {
            return ancestor;
        }
        let mut inner = self.inner.lock().expect("Dict mutex poisoned");
        if let Some(canonical) = inner.table.get(input) {
            return canonical.as_ptr();
        }
        let mut buf = Vec::with_capacity(input.len() + 1);
        buf.extend_from_slice(input);
        buf.push(0);
        let canonical: Box<[u8]> = buf.into_boxed_slice();
        let ptr = canonical.as_ptr();
        inner.owned.insert(ptr as usize);
        inner.table.insert(input.to_vec(), canonical);
        ptr
    }

    /// `&str`-keyed convenience over [`Self::intern`].
    pub fn intern_str(&self, s: &str) -> *const u8 {
        self.intern(s.as_bytes())
    }

    /// Non-allocating lookup.  Returns the canonical pointer on hit,
    /// NULL on miss.
    pub fn lookup(&self, input: &[u8]) -> *const u8 {
        let local = self.inner
            .lock().expect("Dict mutex poisoned")
            .table
            .get(input)
            .map(|c| c.as_ptr());
        match local {
            Some(p) => p,
            None => self.ancestor_lookup(input),
        }
    }

    /// Does `ptr` originate from this dict or any ancestor?  libxml2's
    /// `xmlDictOwns` walks the sub-dict chain, so a string interned in
    /// the parent is "owned" when probed through a child.
    pub fn owns(&self, ptr: *const u8) -> bool {
        if self.inner
            .lock().expect("Dict mutex poisoned")
            .owned.contains(&(ptr as usize))
        {
            return true;
        }
        let mut cur = self.parent;
        while !cur.is_null() {
            // SAFETY: parent chain is kept live by the sub-dict's reference.
            let d = unsafe { &*cur };
            if d.inner.lock().expect("Dict mutex poisoned")
                .owned.contains(&(ptr as usize))
            {
                return true;
            }
            cur = d.parent;
        }
        false
    }

    /// Number of distinct strings interned.
    pub fn len(&self) -> usize {
        self.inner.lock().expect("Dict mutex poisoned").table.len()
    }

    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refcount_lifecycle() {
        let p = Dict::new_refcounted();
        assert_eq!(unsafe { (*p).refcount() }, 1);
        unsafe { (*p).add_ref(); }
        assert_eq!(unsafe { (*p).refcount() }, 2);
        let freed = unsafe { Dict::release(p) };
        assert!(!freed);
        assert_eq!(unsafe { (*p).refcount() }, 1);
        let freed = unsafe { Dict::release(p) };
        assert!(freed);
    }

    #[test]
    fn intern_returns_stable_pointer() {
        let p = Dict::new_refcounted();
        let d = unsafe { &*p };
        let p1 = d.intern(b"div");
        let p2 = d.intern(b"div");
        assert_eq!(p1, p2);
        assert_eq!(unsafe { *p1.add(3) }, 0);
        let slice = unsafe { std::slice::from_raw_parts(p1, 3) };
        assert_eq!(slice, b"div");
        unsafe { Dict::release(p); }
    }

    #[test]
    fn different_inputs_different_pointers() {
        let p = Dict::new_refcounted();
        let d = unsafe { &*p };
        let pa = d.intern(b"p");
        let pb = d.intern(b"span");
        assert_ne!(pa, pb);
        assert_eq!(d.len(), 2);
        unsafe { Dict::release(p); }
    }

    #[test]
    fn owns_recognises_canonicals_only() {
        let p = Dict::new_refcounted();
        let d = unsafe { &*p };
        let canonical = d.intern(b"strong");
        assert!(d.owns(canonical));
        let foreign: *const u8 = b"strong".as_ptr();
        assert!(!d.owns(foreign));
        unsafe { Dict::release(p); }
    }

    #[test]
    fn lookup_returns_null_on_miss() {
        let p = Dict::new_refcounted();
        let d = unsafe { &*p };
        assert!(d.lookup(b"unknown").is_null());
        d.intern(b"known");
        assert!(!d.lookup(b"known").is_null());
        unsafe { Dict::release(p); }
    }

    #[test]
    fn sub_dict_shares_parent_interned_pointers() {
        // libxslt interns a variable name in the stylesheet (parent)
        // dict, then re-looks it up through the transform (sub) dict and
        // compares the two pointers for identity.  The sub-dict must
        // return the parent's canonical pointer for that to hold.
        let parent = Dict::new_refcounted();
        let p = unsafe { &*parent };
        let name_in_parent = p.intern(b"v");

        let sub = unsafe { Dict::new_sub(parent) };
        let s = unsafe { &*sub };
        // Re-interning through the child yields the parent's pointer.
        assert_eq!(s.intern(b"v"), name_in_parent);
        // lookup() and owns() both chain to the parent.
        assert_eq!(s.lookup(b"v"), name_in_parent);
        assert!(s.owns(name_in_parent));
        // A name only the child holds stays local to the child.
        let child_only = s.intern(b"child");
        assert!(s.owns(child_only));
        assert!(!p.owns(child_only));
        assert!(p.lookup(b"child").is_null());

        // Creating the sub took a reference to the parent; releasing the
        // sub releases it, leaving the caller's original parent ref.
        assert_eq!(p.refcount(), 2);
        unsafe { Dict::release(sub); }
        assert_eq!(p.refcount(), 1);
        unsafe { Dict::release(parent); }
    }

    #[test]
    fn retains_foreign_arena_and_keeps_it_alive() {
        use std::sync::Weak;
        // A destination dict retaining a foreign (origin) arena must
        // keep that arena's memory alive even after every other holder
        // drops it — the cross-thread-graft invariant.
        let dst = Dict::new_refcounted();
        let dst_d = unsafe { &*dst };

        let weak: Weak<Bump>;
        {
            // Stand-in for the origin document's arena.
            let src_arena = Arc::new(Bump::new());
            weak = Arc::downgrade(&src_arena);
            // The move hook retains the origin arena onto the dest dict.
            dst_d.retain_arena(Arc::clone(&src_arena));
            // `src_arena` goes out of scope here; only the destination
            // dict's retention keeps the arena alive.
        }
        assert!(weak.upgrade().is_some(), "destination dict must keep the foreign arena alive");

        // Dedup: retaining the same arena twice keeps a single entry.
        let again = weak.upgrade().unwrap();
        dst_d.retain_arena(Arc::clone(&again));
        assert_eq!(dst_d.inner.lock().unwrap().retained_arenas.len(), 1);

        // Releasing the destination dict drops the last reference.
        drop(again);
        unsafe { Dict::release(dst); }
        assert!(weak.upgrade().is_none(), "arena should free once the destination dict drops");
    }
}
