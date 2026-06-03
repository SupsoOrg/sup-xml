//! `xmlHash*` — libxml2's public hash-table API surfaced as opaque
//! handles backed by Rust's [`HashMap`].
//!
//! libxml2 exposes its internal hash for callers that want to share
//! the same data structure (xmlsec, libxslt, etc.) — and lxml uses
//! it indirectly via the parser dictionary.  We expose enough of
//! the surface that consumers calling `xmlHashLookup`/`Scan`/`Size`
//! get sensible answers; full feature parity with libxml2's hash
//! (quadratic probing, dict integration, etc.) isn't required.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

/// `xmlHashTable` — opaque to C.  Backed by a refcell'd HashMap so
/// the same handle can be looked-up + scanned concurrently inside
/// one thread (libxml2's hash is single-threaded too).
pub struct xmlHashTable {
    /// Keyed on `(key1, key2, key3)` triples — libxml2's hash takes
    /// up to three string keys.  Empty `String` means "no key" at
    /// that slot.  Value is a caller-owned pointer; we don't free.
    map: RefCell<HashMap<(String, String, String), *mut c_void>>,
}

/// `xmlHashScanner` callback signature: `void (*)(void *data, void *user, const xmlChar *name)`.
pub type xmlHashScanner = unsafe extern "C" fn(
    data: *mut c_void,
    user: *mut c_void,
    name: *const c_char,
);

/// `xmlHashCreate(size)` — allocate a hash table.  `size` is a hint;
/// our HashMap grows as needed.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashCreate(_size: c_int) -> *mut xmlHashTable {
    Box::into_raw(Box::new(xmlHashTable {
        map: RefCell::new(HashMap::new()),
    }))
}

/// libxml2 `xmlHashCopy(table, copier)` — shallow-clone a hash
/// table.  `copier` is invoked once per entry to transform each
/// value pointer (e.g. cloning the pointee, ref-counting); the
/// returned pointers populate the new table.
///
/// Returns a fresh table or NULL on NULL input.  Caller releases via
/// [`xmlHashFree`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashCopy(
    table:  *mut xmlHashTable,
    copier: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
) -> *mut xmlHashTable {
    if table.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts table came from xmlHashCreate.
    let t = unsafe { &*table };
    let mut copy: HashMap<(String, String, String), *mut c_void> = HashMap::new();
    for ((k1, k2, k3), v) in t.map.borrow().iter() {
        let new_val = match copier {
            Some(f) => {
                let cname = match std::ffi::CString::new(k1.as_str()) {
                    Ok(c)  => c,
                    Err(_) => continue,
                };
                // SAFETY: caller-supplied copier; we trust it.
                unsafe { f(*v, cname.as_ptr()) }
            }
            None => *v,
        };
        copy.insert((k1.clone(), k2.clone(), k3.clone()), new_val);
    }
    Box::into_raw(Box::new(xmlHashTable { map: RefCell::new(copy) }))
}

/// `xmlHashCreateDict(size, dict)` — variant that shares strings via
/// a dict.  We don't intern (see [`crate::dict`]); behaves like
/// `xmlHashCreate`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashCreateDict(
    size: c_int,
    _dict: *mut c_void,
) -> *mut xmlHashTable {
    unsafe { xmlHashCreate(size) }
}

/// `xmlHashFree(table, deallocator)` — release.  `deallocator` is a
/// callback libxml2 invokes for each entry's value; we accept it
/// but pass NULL `user_data` since we don't carry any.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashFree(
    table:        *mut xmlHashTable,
    deallocator:  Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) {
    if table.is_null() { return; }
    // SAFETY: table came from xmlHashCreate.
    let t = unsafe { Box::from_raw(table) };
    if let Some(d) = deallocator {
        for ((k, _, _), v) in t.map.borrow().iter() {
            let key = match std::ffi::CString::new(k.as_str()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // SAFETY: caller-supplied deallocator; we trust it.
            unsafe { d(*v, key.as_ptr()); }
        }
    }
    drop(t);
}

/// `xmlHashLookup(table, name)` — look up by single key.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashLookup(
    table: *mut xmlHashTable,
    name:  *const c_char,
) -> *mut c_void {
    if table.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    let key = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return ptr::null_mut(),
    };
    let t = unsafe { &*table };
    t.map.borrow()
        .get(&(key, String::new(), String::new()))
        .copied()
        .unwrap_or(ptr::null_mut())
}

