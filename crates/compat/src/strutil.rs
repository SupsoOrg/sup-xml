//! libxml2 byte-string utilities.
//!
//! libxml2's `xmlChar` is `unsigned char`; all "strings" are
//! NUL-terminated UTF-8 byte sequences.  These seven functions wrap
//! standard operations (length, compare, find, duplicate) at the C
//! ABI level so consumer code that calls `xmlStrlen(...)`,
//! `xmlStrcmp(...)`, etc. works against our shim.
//!
//! All seven are NULL-safe in the libxml2 sense:
//!
//! - `xmlStrlen(NULL) == 0`
//! - `xmlStrcmp` / `xmlStrncmp` / `xmlStrcasecmp`: NULL == NULL → 0;
//!   NULL otherwise sorts less than non-NULL.
//! - `xmlStrchr(NULL, _)` / `xmlStrstr(NULL, _)` / `xmlStrstr(_, NULL)`:
//!   return NULL.
//! - `xmlStrdup(NULL)` returns NULL.
//!
//! `xmlStrdup` uses [`crate::alloc::alloc_registered_cstring`] so the
//! pointer it returns is recognized by [`crate::parse::xml_free_impl`].

use std::cmp::Ordering;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::alloc::alloc_registered_cstring;

/// `xmlStrlen` — length of a NUL-terminated `xmlChar*` (excluding NUL).
/// Returns 0 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrlen(s: *const c_char) -> c_int {
    if s.is_null() {
        return 0;
    }
    // SAFETY: caller asserts NUL-terminated readable string.
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    bytes.len().try_into().unwrap_or(c_int::MAX)
}

/// `xmlStrcmp` — byte-wise compare two NUL-terminated `xmlChar*`s.
/// Returns negative / 0 / positive like `strcmp`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrcmp(a: *const c_char, b: *const c_char) -> c_int {
    match (a.is_null(), b.is_null()) {
        (true,  true)  => 0,
        (true,  false) => -1,
        (false, true)  => 1,
        (false, false) => {
            // SAFETY: callers asserted both are NUL-terminated.
            let aa = unsafe { CStr::from_ptr(a) }.to_bytes();
            let bb = unsafe { CStr::from_ptr(b) }.to_bytes();
            ord_to_c(aa.cmp(bb))
        }
    }
}

/// `xmlStrncmp` — compare up to `n` bytes, NUL-stopping in either.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrncmp(
    a: *const c_char,
    b: *const c_char,
    n: c_int,
) -> c_int {
    let n = if n < 0 { 0 } else { n as usize };
    if n == 0 {
        return 0;
    }
    match (a.is_null(), b.is_null()) {
        (true,  true)  => 0,
        (true,  false) => -1,
        (false, true)  => 1,
        (false, false) => {
            // SAFETY: callers assert NUL-terminated; we cap at NUL.
            let aa = unsafe { CStr::from_ptr(a) }.to_bytes();
            let bb = unsafe { CStr::from_ptr(b) }.to_bytes();
            let lim_a = aa.len().min(n);
            let lim_b = bb.len().min(n);
            ord_to_c(aa[..lim_a].cmp(&bb[..lim_b]))
        }
    }
}

/// `xmlStrcasecmp` — ASCII case-insensitive compare.  libxml2 doesn't
/// do Unicode case-folding here either.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrcasecmp(a: *const c_char, b: *const c_char) -> c_int {
    match (a.is_null(), b.is_null()) {
        (true,  true)  => 0,
        (true,  false) => -1,
        (false, true)  => 1,
        (false, false) => {
            // SAFETY: callers assert NUL-terminated.
            let aa = unsafe { CStr::from_ptr(a) }.to_bytes();
            let bb = unsafe { CStr::from_ptr(b) }.to_bytes();
            // Compare lowercase-folded.
            for (&x, &y) in aa.iter().zip(bb.iter()) {
                let xl = x.to_ascii_lowercase();
                let yl = y.to_ascii_lowercase();
                match xl.cmp(&yl) {
                    Ordering::Equal => continue,
                    o => return ord_to_c(o),
                }
            }
            ord_to_c(aa.len().cmp(&bb.len()))
        }
    }
}

/// `xmlStrchr` — first occurrence of byte `c` in `s`, or NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrchr(s: *const c_char, c: c_int) -> *const c_char {
    if s.is_null() {
        return ptr::null();
    }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    let target = c as u8;
    match bytes.iter().position(|&b| b == target) {
        Some(i) => unsafe { s.add(i) },
        None    => ptr::null(),
    }
}

/// `xmlStrstr` — first occurrence of `needle` substring in `haystack`,
/// or NULL.  Empty needle returns `haystack` (matches libxml2 /
/// glibc convention).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrstr(
    haystack: *const c_char,
    needle:   *const c_char,
) -> *const c_char {
    if haystack.is_null() || needle.is_null() {
        return ptr::null();
    }
    // SAFETY: callers assert NUL-terminated.
    let h = unsafe { CStr::from_ptr(haystack) }.to_bytes();
    let n = unsafe { CStr::from_ptr(needle) }.to_bytes();
    if n.is_empty() {
        return haystack;
    }
    // O(h.len() * n.len()) naive — same complexity class as libxml2's
    // implementation; substrings here are short in practice.
    h.windows(n.len())
        .position(|w| w == n)
        .map(|i| unsafe { haystack.add(i) })
        .unwrap_or(ptr::null())
}

/// `xmlStrdup` — return a fresh heap-allocated copy of `s` (caller
/// releases via `xmlFree`).  Returns NULL on NULL input or alloc
/// failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrdup(s: *const c_char) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    alloc_registered_cstring(bytes)
}

/// `xmlMemStrdup(s)` — duplicate a NUL-terminated string through the
/// registered allocator.  Same semantics as [`xmlStrdup`]; libxml2
/// keeps the two symbols separate because `xmlMemStrdup` is rooted
/// in the `xmlMemory` layer (used by callers that hold a raw
/// `xmlMallocFunc`/`xmlStrdup` pointer pair) — both paths land on
/// the same allocator in our build.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMemStrdup(s: *const c_char) -> *mut c_char {
    unsafe { xmlStrdup(s) }
}

/// `xmlCharStrndup(s, n)` — like [`xmlStrdup`] but copies at most `n`
/// bytes, stopping early on an embedded NUL.  Returns NULL on NULL
/// input or negative `n`.  The result is NUL-terminated.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharStrndup(s: *const c_char, n: c_int) -> *mut c_char {
    if s.is_null() || n < 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `s` points to at least `n` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(s as *const u8, n as usize) };
    // libxml2 stops at the first NUL within the first n bytes.
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    alloc_registered_cstring(&bytes[..len])
}

/// `xmlCharStrdup(s)` — duplicate a `const char*` as an `xmlChar*`.
/// Same shape as [`xmlStrdup`] but takes the libxml2 "C string" type.
/// Returns NULL on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCharStrdup(s: *const c_char) -> *mut c_char {
    unsafe { xmlStrdup(s) }
}

/// `xmlBuildQName(local, prefix, memory, len)` — build a QName from
/// `prefix:local`.  If `memory` is non-NULL and `len` is large enough
/// to hold `prefix:local\0`, the result is written there; otherwise a
/// fresh heap allocation is returned.
///
/// Returns NULL on NULL local or alloc failure.  `prefix` may be NULL
/// (then the result is just `local`, possibly returning `local` itself).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBuildQName(
    local:  *const c_char,
    prefix: *const c_char,
    memory: *mut c_char,
    len:    c_int,
) -> *mut c_char {
    if local.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts NUL-terminated.
    let local_bytes = unsafe { CStr::from_ptr(local) }.to_bytes();
    if prefix.is_null() {
        // libxml2 returns the original `local` pointer when no prefix
        // is supplied (caller knows not to free it via xmlFree).
        return local as *mut c_char;
    }
    let prefix_bytes = unsafe { CStr::from_ptr(prefix) }.to_bytes();
    let total = prefix_bytes.len() + 1 + local_bytes.len();
    let needed = total + 1; // NUL terminator
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(prefix_bytes);
    buf.push(b':');
    buf.extend_from_slice(local_bytes);
    if !memory.is_null() && (len as usize) >= needed {
        // Write in-place into caller's buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.as_ptr(), memory as *mut u8, total);
            *memory.add(total) = 0;
        }
        return memory;
    }
    alloc_registered_cstring(&buf)
}

/// `xmlSplitQName3(name, len)` — split `prefix:local` and return the
/// local part.  `len` is an out-param: on entry/exit it receives the
/// length of the prefix.  Returns NULL if the name has no prefix.
///
/// The returned pointer is interior to the input string; the caller
/// must NOT free it.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSplitQName3(
    name: *const c_char,
    len:  *mut c_int,
) -> *const c_char {
    if name.is_null() { return ptr::null(); }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let colon = match bytes.iter().position(|&b| b == b':') {
        Some(i) => i,
        None => return ptr::null(),
    };
    if !len.is_null() {
        unsafe { *len = colon as c_int; }
    }
    // SAFETY: `name + colon + 1` is in-bounds since we found `:` at `colon`.
    unsafe { name.add(colon + 1) }
}

/// `xmlGetUTF8Char(utf, len)` — decode one UTF-8 codepoint at `utf`,
/// updating `*len` to the byte count consumed.  Returns the Unicode
/// scalar value (≥0) on success, -1 on malformed input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetUTF8Char(
    utf: *const c_char,
    len: *mut c_int,
) -> c_int {
    if utf.is_null() || len.is_null() { return -1; }
    let max = unsafe { *len };
    if max <= 0 { return -1; }
    // SAFETY: caller asserts `utf` is readable for `max` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(utf as *const u8, max as usize) };
    let mut iter = std::str::from_utf8(bytes).ok().map(|s| s.chars()).into_iter().flatten();
    let Some(c) = iter.next() else { return -1; };
    let consumed = c.len_utf8() as c_int;
    unsafe { *len = consumed; }
    c as u32 as c_int
}

/// `xmlUTF8Strsub(utf, start, len)` — fresh heap copy of the
/// codepoint substring `utf[start..start+len]`.  Returns NULL on
/// NULL input, out-of-range, or alloc failure.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strsub(
    utf:   *const c_char,
    start: c_int,
    len:   c_int,
) -> *mut c_char {
    if utf.is_null() || start < 0 || len < 0 { return ptr::null_mut(); }
    // SAFETY: caller asserts NUL-terminated.
    let s = match unsafe { CStr::from_ptr(utf) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let sub: String = s.chars()
        .skip(start as usize)
        .take(len as usize)
        .collect();
    alloc_registered_cstring(sub.as_bytes())
}

#[inline]
fn ord_to_c(o: Ordering) -> c_int {
    match o {
        Ordering::Less    => -1,
        Ordering::Equal   =>  0,
        Ordering::Greater =>  1,
    }
}

// ── additional libxml2 string helpers ──────────────────────────────────────

/// `xmlStrEqual` — byte-wise equality of two `xmlChar*`s, NULL-safe.
/// Returns 1 if both NULL or contents identical; 0 otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrEqual(a: *const c_char, b: *const c_char) -> c_int {
    if unsafe { xmlStrcmp(a, b) } == 0 { 1 } else { 0 }
}