/// `xmlHashAddEntry(table, name, value)` — insert if absent.  Returns
/// 0 on insert, -1 on key already present.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashAddEntry(
    table: *mut xmlHashTable,
    name:  *const c_char,
    value: *mut c_void,
) -> c_int {
    if table.is_null() || name.is_null() {
        return -1;
    }
    let key = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    let k = (key, String::new(), String::new());
    if map.contains_key(&k) { -1 }
    else { map.insert(k, value); 0 }
}

/// `xmlHashUpdateEntry(table, name, value, deallocator)` — insert or
/// replace; calls `deallocator(old_value, name)` if replacing.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashUpdateEntry(
    table: *mut xmlHashTable,
    name:  *const c_char,
    value: *mut c_void,
    deallocator: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) -> c_int {
    if table.is_null() || name.is_null() {
        return -1;
    }
    let key = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    let k = (key.clone(), String::new(), String::new());
    if let Some(old) = map.insert(k, value) {
        if let Some(d) = deallocator {
            let cs = std::ffi::CString::new(key).unwrap_or_default();
            unsafe { d(old, cs.as_ptr()); }
        }
    }
    0
}

/// `xmlHashRemoveEntry(table, name, deallocator)` — remove.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashRemoveEntry(
    table: *mut xmlHashTable,
    name:  *const c_char,
    deallocator: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) -> c_int {
    if table.is_null() || name.is_null() {
        return -1;
    }
    let key = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    let k = (key.clone(), String::new(), String::new());
    match map.remove(&k) {
        Some(v) => {
            if let Some(d) = deallocator {
                let cs = std::ffi::CString::new(key).unwrap_or_default();
                unsafe { d(v, cs.as_ptr()); }
            }
            0
        }
        None => -1,
    }
}

/// `xmlHashScan(table, scanner, user_data)` — invoke `scanner` for
/// every entry.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashScan(
    table:   *mut xmlHashTable,
    scanner: Option<xmlHashScanner>,
    data:    *mut c_void,
) {
    if table.is_null() { return; }
    let Some(s) = scanner else { return };
    let t = unsafe { &*table };
    // Collect first (the callback may mutate the table).
    let entries: Vec<(String, *mut c_void)> = t.map.borrow().iter()
        .map(|((k, _, _), v)| (k.clone(), *v))
        .collect();
    for (key, value) in entries {
        let cs = match std::ffi::CString::new(key) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // SAFETY: caller-supplied callback.
        unsafe { s(value, data, cs.as_ptr()); }
    }
}

/// `xmlHashSize(table)` — number of entries.  -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashSize(table: *mut xmlHashTable) -> c_int {
    if table.is_null() { return -1; }
    let t = unsafe { &*table };
    t.map.borrow().len() as c_int
}

// ── multi-key variants ────────────────────────────────────────────────────
//
// libxml2's hash supports keying on 1, 2, or 3 strings.  Used by
// libxslt (template tables keyed on `(local-name, namespace,
// mode-name)`), DTD validation (element types keyed on
// `(name, namespace)`), and XPath function tables.

/// Build a (key1, key2, key3) tuple from C strings.  Empty slots are
/// stored as empty `String`s — matching the 1-key path above which
/// uses `String::new()` for the unused slots.
fn build_key(
    n1: *const c_char,
    n2: *const c_char,
    n3: *const c_char,
) -> Option<(String, String, String)> {
    let pull = |p: *const c_char| -> Option<String> {
        if p.is_null() {
            return Some(String::new());
        }
        unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_string)
    };
    Some((pull(n1)?, pull(n2)?, pull(n3)?))
}

/// `xmlHashLookup2(table, name, name2)` — look up by 2-key.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashLookup2(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
) -> *mut c_void {
    if table.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    let Some(k) = build_key(name, name2, ptr::null()) else { return ptr::null_mut() };
    let t = unsafe { &*table };
    t.map.borrow().get(&k).copied().unwrap_or(ptr::null_mut())
}