/// `xmlStrncasecmp(a, b, n)` — case-insensitive (ASCII) compare of up
/// to `n` bytes.  NUL-stops in either operand.  NULL-safe like
/// `xmlStrcmp`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrncasecmp(
    a: *const c_char,
    b: *const c_char,
    n: c_int,
) -> c_int {
    if n <= 0 {
        return 0;
    }
    match (a.is_null(), b.is_null()) {
        (true,  true)  => 0,
        (true,  false) => -1,
        (false, true)  => 1,
        (false, false) => {
            let aa = unsafe { CStr::from_ptr(a) }.to_bytes();
            let bb = unsafe { CStr::from_ptr(b) }.to_bytes();
            let lim = (n as usize).min(aa.len()).min(bb.len());
            for i in 0..lim {
                let ca = aa[i].to_ascii_lowercase();
                let cb = bb[i].to_ascii_lowercase();
                match ca.cmp(&cb) {
                    Ordering::Equal => {}
                    other => return ord_to_c(other),
                }
            }
            // Equal up to lim — shorter (with NUL inside the window)
            // sorts less.  Stop iff we hit n bytes or both terminated.
            if lim < n as usize {
                ord_to_c(aa.len().cmp(&bb.len()))
            } else {
                0
            }
        }
    }
}

/// `xmlStrndup(s, len)` — heap-allocate a copy of exactly `len`
/// bytes of `s`, NUL-terminated.  Returns NULL on NULL or len < 0.
///
/// Binary-safe: does NOT stop at embedded NUL bytes (libxml2 uses a
/// raw `memcpy`).  Required for UTF-16 buffers where ASCII chars
/// emit `<byte>\x00` pairs — truncating would hand back a 4-byte
/// allocation with a 100-byte length claim, and the caller would
/// read uninitialised memory.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrndup(s: *const c_char, len: c_int) -> *mut c_char {
    if s.is_null() || len < 0 {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `s` is readable for at least `len` bytes.
    let slice = unsafe { std::slice::from_raw_parts(s as *const u8, len as usize) };
    crate::alloc::alloc_registered_buffer(slice)
}

/// `xmlStrcat(cur, add)` — append a NUL-terminated string to a
/// previously heap-allocated `xmlChar*`.  libxml2's contract: `cur`
/// is `xmlFree`-ed; the return value is a fresh allocation.  If
/// `add` is NULL the returned pointer equals `cur` (the caller still
/// owns the original allocation).  If `cur` is NULL, behaves like
/// `xmlStrdup(add)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrcat(
    cur: *mut c_char,
    add: *const c_char,
) -> *mut c_char {
    if add.is_null() {
        return cur;
    }
    if cur.is_null() {
        return unsafe { xmlStrdup(add) };
    }
    let cur_bytes = unsafe { CStr::from_ptr(cur) }.to_bytes();
    let add_bytes = unsafe { CStr::from_ptr(add) }.to_bytes();
    let mut combined = Vec::with_capacity(cur_bytes.len() + add_bytes.len());
    combined.extend_from_slice(cur_bytes);
    combined.extend_from_slice(add_bytes);
    let p = alloc_registered_cstring(&combined);
    // Release the old buffer if it was one of ours; otherwise it's
    // caller-owned and we leave it alone (libxml2 does the same).
    unsafe { crate::parse::xml_free_impl(cur as *mut std::os::raw::c_void); }
    p
}

/// `xmlStrncat(cur, add, len)` — `xmlStrcat` capped at `len` bytes of
/// `add`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrncat(
    cur: *mut c_char,
    add: *const c_char,
    len: c_int,
) -> *mut c_char {
    if add.is_null() || len <= 0 {
        return cur;
    }
    let add_slice = unsafe { std::slice::from_raw_parts(add as *const u8, len as usize) };
    let add_cut = add_slice.iter().position(|&b| b == 0).unwrap_or(add_slice.len());
    if cur.is_null() {
        return alloc_registered_cstring(&add_slice[..add_cut]);
    }
    let cur_bytes = unsafe { CStr::from_ptr(cur) }.to_bytes();
    let mut combined = Vec::with_capacity(cur_bytes.len() + add_cut);
    combined.extend_from_slice(cur_bytes);
    combined.extend_from_slice(&add_slice[..add_cut]);
    let p = alloc_registered_cstring(&combined);
    unsafe { crate::parse::xml_free_impl(cur as *mut std::os::raw::c_void); }
    p
}

/// `xmlStrsub(s, start, len)` — heap-allocate a copy of the substring
/// `s[start..start+len]`.  `start` and `len` are *character* counts
/// in libxml2 but for ASCII-and-byte-counted `xmlChar*` semantics we
/// treat them as byte counts (matches libxml2's behaviour on UTF-8
/// where it counts bytes, not codepoints — see the `xmlUTF8Strsub`
/// twin for the codepoint-counted variant).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlStrsub(
    s: *const c_char,
    start: c_int,
    len: c_int,
) -> *mut c_char {
    if s.is_null() || start < 0 || len < 0 {
        return ptr::null_mut();
    }
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    let start = start as usize;
    let len = len as usize;
    if start > bytes.len() {
        return ptr::null_mut();
    }
    let end = (start + len).min(bytes.len());
    alloc_registered_cstring(&bytes[start..end])
}

// ── UTF-8 codepoint-aware string ops ──────────────────────────────────────
//
// libxml2's `xmlUTF8*` family counts Unicode codepoints, not bytes —
// the corresponding `xmlStr*` family counts bytes.  Both operate on
// UTF-8-encoded `xmlChar*`s.