/// `xmlHashLookup3(table, name, name2, name3)` — look up by 3-key.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashLookup3(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    name3: *const c_char,
) -> *mut c_void {
    if table.is_null() || name.is_null() {
        return ptr::null_mut();
    }
    let Some(k) = build_key(name, name2, name3) else { return ptr::null_mut() };
    let t = unsafe { &*table };
    t.map.borrow().get(&k).copied().unwrap_or(ptr::null_mut())
}

/// `xmlHashAddEntry2(table, name, name2, value)` — insert if absent.
/// Returns 0 on insert, -1 on key already present.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashAddEntry2(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    value: *mut c_void,
) -> c_int {
    if table.is_null() || name.is_null() { return -1; }
    let Some(k) = build_key(name, name2, ptr::null()) else { return -1 };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    if map.contains_key(&k) { -1 }
    else { map.insert(k, value); 0 }
}

/// `xmlHashAddEntry3(table, name, name2, name3, value)` — insert if absent.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashAddEntry3(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    name3: *const c_char,
    value: *mut c_void,
) -> c_int {
    if table.is_null() || name.is_null() { return -1; }
    let Some(k) = build_key(name, name2, name3) else { return -1 };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    if map.contains_key(&k) { -1 }
    else { map.insert(k, value); 0 }
}

/// `xmlHashUpdateEntry2(table, name, name2, value, deallocator)` —
/// insert or replace; calls `deallocator(old, name)` on replace.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashUpdateEntry2(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    value: *mut c_void,
    deallocator: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) -> c_int {
    if table.is_null() || name.is_null() { return -1; }
    let Some(k) = build_key(name, name2, ptr::null()) else { return -1 };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    if let Some(old) = map.insert(k.clone(), value) {
        if let Some(d) = deallocator {
            let cs = std::ffi::CString::new(k.0).unwrap_or_default();
            unsafe { d(old, cs.as_ptr()); }
        }
    }
    0
}

/// `xmlHashUpdateEntry3(table, name, name2, name3, value, deallocator)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashUpdateEntry3(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    name3: *const c_char,
    value: *mut c_void,
    deallocator: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) -> c_int {
    if table.is_null() || name.is_null() { return -1; }
    let Some(k) = build_key(name, name2, name3) else { return -1 };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    if let Some(old) = map.insert(k.clone(), value) {
        if let Some(d) = deallocator {
            let cs = std::ffi::CString::new(k.0).unwrap_or_default();
            unsafe { d(old, cs.as_ptr()); }
        }
    }
    0
}

/// `xmlHashRemoveEntry2(table, name, name2, deallocator)` — remove a
/// 2-key entry; calls `deallocator(value, name)` after removal.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashRemoveEntry2(
    table: *mut xmlHashTable,
    name:  *const c_char,
    name2: *const c_char,
    deallocator: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
) -> c_int {
    if table.is_null() || name.is_null() { return -1; }
    let Some(k) = build_key(name, name2, ptr::null()) else { return -1 };
    let t = unsafe { &*table };
    let mut map = t.map.borrow_mut();
    match map.remove(&k) {
        Some(v) => {
            if let Some(d) = deallocator {
                let cs = std::ffi::CString::new(k.0).unwrap_or_default();
                unsafe { d(v, cs.as_ptr()); }
            }
            0
        }
        None => -1,
    }
}

/// `xmlHashScanFull(table, scanner, user_data)` — like `xmlHashScan`
/// but the scanner callback receives all three keys.  Signature:
/// `void (*)(void *data, void *user, const xmlChar *name, const
/// xmlChar *name2, const xmlChar *name3)`.
pub type xmlHashScannerFull = unsafe extern "C" fn(
    data:  *mut c_void,
    user:  *mut c_void,
    name:  *const c_char,
    name2: *const c_char,
    name3: *const c_char,
);