/// `xmlUTF8Strlen(utf)` — number of Unicode codepoints in `utf`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strlen(utf: *const c_char) -> c_int {
    if utf.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(utf) }.to_bytes();
    match std::str::from_utf8(bytes) {
        Ok(s)  => s.chars().count().try_into().unwrap_or(c_int::MAX),
        Err(_) => -1,
    }
}

/// `xmlUTF8Strsize(utf, n)` — byte length of the first `n` Unicode
/// codepoints in `utf`.  Returns 0 on NULL or invalid UTF-8.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strsize(utf: *const c_char, n: c_int) -> c_int {
    if utf.is_null() || n < 0 {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(utf) }.to_bytes();
    let s = match std::str::from_utf8(bytes) { Ok(s) => s, Err(_) => return 0 };
    s.char_indices()
        .nth(n as usize)
        .map(|(idx, _)| idx as c_int)
        .unwrap_or(bytes.len() as c_int)
}

/// `xmlUTF8Strpos(utf, pos)` — pointer to the `pos`-th codepoint of
/// `utf`, or NULL on out-of-range / invalid UTF-8.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strpos(
    utf: *const c_char,
    pos: c_int,
) -> *const c_char {
    if utf.is_null() || pos < 0 {
        return ptr::null();
    }
    let bytes = unsafe { CStr::from_ptr(utf) }.to_bytes();
    let s = match std::str::from_utf8(bytes) { Ok(s) => s, Err(_) => return ptr::null() };
    match s.char_indices().nth(pos as usize) {
        Some((idx, _)) => unsafe { utf.add(idx) },
        None           => ptr::null(),
    }
}

/// `xmlUTF8Strloc(utf, utfchar)` — codepoint index of the first
/// occurrence of `utfchar` (a NUL-terminated UTF-8 sequence) in
/// `utf`.  Returns -1 if not found, -2 on invalid UTF-8.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strloc(
    utf: *const c_char,
    utfchar: *const c_char,
) -> c_int {
    if utf.is_null() || utfchar.is_null() {
        return -1;
    }
    let hay = match std::str::from_utf8(unsafe { CStr::from_ptr(utf) }.to_bytes()) {
        Ok(s) => s, Err(_) => return -2,
    };
    let needle = match std::str::from_utf8(unsafe { CStr::from_ptr(utfchar) }.to_bytes()) {
        Ok(s) => s, Err(_) => return -2,
    };
    match hay.find(needle) {
        Some(byte_idx) => hay[..byte_idx].chars().count().try_into().unwrap_or(c_int::MAX),
        None           => -1,
    }
}

/// `xmlUTF8Charcmp(utf1, utf2)` — codepoint-by-codepoint compare of
/// the first chars of two UTF-8 strings.  Like libxml2's wrapper
/// around `xmlGetUTF8Char` — we just decode the first chars and
/// compare.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Charcmp(
    utf1: *const c_char,
    utf2: *const c_char,
) -> c_int {
    let s1 = if utf1.is_null() { return -1 }
             else { unsafe { CStr::from_ptr(utf1) }.to_bytes() };
    let s2 = if utf2.is_null() { return  1 }
             else { unsafe { CStr::from_ptr(utf2) }.to_bytes() };
    let c1 = match std::str::from_utf8(s1).ok().and_then(|s| s.chars().next()) {
        Some(c) => c, None => return -1,
    };
    let c2 = match std::str::from_utf8(s2).ok().and_then(|s| s.chars().next()) {
        Some(c) => c, None => return 1,
    };
    ord_to_c(c1.cmp(&c2))
}

/// `xmlUTF8Strndup(utf, len)` — duplicate the first `len` Unicode
/// codepoints of `utf` as a fresh heap allocation.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUTF8Strndup(
    utf: *const c_char,
    len: c_int,
) -> *mut c_char {
    if utf.is_null() || len < 0 {
        return ptr::null_mut();
    }
    let bytes = unsafe { CStr::from_ptr(utf) }.to_bytes();
    let s = match std::str::from_utf8(bytes) { Ok(s) => s, Err(_) => return ptr::null_mut() };
    let cut: String = s.chars().take(len as usize).collect();
    alloc_registered_cstring(cut.as_bytes())
}

// ── QName splitter ─────────────────────────────────────────────────────────

/// `xmlSplitQName(ctxt, name, prefix_out)` — the legacy 3-arg
/// splitter that predates `xmlSplitQName2`.  Always returns a fresh
/// allocation of the local part; writes the prefix copy (or NULL) to
/// `*prefix_out`.  The `ctxt` argument exists for libxml2's dict
/// interning and is ignored here.
///
/// Differences from [`xmlSplitQName2`]:
/// - Returns the *whole name* (copied) when there's no colon, instead
///   of NULL.  Callers can always free the returned pointer.
/// - `*prefix_out` is set to NULL on the no-colon path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSplitQName(
    _ctxt:      *mut std::os::raw::c_void,
    name:       *const c_char,
    prefix_out: *mut *mut c_char,
) -> *mut c_char {
    if name.is_null() {
        if !prefix_out.is_null() { unsafe { *prefix_out = ptr::null_mut(); } }
        return ptr::null_mut();
    }
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    match bytes.iter().position(|&b| b == b':') {
        Some(i) if i > 0 && i + 1 < bytes.len() => {
            let pfx   = alloc_registered_cstring(&bytes[..i]);
            let local = alloc_registered_cstring(&bytes[i + 1..]);
            if !prefix_out.is_null() { unsafe { *prefix_out = pfx; } }
            local
        }
        _ => {
            if !prefix_out.is_null() { unsafe { *prefix_out = ptr::null_mut(); } }
            alloc_registered_cstring(bytes)
        }
    }
}