#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHashScanFull(
    table:   *mut xmlHashTable,
    scanner: Option<xmlHashScannerFull>,
    data:    *mut c_void,
) {
    if table.is_null() { return; }
    let Some(s) = scanner else { return };
    // SAFETY: caller asserts `table` is a live xmlHashTable handle.
    let t: &xmlHashTable = unsafe { &*table };
    // Snapshot the entries (callback may mutate the table during scan).
    let entries: Vec<((String, String, String), *mut c_void)> = t.map.borrow().iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    for ((n1, n2, n3), value) in entries {
        let cs1 = std::ffi::CString::new(n1).unwrap_or_default();
        let cs2 = std::ffi::CString::new(n2).unwrap_or_default();
        let cs3 = std::ffi::CString::new(n3).unwrap_or_default();
        // Empty keys → NULL pointer, matching libxml2 conventions
        // (callers test `name2 == NULL` to distinguish 1-key entries).
        let p2 = if cs2.as_bytes().is_empty() { ptr::null() } else { cs2.as_ptr() };
        let p3 = if cs3.as_bytes().is_empty() { ptr::null() } else { cs3.as_ptr() };
        // SAFETY: `s` is a caller-supplied function pointer we accept
        // on trust (xmlHashScannerFull is `extern "C" fn`).  All
        // string args have valid lifetimes for the duration of the
        // call: cs1/cs2/cs3 outlive the call body.
        unsafe { s(value, data, cs1.as_ptr(), p2, p3); }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn add_lookup_size_remove() {
        let h = unsafe { xmlHashCreate(8) };
        assert!(!h.is_null());
        assert_eq!(unsafe { xmlHashSize(h) }, 0);

        let key = CString::new("alpha").unwrap();
        let val = 0x1234usize as *mut c_void;
        assert_eq!(unsafe { xmlHashAddEntry(h, key.as_ptr(), val) }, 0);
        assert_eq!(unsafe { xmlHashSize(h) }, 1);
        assert_eq!(unsafe { xmlHashLookup(h, key.as_ptr()) } as usize, 0x1234);

        // Duplicate add → -1.
        assert_eq!(unsafe { xmlHashAddEntry(h, key.as_ptr(), val) }, -1);

        // Update replaces.
        let v2 = 0x5678usize as *mut c_void;
        assert_eq!(unsafe { xmlHashUpdateEntry(h, key.as_ptr(), v2, None) }, 0);
        assert_eq!(unsafe { xmlHashLookup(h, key.as_ptr()) } as usize, 0x5678);

        // Remove.
        assert_eq!(unsafe { xmlHashRemoveEntry(h, key.as_ptr(), None) }, 0);
        assert_eq!(unsafe { xmlHashSize(h) }, 0);
        assert!(unsafe { xmlHashLookup(h, key.as_ptr()) }.is_null());

        unsafe { xmlHashFree(h, None); }
    }

    static SCAN_COUNT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    unsafe extern "C" fn scan_counter(_data: *mut c_void, _user: *mut c_void, _name: *const c_char) {
        SCAN_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn scan_iterates() {
        let h = unsafe { xmlHashCreate(8) };
        for k in ["a", "b", "c"] {
            let cs = CString::new(k).unwrap();
            unsafe { xmlHashAddEntry(h, cs.as_ptr(), 0x1 as *mut _); }
        }
        SCAN_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
        unsafe { xmlHashScan(h, Some(scan_counter), ptr::null_mut()); }
        assert_eq!(SCAN_COUNT.load(std::sync::atomic::Ordering::SeqCst), 3);
        unsafe { xmlHashFree(h, None); }
    }

    // ── multi-key tests ──────────────────────────────────────────────────

    #[test]
    fn two_key_lookup_and_add() {
        let h = unsafe { xmlHashCreate(8) };
        let n  = CString::new("local").unwrap();
        let n2 = CString::new("ns://example").unwrap();

        // Insert under (local, ns) — value pointer is just a tagged usize.
        let val = 0xCAFEusize as *mut std::os::raw::c_void;
        assert_eq!(unsafe { xmlHashAddEntry2(h, n.as_ptr(), n2.as_ptr(), val) }, 0);

        // Lookup via 2-key.
        assert_eq!(unsafe { xmlHashLookup2(h, n.as_ptr(), n2.as_ptr()) }, val);
        // Lookup via 1-key (different storage slot) must miss.
        assert!(unsafe { xmlHashLookup(h, n.as_ptr()) }.is_null());

        // Adding the same 2-key again returns -1 (already present).
        assert_eq!(unsafe { xmlHashAddEntry2(h, n.as_ptr(), n2.as_ptr(), val) }, -1);

        unsafe { xmlHashFree(h, None); }
    }

    #[test]
    fn three_key_round_trip() {
        let h = unsafe { xmlHashCreate(8) };
        let a = CString::new("a").unwrap();
        let b = CString::new("b").unwrap();
        let c = CString::new("c").unwrap();
        let val = 0xBEEFusize as *mut std::os::raw::c_void;

        assert_eq!(unsafe { xmlHashAddEntry3(h, a.as_ptr(), b.as_ptr(), c.as_ptr(), val) }, 0);
        assert_eq!(unsafe { xmlHashLookup3(h, a.as_ptr(), b.as_ptr(), c.as_ptr()) }, val);
        // Wrong third key → miss.
        let bad = CString::new("x").unwrap();
        assert!(unsafe { xmlHashLookup3(h, a.as_ptr(), b.as_ptr(), bad.as_ptr()) }.is_null());

        unsafe { xmlHashFree(h, None); }
    }

    #[test]
    fn update_entry2_replaces_and_deallocates() {
        let h = unsafe { xmlHashCreate(8) };
        let n  = CString::new("k").unwrap();
        let n2 = CString::new("k2").unwrap();
        let v1 = 1usize as *mut std::os::raw::c_void;
        let v2 = 2usize as *mut std::os::raw::c_void;

        // Static recorder for the deallocator.
        static SEEN_OLD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        extern "C" fn dealloc(old: *mut std::os::raw::c_void, _name: *const c_char) {
            SEEN_OLD.store(old as usize, std::sync::atomic::Ordering::SeqCst);
        }

        // First update: insert v1, no dealloc call.
        unsafe { xmlHashUpdateEntry2(h, n.as_ptr(), n2.as_ptr(), v1, Some(dealloc)); }
        assert_eq!(SEEN_OLD.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Second update: replaces v1 with v2, dealloc fires on v1.
        unsafe { xmlHashUpdateEntry2(h, n.as_ptr(), n2.as_ptr(), v2, Some(dealloc)); }
        assert_eq!(SEEN_OLD.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Final value is v2.
        assert_eq!(unsafe { xmlHashLookup2(h, n.as_ptr(), n2.as_ptr()) }, v2);

        unsafe { xmlHashFree(h, None); }
    }

    #[test]
    fn remove_entry2_returns_minus_one_on_miss() {
        let h = unsafe { xmlHashCreate(8) };
        let n  = CString::new("x").unwrap();
        let n2 = CString::new("y").unwrap();
        assert_eq!(unsafe { xmlHashRemoveEntry2(h, n.as_ptr(), n2.as_ptr(), None) }, -1);
        unsafe { xmlHashFree(h, None); }
    }

    #[test]
    fn scan_full_invokes_callback_with_all_keys() {
        let h = unsafe { xmlHashCreate(8) };
        let a = CString::new("a").unwrap();
        let b = CString::new("b").unwrap();
        unsafe { xmlHashAddEntry2(h, a.as_ptr(), b.as_ptr(), 0x42 as *mut _); }

        static N2_SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        extern "C" fn record(
            _v: *mut std::os::raw::c_void,
            _u: *mut std::os::raw::c_void,
            _n1: *const c_char,
            n2: *const c_char,
            _n3: *const c_char,
        ) {
            // Verify n2 came through non-NULL (entry had a 2-key).
            if !n2.is_null() {
                let s = unsafe { CStr::from_ptr(n2) }.to_str().unwrap();
                N2_SEEN.store(s.len(), std::sync::atomic::Ordering::SeqCst);
            }
        }

        unsafe { xmlHashScanFull(h, Some(record), ptr::null_mut()); }
        assert_eq!(N2_SEEN.load(std::sync::atomic::Ordering::SeqCst), 1, "n2='b' has length 1");

        unsafe { xmlHashFree(h, None); }
    }
}