/// `xmlSplitQName2(name, prefix_out)` — if `name` is "prefix:local",
/// allocates a copy of "prefix" and writes the pointer to
/// `*prefix_out`, then returns a fresh allocation of "local".  If
/// `name` has no colon, returns NULL and `*prefix_out` is unchanged.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSplitQName2(
    name: *const c_char,
    prefix_out: *mut *mut c_char,
) -> *mut c_char {
    if name.is_null() || prefix_out.is_null() {
        return ptr::null_mut();
    }
    let bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    let colon = match bytes.iter().position(|&b| b == b':') {
        Some(i) if i > 0 && i + 1 < bytes.len() => i,
        _ => return ptr::null_mut(),
    };
    let pfx = alloc_registered_cstring(&bytes[..colon]);
    let local = alloc_registered_cstring(&bytes[colon + 1..]);
    unsafe { *prefix_out = pfx; }
    local
}

// ── globally-shared text-node-name constants ──────────────────────────────
//
// libxml2 has two pre-allocated NUL-terminated `xmlChar*` constants
// used as the `name` field of text nodes.  Consumer code compares
// `node->name` by *pointer* against these globals to identify text
// kinds, so the symbols must point at stable storage.

// xmlStringText / xmlStringTextNoenc are defined in sup-xml-tree
// (with `#[no_mangle]`) so the tree builder can pin Text-node names
// to them at construction.  Since this crate links tree statically,
// the cdylib exports them once — no duplicate-symbol risk.  See
// `crates/tree/src/dom.rs`.
//
// The unit tests below still reference the symbols by name; pull
// them in via an `extern "C"` declaration.
unsafe extern "C" {
    pub static xmlStringText:      [u8; 5];
    pub static xmlStringTextNoenc: [u8; 10];
}


// ── name validators (XML 1.0 § 2.3) ────────────────────────────────────────

/// `xmlValidateNameValue(value)` — check whether `value` matches the
/// XML 1.0 `Name` production.  Returns 1 on success, 0 on failure or
/// Returns 1 if valid, 0 otherwise.  Matches libxml2's XML 1.0
/// 5th-edition `Name` production (§ 2.3 / Appendix B) — covers
/// ASCII letters/digits/`_-.:` plus the Unicode `NameStartChar`
/// and `NameChar` ranges.  Non-ASCII tag names like `älämänt` (used
/// by lxml's test suite to verify Unicode round-trips) must
/// validate.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateNameValue(value: *const c_char) -> c_int {
    if value.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    let s = match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() => s,
        _ => return 0,
    };
    let mut chars = s.chars();
    let first = match chars.next() { Some(c) => c, None => return 0 };
    // NameStartChar: ASCII letter, '_', ':', or any 5th-edition
    // Unicode NameStartChar above 0x80.
    let start_ok = first.is_ascii_alphabetic()
        || first == '_' || first == ':'
        || (first as u32 >= 0x80
            && sup_xml_core::charsets::is_name_start_char(first));
    if !start_ok {
        return 0;
    }
    for c in chars {
        let ok = c.is_ascii_alphanumeric()
            || c == '_' || c == ':' || c == '-' || c == '.'
            || (c as u32 >= 0x80 && sup_xml_core::charsets::is_name_char_unicode(c));
        if !ok {
            return 0;
        }
    }
    1
}

/// `xmlValidateNCName(value, space)` — like [`xmlValidateNameValue`]
/// but rejects names containing a `':'` (XML Namespaces 1.0 NCName
/// production).  `space` is libxml2's optional leading-whitespace
/// allowance; when non-zero we trim leading XML whitespace before
/// validating.  Returns 0 on success, 1 on failure (libxml2's
/// inverted convention here — it returns 0 on valid).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateNCName(value: *const c_char, space: c_int) -> c_int {
    if value.is_null() {
        return 1;
    }
    // SAFETY: caller asserts NUL-terminated readable.
    let mut bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    if space != 0 {
        while let Some(&b) = bytes.first() {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') { bytes = &bytes[1..]; }
            else { break; }
        }
    }
    let s = match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() => s,
        _ => return 1,
    };
    let mut chars = s.chars();
    let first = match chars.next() { Some(c) => c, None => return 1 };
    // NCName NameStartChar: same as Name's, minus the colon.
    let start_ok = first.is_ascii_alphabetic()
        || first == '_'
        || (first as u32 >= 0x80
            && sup_xml_core::charsets::is_name_start_char(first));
    if !start_ok {
        return 1;
    }
    for c in chars {
        let ok = c.is_ascii_alphanumeric()
            || c == '_' || c == '-' || c == '.'
            || (c as u32 >= 0x80 && sup_xml_core::charsets::is_name_char_unicode(c));
        if !ok {
            return 1;
        }
    }
    0
}

/// `xmlValidateQName(value, space)` — validate an XML Namespaces 1.0
/// QName: either an NCName or `prefix:local` where both prefix and
/// local are NCNames.  Same inverted return as
/// [`xmlValidateNCName`]: 0 on valid, 1 on invalid.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlValidateQName(value: *const c_char, space: c_int) -> c_int {
    if value.is_null() {
        return 1;
    }
    // SAFETY: caller asserts NUL-terminated readable.
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    let bytes = if space != 0 {
        let mut b = bytes;
        while let Some(&c) = b.first() {
            if matches!(c, b' ' | b'\t' | b'\r' | b'\n') { b = &b[1..]; }
            else { break; }
        }
        b
    } else { bytes };
    let s = match std::str::from_utf8(bytes) {
        Ok(s) if !s.is_empty() => s,
        _ => return 1,
    };
    // Split on the first colon.  No colon → must be a valid NCName.
    // Colon present → both halves must be NCNames AND there can be
    // exactly one colon.
    let parts: Vec<&str> = s.splitn(3, ':').collect();
    match parts.len() {
        1 => validate_ncname_str(parts[0]),
        2 => {
            // Trailing colon ("a:") or leading colon (":a") → invalid.
            if parts[0].is_empty() || parts[1].is_empty() { 1 }
            else if validate_ncname_str(parts[0]) != 0 { 1 }
            else { validate_ncname_str(parts[1]) }
        }
        _ => 1, // more than one colon
    }
}

/// Internal helper — the actual NCName logic operating on a `&str`,
/// matching xmlValidateNCName's "0 on valid, 1 on invalid" convention.
fn validate_ncname_str(s: &str) -> c_int {
    if s.is_empty() { return 1; }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    let start_ok = first.is_ascii_alphabetic()
        || first == '_'
        || (first as u32 >= 0x80
            && sup_xml_core::charsets::is_name_start_char(first));
    if !start_ok { return 1; }
    for c in chars {
        let ok = c.is_ascii_alphanumeric()
            || c == '_' || c == '-' || c == '.'
            || (c as u32 >= 0x80 && sup_xml_core::charsets::is_name_char_unicode(c));
        if !ok { return 1; }
    }
    0
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString { CString::new(s).unwrap() }

    #[test]
    fn xml_strlen_basic() {
        let s = cs("hello");
        assert_eq!(unsafe { xmlStrlen(s.as_ptr()) }, 5);
        assert_eq!(unsafe { xmlStrlen(ptr::null()) }, 0);
    }

    #[test]
    fn xml_strcmp_orders() {
        let a = cs("alpha");
        let b = cs("beta");
        let a2 = cs("alpha");
        assert!(unsafe { xmlStrcmp(a.as_ptr(),  a2.as_ptr()) } == 0);
        assert!(unsafe { xmlStrcmp(a.as_ptr(),  b.as_ptr()) }  < 0);
        assert!(unsafe { xmlStrcmp(b.as_ptr(),  a.as_ptr()) }  > 0);
        assert_eq!(unsafe { xmlStrcmp(ptr::null(), ptr::null()) }, 0);
        assert!(unsafe { xmlStrcmp(ptr::null(), a.as_ptr()) } < 0);
        assert!(unsafe { xmlStrcmp(a.as_ptr(), ptr::null()) } > 0);
    }

    #[test]
    fn xml_strncmp_limited() {
        let a = cs("abcdef");
        let b = cs("abcxyz");
        // First 3 bytes match.
        assert_eq!(unsafe { xmlStrncmp(a.as_ptr(), b.as_ptr(), 3) }, 0);
        // 4 bytes: 'd' vs 'x'.
        assert!(unsafe { xmlStrncmp(a.as_ptr(), b.as_ptr(), 4) } < 0);
        // n == 0 → equal regardless.
        assert_eq!(unsafe { xmlStrncmp(a.as_ptr(), b.as_ptr(), 0) }, 0);
    }

    #[test]
    fn xml_strcasecmp_ascii() {
        let a = cs("HELLO");
        let b = cs("hello");
        assert_eq!(unsafe { xmlStrcasecmp(a.as_ptr(), b.as_ptr()) }, 0);
        let c = cs("hellp");
        assert!(unsafe { xmlStrcasecmp(a.as_ptr(), c.as_ptr()) } < 0);
    }

    #[test]
    fn xml_strchr_finds_byte() {
        let s = cs("hello world");
        let p = unsafe { xmlStrchr(s.as_ptr(), b'w' as c_int) };
        assert!(!p.is_null());
        // Pointer should point at 'w' — 6 bytes past start.
        assert_eq!(unsafe { p.offset_from(s.as_ptr()) }, 6);
        // Missing → NULL.
        let p = unsafe { xmlStrchr(s.as_ptr(), b'z' as c_int) };
        assert!(p.is_null());
    }

    #[test]
    fn xml_strstr_finds_substring() {
        let h = cs("the quick brown fox");
        let n = cs("brown");
        let p = unsafe { xmlStrstr(h.as_ptr(), n.as_ptr()) };
        assert!(!p.is_null());
        assert_eq!(unsafe { p.offset_from(h.as_ptr()) }, 10);
        // Empty needle → haystack.
        let empty = cs("");
        let p = unsafe { xmlStrstr(h.as_ptr(), empty.as_ptr()) };
        assert_eq!(p, h.as_ptr());
        // Missing → NULL.
        let miss = cs("zzz");
        let p = unsafe { xmlStrstr(h.as_ptr(), miss.as_ptr()) };
        assert!(p.is_null());
    }

    #[test]
    fn xml_strdup_round_trip() {
        let s = cs("hello");
        let p = unsafe { xmlStrdup(s.as_ptr()) };
        assert!(!p.is_null());
        assert_ne!(p, s.as_ptr() as *mut _); // fresh allocation
        let got = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(got, "hello");
        // Release via xmlFree.
        unsafe {
            crate::parse::xml_free_impl(p as *mut std::os::raw::c_void);
        }
        // NULL → NULL.
        assert!(unsafe { xmlStrdup(ptr::null()) }.is_null());
    }

    #[test]
    fn xml_str_equal_matches_xmlStrcmp() {
        let a = cs("foo");
        let b = cs("foo");
        let c = cs("bar");
        assert_eq!(unsafe { xmlStrEqual(a.as_ptr(), b.as_ptr()) }, 1);
        assert_eq!(unsafe { xmlStrEqual(a.as_ptr(), c.as_ptr()) }, 0);
        assert_eq!(unsafe { xmlStrEqual(ptr::null(), ptr::null()) }, 1);
        assert_eq!(unsafe { xmlStrEqual(a.as_ptr(), ptr::null()) }, 0);
    }

    #[test]
    fn xml_strncasecmp_is_case_insensitive() {
        let a = cs("Hello");
        let b = cs("HELLO");
        let c = cs("world");
        assert_eq!(unsafe { xmlStrncasecmp(a.as_ptr(), b.as_ptr(), 5) }, 0);
        assert!(unsafe { xmlStrncasecmp(a.as_ptr(), c.as_ptr(), 5) } < 0);
        // Prefix match (n stops first)
        assert_eq!(unsafe { xmlStrncasecmp(cs("foo").as_ptr(), cs("foobar").as_ptr(), 3) }, 0);
    }

    #[test]
    fn xml_strndup_copies_exact_len_bytes() {
        let s = cs("hello world");
        let p = unsafe { xmlStrndup(s.as_ptr(), 5) };
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), b"hello");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
        // Full string (exact length) — caller is responsible for not
        // over-reading per libxml2's contract.
        let q = unsafe { xmlStrndup(s.as_ptr(), 11) };
        assert_eq!(unsafe { CStr::from_ptr(q) }.to_bytes(), b"hello world");
        unsafe { crate::parse::xml_free_impl(q as *mut _); }
        assert!(unsafe { xmlStrndup(ptr::null(), 5) }.is_null());
    }

    /// UTF-16 ASCII content has interior NULs (e.g. `<\x00`).  The
    /// xsltSaveResultToString path calls `xmlStrndup(buf, len)` over
    /// such a region; truncating at the first NUL would hand back a
    /// 1-byte allocation that the caller reads as `len` bytes.
    #[test]
    fn xml_strndup_preserves_interior_nuls() {
        let raw: &[u8] = b"\xff\xfe<\x00r\x00>\x00";
        let p = unsafe { xmlStrndup(raw.as_ptr() as *const c_char, raw.len() as c_int) };
        let copy = unsafe { std::slice::from_raw_parts(p as *const u8, raw.len()) };
        assert_eq!(copy, raw);
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
    }

    #[test]
    fn xml_strcat_appends_and_frees_old() {
        let cur = unsafe { xmlStrdup(cs("foo").as_ptr()) };
        let p = unsafe { xmlStrcat(cur, cs("bar").as_ptr()) };
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), b"foobar");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
        // NULL cur + non-NULL add → behaves like xmlStrdup
        let q = unsafe { xmlStrcat(ptr::null_mut(), cs("hi").as_ptr()) };
        assert_eq!(unsafe { CStr::from_ptr(q) }.to_bytes(), b"hi");
        unsafe { crate::parse::xml_free_impl(q as *mut _); }
    }

    #[test]
    fn xml_strncat_caps_at_len() {
        let cur = unsafe { xmlStrdup(cs("foo").as_ptr()) };
        let p = unsafe { xmlStrncat(cur, cs("barbaz").as_ptr(), 3) };
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), b"foobar");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
    }

    #[test]
    fn xml_strsub_extracts_byte_window() {
        let s = cs("the quick brown fox");
        let p = unsafe { xmlStrsub(s.as_ptr(), 4, 5) };
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), b"quick");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
        // Out-of-range start → NULL.
        assert!(unsafe { xmlStrsub(s.as_ptr(), 1000, 3) }.is_null());
        // Negative args → NULL.
        assert!(unsafe { xmlStrsub(s.as_ptr(), -1, 3) }.is_null());
    }

    #[test]
    fn xml_utf8_strlen_counts_codepoints() {
        // ASCII: bytes == chars.
        assert_eq!(unsafe { xmlUTF8Strlen(cs("hello").as_ptr()) }, 5);
        // German umlaut ä is 2 UTF-8 bytes but one codepoint.
        let s = cs("älämänt");
        assert_eq!(unsafe { xmlUTF8Strlen(s.as_ptr()) }, 7);
        // NULL → 0.
        assert_eq!(unsafe { xmlUTF8Strlen(ptr::null()) }, 0);
    }

    #[test]
    fn xml_utf8_strsize_byte_length_of_n_chars() {
        // First 3 chars of "älämänt" = ä (2) + l (1) + ä (2) = 5 bytes.
        let s = cs("älämänt");
        assert_eq!(unsafe { xmlUTF8Strsize(s.as_ptr(), 3) }, 5);
        // n beyond length → full byte length.
        assert_eq!(unsafe { xmlUTF8Strsize(s.as_ptr(), 100) },
                   s.to_bytes().len() as c_int);
    }

    #[test]
    fn xml_utf8_strpos_returns_byte_offset_pointer() {
        let s = cs("ax̱bc");        // x̱ = 'x' + combining low line (U+0331), 1+2 bytes
        // Position 0 → 'a' (offset 0).
        let p = unsafe { xmlUTF8Strpos(s.as_ptr(), 0) };
        assert_eq!(p, s.as_ptr() as *const c_char);
        // Position 3 (after a, x, combining-low-line) → 'b' (offset 1+1+2=4).
        let p = unsafe { xmlUTF8Strpos(s.as_ptr(), 3) };
        assert_eq!(unsafe { p.offset_from(s.as_ptr()) }, 4);
        // Out-of-range → NULL.
        assert!(unsafe { xmlUTF8Strpos(s.as_ptr(), 100) }.is_null());
    }

    #[test]
    fn xml_utf8_strloc_codepoint_index() {
        let s = cs("hello world");
        let needle = cs("world");
        // "world" starts at byte 6 == codepoint 6 (all ASCII).
        assert_eq!(unsafe { xmlUTF8Strloc(s.as_ptr(), needle.as_ptr()) }, 6);
        // Not found → -1.
        assert_eq!(unsafe { xmlUTF8Strloc(s.as_ptr(), cs("xyz").as_ptr()) }, -1);
    }

    #[test]
    fn xml_utf8_charcmp_compares_first_codepoint() {
        assert_eq!(unsafe { xmlUTF8Charcmp(cs("apple").as_ptr(), cs("apricot").as_ptr()) }, 0);
        assert!(unsafe { xmlUTF8Charcmp(cs("apple").as_ptr(), cs("banana").as_ptr()) } < 0);
    }

    #[test]
    fn xml_utf8_strndup_takes_n_codepoints() {
        let s = cs("hëllo wörld");          // 'ë' and 'ö' are 2 bytes each
        let p = unsafe { xmlUTF8Strndup(s.as_ptr(), 5) };
        // First 5 codepoints = "hëllo" (1+2+1+1+1 = 6 bytes)
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), "hëllo".as_bytes());
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
    }

    #[test]
    fn xml_split_qname_legacy_returns_full_name_when_no_colon() {
        let mut pfx: *mut c_char = ptr::null_mut();
        // Prefix:local case — same shape as xmlSplitQName2.
        let local = unsafe { xmlSplitQName(ptr::null_mut(), cs("dc:title").as_ptr(), &mut pfx) };
        assert_eq!(unsafe { CStr::from_ptr(pfx)   }.to_bytes(), b"dc");
        assert_eq!(unsafe { CStr::from_ptr(local) }.to_bytes(), b"title");
        unsafe {
            crate::parse::xml_free_impl(pfx as *mut _);
            crate::parse::xml_free_impl(local as *mut _);
        }
        // No-colon case — legacy returns a copy of the full name and NULL prefix.
        let mut pfx2: *mut c_char = ptr::null_mut();
        let local2 = unsafe { xmlSplitQName(ptr::null_mut(), cs("foo").as_ptr(), &mut pfx2) };
        assert!(pfx2.is_null());
        assert_eq!(unsafe { CStr::from_ptr(local2) }.to_bytes(), b"foo");
        unsafe { crate::parse::xml_free_impl(local2 as *mut _); }
    }

    #[test]
    fn xml_split_qname2_splits_on_colon() {
        let mut pfx: *mut c_char = ptr::null_mut();
        let local = unsafe { xmlSplitQName2(cs("dc:title").as_ptr(), &mut pfx) };
        assert!(!local.is_null());
        assert!(!pfx.is_null());
        assert_eq!(unsafe { CStr::from_ptr(pfx)   }.to_bytes(), b"dc");
        assert_eq!(unsafe { CStr::from_ptr(local) }.to_bytes(), b"title");
        unsafe {
            crate::parse::xml_free_impl(pfx as *mut _);
            crate::parse::xml_free_impl(local as *mut _);
        }
        // No colon → NULL.
        let mut pfx2: *mut c_char = ptr::null_mut();
        let local2 = unsafe { xmlSplitQName2(cs("foo").as_ptr(), &mut pfx2) };
        assert!(local2.is_null());
    }

    #[test]
    fn xml_string_text_constants_are_null_terminated() {
        // The libxml2-shared constants used as text-node names — bare
        // bytes, NUL-terminated, addressable as `xmlChar*`.  Now
        // defined in `sup-xml-tree`; we re-import via `extern "C"`
        // so the unit test needs an unsafe block.
        unsafe {
            assert_eq!(&xmlStringText, b"text\0");
            assert_eq!(&xmlStringTextNoenc, b"textnoenc\0");
        }
    }

    #[test]
    fn validate_ncname_returns_zero_on_valid_no_colon_name() {
        // libxml2's inverted convention: 0 == valid, 1 == invalid.
        assert_eq!(unsafe { xmlValidateNCName(cs("hello").as_ptr(),  0) }, 0);
        assert_eq!(unsafe { xmlValidateNCName(cs("_under").as_ptr(), 0) }, 0);
        // Colons are forbidden in NCNames.
        assert_eq!(unsafe { xmlValidateNCName(cs("pre:fix").as_ptr(), 0) }, 1);
        // Empty is invalid.
        assert_eq!(unsafe { xmlValidateNCName(cs("").as_ptr(), 0) }, 1);
        // Leading whitespace rejected when `space == 0`, accepted when 1.
        assert_eq!(unsafe { xmlValidateNCName(cs(" name").as_ptr(), 0) }, 1);
        assert_eq!(unsafe { xmlValidateNCName(cs(" name").as_ptr(), 1) }, 0);
    }

    #[test]
    fn validate_qname_accepts_prefix_local_pair() {
        // 0 == valid.
        assert_eq!(unsafe { xmlValidateQName(cs("dc:title").as_ptr(), 0) }, 0);
        assert_eq!(unsafe { xmlValidateQName(cs("local").as_ptr(),    0) }, 0);
        // Both halves must be NCNames.
        assert_eq!(unsafe { xmlValidateQName(cs(":local").as_ptr(),   0) }, 1);
        assert_eq!(unsafe { xmlValidateQName(cs("pre:").as_ptr(),     0) }, 1);
        assert_eq!(unsafe { xmlValidateQName(cs("a:b:c").as_ptr(),    0) }, 1);
    }
}
