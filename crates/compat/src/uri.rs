//! `xmlURI` + parse/build/free.
//!
//! Used by xinclude, `xml:base`, and any consumer that resolves
//! relative URIs against a base.  Hand-rolled because pulling in a
//! full URI crate is overkill for the subset libxml2 exposes — we
//! only need scheme/authority/path/query/fragment splitting and
//! base+relative resolution per RFC 3986 § 5.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::alloc::alloc_registered_cstring;

/// `_xmlURI` — byte-exact mirror.  Verified offsets via offsetof
/// against `/usr/include/libxml/uri.h`.
#[repr(C)]
pub struct xmlURI {
    pub scheme:    *mut c_char,  //  0
    pub opaque:    *mut c_char,  //  8
    pub authority: *mut c_char,  // 16
    pub server:    *mut c_char,  // 24
    pub user:      *mut c_char,  // 32
    pub port:      c_int,        // 40
    _pad_port:     u32,          // 44
    pub path:      *mut c_char,  // 48
    pub query:     *mut c_char,  // 56
    pub fragment:  *mut c_char,  // 64
    pub cleanup:   c_int,        // 72
    _pad_cleanup:  u32,          // 76
    pub query_raw: *mut c_char,  // 80
}
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlURI, scheme)    ==  0);
    assert!(offset_of!(xmlURI, opaque)    ==  8);
    assert!(offset_of!(xmlURI, authority) == 16);
    assert!(offset_of!(xmlURI, server)    == 24);
    assert!(offset_of!(xmlURI, user)      == 32);
    assert!(offset_of!(xmlURI, port)      == 40);
    assert!(offset_of!(xmlURI, path)      == 48);
    assert!(offset_of!(xmlURI, query)     == 56);
    assert!(offset_of!(xmlURI, fragment)  == 64);
    assert!(offset_of!(xmlURI, cleanup)   == 72);
    assert!(offset_of!(xmlURI, query_raw) == 80);
    assert!(std::mem::size_of::<xmlURI>() == 88);
};

// ── parse ─────────────────────────────────────────────────────────────────

/// `xmlParseURI(str)` — parse a URI string into its components.
/// Returns a heap-allocated `xmlURI` (caller releases via
/// [`xmlFreeURI`]).  NULL on NULL input or a string that violates the
/// RFC 3986 character grammar (see [`uri_chars_valid`]).
///
/// The validation matters beyond well-formedness reporting: lxml's
/// `_uriValidOrRaise` treats a NULL return as "invalid namespace URI"
/// and raises `ValueError`, so an over-permissive parse here lets
/// non-conforming namespace URIs (e.g. raw non-ASCII bytes) through.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseURI(s: *const c_char) -> *mut xmlURI {
    if s.is_null() { return ptr::null_mut(); }
    let src = match unsafe { CStr::from_ptr(s) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    if !uri_chars_valid(src) {
        return ptr::null_mut();
    }
    let parts = parse_uri_str(src);
    Box::into_raw(Box::new(parts.into_xml_uri()))
}

/// True when every byte of `s` is permitted in an RFC 3986 URI
/// reference: the unreserved and reserved (gen-delims + sub-delims)
/// character sets plus `%`, where each `%` must introduce a
/// two-hex-digit escape.  Everything else — raw non-ASCII bytes,
/// spaces, control characters, `<` `>` `{` `}` `|` `\` `^` `"` and a
/// backtick, and lone/truncated `%` escapes — is rejected, matching
/// libxml2's `xmlParseURI`.
///
/// This is intentionally a character-class gate, not a full grammar
/// parse: it draws the same accept/reject line libxml2 does for the
/// inputs callers actually hand `xmlParseURI` (validity probes for
/// namespace URIs, `xml:base`, etc.) without the permissive splitting
/// in [`parse_uri_str`] — which stays lenient because `xmlBuildURI`'s
/// path resolution relies on it.
fn uri_chars_valid(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
            continue;
        }
        let ok = b.is_ascii_alphanumeric()
            || matches!(
                b,
                // unreserved
                b'-' | b'.' | b'_' | b'~'
                // gen-delims
                | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@'
                // sub-delims
                | b'!' | b'$' | b'&' | b'\'' | b'(' | b')'
                | b'*' | b'+' | b',' | b';' | b'='
            );
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

/// Decide whether a filename handed to `xmlOutputBufferCreateFilename` /
/// `xmlParserInputBufferCreateFilename` should be percent-unescaped
/// before the file is opened.
///
/// libxml2 parses the filename as a URI and unescapes it only when that
/// parse succeeds **and** the URI has no scheme or the `file` scheme — a
/// "limit the damage" guard (libxml2's own comment) that leaves names
/// which aren't valid URIs (embedded spaces, raw non-ASCII bytes, a lone
/// `%`) untouched.  A plain path like `dir/a%2520b.xml` parses cleanly
/// with no scheme, so it is unescaped to `dir/a%20b.xml`; that is why
/// lxml percent-escapes a literal `%` to `%25` before serializing to a
/// path — it relies on this unescape to round-trip back.
pub(crate) fn filename_should_unescape(uri: &str) -> bool {
    uri_chars_valid(uri) && matches!(parse_uri_str(uri).scheme, None | Some("file"))
}

/// `xmlFreeURI(uri)` — release.  NULL-safe.  Frees every non-NULL
/// string field via libc free (matching libxml2's xmlFree contract
/// for these specific allocations — they originate from us via
/// alloc_registered_cstring).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeURI(uri: *mut xmlURI) {
    if uri.is_null() { return; }
    // SAFETY: uri came from xmlParseURI (Box::into_raw).
    let b = unsafe { Box::from_raw(uri) };
    // Each string was allocated via alloc_registered_cstring → xmlFree
    // recognises it via the alloc registry.
    for p in [b.scheme, b.opaque, b.authority, b.server, b.user,
              b.path, b.query, b.fragment, b.query_raw] {
        if !p.is_null() {
            // SAFETY: `p` was alloc_registered_cstring'd by us; the
            // registry-aware free is the only valid release path.
            unsafe { crate::parse::xml_free_impl(p as *mut std::os::raw::c_void); }
        }
    }
}

/// `xmlBuildURI(uri, base)` — resolve relative `uri` against `base`.
/// Returns a heap-allocated string (caller `xmlFree`s) or NULL.
///
/// Implements RFC 3986 § 5.3 "Component Recomposition".  When the
/// relative `uri` is already absolute (has a scheme), it's returned
/// verbatim; otherwise the base supplies scheme + authority + the
/// merged path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlBuildURI(
    uri:  *const c_char,
    base: *const c_char,
) -> *mut c_char {
    if uri.is_null() {
        return ptr::null_mut();
    }
    let rel = match unsafe { CStr::from_ptr(uri) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    let base_s: Option<&str> = if base.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(base) }.to_str() {
            Ok(s) => Some(s),
            Err(_) => return ptr::null_mut(),
        }
    };
    let result = build_uri(rel, base_s);
    alloc_registered_cstring(result.as_bytes())
}

// ── parsing innards (RFC 3986 § 3) ─────────────────────────────────────────

#[derive(Default)]
struct UriParts<'a> {
    scheme:    Option<&'a str>,
    authority: Option<&'a str>,
    user:      Option<&'a str>,
    host:      Option<&'a str>,
    port:      Option<c_int>,
    path:      &'a str,
    query:     Option<&'a str>,
    fragment:  Option<&'a str>,
}

impl<'a> UriParts<'a> {
    fn into_xml_uri(self) -> xmlURI {
        fn dup(s: Option<&str>) -> *mut c_char {
            match s {
                Some(s) => alloc_registered_cstring(s.as_bytes()),
                None    => ptr::null_mut(),
            }
        }
        let query_raw = dup(self.query);
        xmlURI {
            scheme:    dup(self.scheme),
            opaque:    ptr::null_mut(),
            authority: dup(self.authority),
            server:    dup(self.host),
            user:      dup(self.user),
            port:      self.port.unwrap_or(0),
            _pad_port: 0,
            path:      if self.path.is_empty() { ptr::null_mut() } else { dup(Some(self.path)) },
            query:     dup(self.query),
            fragment:  dup(self.fragment),
            cleanup:   0,
            _pad_cleanup: 0,
            query_raw,
        }
    }
}

fn parse_uri_str(s: &str) -> UriParts<'_> {
    let mut p = UriParts::default();
    let mut rest = s;

    // scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"
    if let Some(colon_idx) = rest.find(':') {
        let candidate = &rest[..colon_idx];
        if !candidate.is_empty()
            && candidate.chars().next().unwrap().is_ascii_alphabetic()
            && candidate.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+'|'-'|'.'))
        {
            p.scheme = Some(candidate);
            rest = &rest[colon_idx + 1..];
        }
    }

    // fragment = after first '#'
    if let Some(hash) = rest.find('#') {
        p.fragment = Some(&rest[hash + 1..]);
        rest = &rest[..hash];
    }

    // query = after first '?'
    if let Some(q) = rest.find('?') {
        p.query = Some(&rest[q + 1..]);
        rest = &rest[..q];
    }

    // authority = leading "//" then up to next '/' or end
    if let Some(stripped) = rest.strip_prefix("//") {
        let (auth, after) = match stripped.find('/') {
            Some(i) => (&stripped[..i], &stripped[i..]),
            None    => (stripped, ""),
        };
        p.authority = Some(auth);
        // userinfo @ host : port
        let (userinfo, host_port) = match auth.find('@') {
            Some(i) => (Some(&auth[..i]), &auth[i + 1..]),
            None    => (None, auth),
        };
        p.user = userinfo;
        let (host, port_str) = match host_port.rfind(':') {
            // Beware of IPv6 [::1]:80 — naive rfind works for hostnames
            // and IPv4 only; libxml2 handles bracketed v6 elsewhere.
            Some(i) if !host_port.starts_with('[') => (&host_port[..i], Some(&host_port[i + 1..])),
            _ => (host_port, None),
        };
        if !host.is_empty() {
            p.host = Some(host);
        }
        if let Some(pstr) = port_str {
            p.port = pstr.parse::<c_int>().ok();
        }
        rest = after;
    }

    p.path = rest;
    p
}

// ── RFC 3986 § 5.2 transform-reference ────────────────────────────────────

/// Fuzz-only entry point exposing the otherwise-private Rust
/// implementation of `xmlBuildURI` without the C ABI's CString
/// round-trip (which would consume fuzzer cycles on non-bug noise
/// like invalid UTF-8 / interior nulls).  Hidden behind the
/// crate-private `fuzzing` feature so the symbol disappears from
/// production builds.
#[doc(hidden)]
#[cfg(feature = "fuzzing")]
pub fn __fuzz_build_uri(rel: &str, base: Option<&str>) -> String {
    build_uri(rel, base)
}

fn build_uri(rel: &str, base: Option<&str>) -> String {
    let r = parse_uri_str(rel);
    let b = base.map(parse_uri_str);

    // If rel has a scheme, it's already absolute.
    if r.scheme.is_some() {
        return rebuild(&r);
    }
    let b = match b {
        Some(b) => b,
        None    => return rebuild(&r),  // no base → return as-is
    };

    let mut t = UriParts::default();
    t.scheme = b.scheme;
    if r.authority.is_some() {
        // R has authority → take R's authority + R's path.
        t.authority = r.authority;
        t.user      = r.user;
        t.host      = r.host;
        t.port      = r.port;
        t.path      = r.path;
        t.query     = r.query;
    } else {
        t.authority = b.authority;
        t.user      = b.user;
        t.host      = b.host;
        t.port      = b.port;
        if r.path.is_empty() {
            t.path = b.path;
            t.query = r.query.or(b.query);
        } else if r.path.starts_with('/') {
            t.path = r.path;
            t.query = r.query;
        } else {
            // Merge base path + rel path.
            // We need an owned String, so build below and replace.
            let merged = merge_paths(b.path, b.authority, r.path);
            // Stash the merged path in a leaked location via a trick:
            // we own it as a String, but UriParts.path is &str.  Use
            // a local then format inline.
            let removed = remove_dot_segments(&merged);
            return format_uri(b.scheme, b.authority, &removed, r.query, r.fragment);
        }
    }
    t.fragment = r.fragment;
    rebuild_with_dot_removal(&t)
}

fn merge_paths(base_path: &str, base_authority: Option<&str>, rel_path: &str) -> String {
    if base_authority.is_some() && base_path.is_empty() {
        format!("/{rel_path}")
    } else {
        // Right-most slash of base_path; drop everything after.
        match base_path.rfind('/') {
            Some(i) => format!("{}{}", &base_path[..=i], rel_path),
            None    => rel_path.to_string(),
        }
    }
}

fn remove_dot_segments(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut input = path;
    while !input.is_empty() {
        if let Some(stripped) = input.strip_prefix("../") { input = stripped; }
        else if let Some(stripped) = input.strip_prefix("./") { input = stripped; }
        else if input.starts_with("/./") { input = &input[2..]; }
        else if input == "/." { input = "/"; }
        else if input.starts_with("/../") {
            input = &input[3..];
            // remove last segment in out (if any)
            pop_last_segment(&mut out);
        }
        else if input == "/.." {
            input = "/";
            pop_last_segment(&mut out);
        }
        else if input == "." || input == ".." { break; }
        else {
            // Move first segment from input to out.  Byte-level search
            // because `&input[1..]` panics when the leading char is
            // multi-byte UTF-8 (byte 1 isn't a char boundary); `/` is
            // ASCII, so its byte position is always a char boundary
            // and the subsequent string slices are safe.
            let next_slash = input.as_bytes()[1..]
                .iter()
                .position(|&b| b == b'/')
                .map(|i| i + 1)
                .unwrap_or(input.len());
            out.push_str(&input[..next_slash]);
            input = &input[next_slash..];
        }
    }
    out
}

fn pop_last_segment(out: &mut String) {
    if let Some(i) = out.rfind('/') {
        out.truncate(i);
    } else {
        out.clear();
    }
}

fn rebuild(p: &UriParts) -> String {
    let mut out = String::new();
    if let Some(s) = p.scheme { out.push_str(s); out.push(':'); }
    if let Some(a) = p.authority { out.push_str("//"); out.push_str(a); }
    out.push_str(p.path);
    if let Some(q) = p.query { out.push('?'); out.push_str(q); }
    if let Some(f) = p.fragment { out.push('#'); out.push_str(f); }
    out
}

fn rebuild_with_dot_removal(p: &UriParts) -> String {
    let cleaned = remove_dot_segments(p.path);
    format_uri(p.scheme, p.authority, &cleaned, p.query, p.fragment)
}

fn format_uri(
    scheme: Option<&str>, authority: Option<&str>,
    path: &str, query: Option<&str>, fragment: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(s) = scheme { out.push_str(s); out.push(':'); }
    if let Some(a) = authority { out.push_str("//"); out.push_str(a); }
    out.push_str(path);
    if let Some(q) = query { out.push('?'); out.push_str(q); }
    if let Some(f) = fragment { out.push('#'); out.push_str(f); }
    out
}

// ── extra entry points used by libxslt ────────────────────────────────────

/// `xmlCreateURI()` — allocate an empty `xmlURI` struct.  Caller
/// populates the fields and eventually releases via [`xmlFreeURI`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCreateURI() -> *mut xmlURI {
    Box::into_raw(Box::new(xmlURI {
        scheme:    ptr::null_mut(),
        opaque:    ptr::null_mut(),
        authority: ptr::null_mut(),
        server:    ptr::null_mut(),
        user:      ptr::null_mut(),
        port:      0,
        _pad_port: 0,
        path:      ptr::null_mut(),
        query:     ptr::null_mut(),
        fragment:  ptr::null_mut(),
        cleanup:   0,
        _pad_cleanup: 0,
        query_raw: ptr::null_mut(),
    }))
}

/// `xmlURIEscapeStr(str, list)` — percent-encode `str` for use in a
/// URI component.  `list` is a string of additional ASCII chars to
/// leave UN-escaped (libxml2's convention).  Returns a fresh heap
/// allocation; NULL on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlURIEscapeStr(
    s:    *const c_char,
    list: *const c_char,
) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `s` is NUL-terminated readable.
    let src = unsafe { CStr::from_ptr(s) }.to_bytes();
    let extra: &[u8] = if list.is_null() {
        &[]
    } else {
        // SAFETY: caller asserts `list` is NUL-terminated readable.
        unsafe { CStr::from_ptr(list) }.to_bytes()
    };
    // RFC 3986 § 2.3: ALPHA / DIGIT / `-` / `.` / `_` / `~` are
    // always safe.  Anything else is percent-encoded unless the
    // caller's `list` whitelists it.
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    for &b in src {
        let safe = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~')
            || extra.contains(&b);
        if safe {
            out.push(b);
        } else {
            out.extend_from_slice(&[
                b'%',
                hex_nibble(b >> 4),
                hex_nibble(b & 0x0F),
            ]);
        }
    }
    alloc_registered_cstring(&out)
}

/// `xmlURIUnescapeString(str, len, target)` — reverse of
/// `xmlURIEscapeStr`: replace `%XX` sequences with the decoded byte.
/// `len` is the length of `str` to process, or 0 / negative to
/// use a NUL-terminated read.  When `target` is NULL, allocate a
/// fresh heap string; otherwise decode into the caller's buffer
/// (which must be at least `len + 1` bytes).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlURIUnescapeString(
    s: *const c_char,
    len: c_int,
    target: *mut c_char,
) -> *mut c_char {
    if s.is_null() {
        return ptr::null_mut();
    }
    // Determine the source slice.  len <= 0 means "treat as
    // NUL-terminated."  When len > 0, libxml2 stops at the first NUL
    // inside that window.
    let src_bytes: &[u8] = if len <= 0 {
        // SAFETY: caller asserts NUL-terminated readable.
        unsafe { CStr::from_ptr(s) }.to_bytes()
    } else {
        // SAFETY: caller asserts `s` readable for `len` bytes.
        let raw = unsafe { std::slice::from_raw_parts(s as *const u8, len as usize) };
        match raw.iter().position(|&b| b == 0) {
            Some(i) => &raw[..i],
            None    => raw,
        }
    };

    let mut out: Vec<u8> = Vec::with_capacity(src_bytes.len());
    let mut i = 0;
    while i < src_bytes.len() {
        let b = src_bytes[i];
        if b == b'%' && i + 2 < src_bytes.len() {
            if let (Some(hi), Some(lo)) = (
                nibble_value(src_bytes[i + 1]),
                nibble_value(src_bytes[i + 2]),
            ) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }

    if target.is_null() {
        alloc_registered_cstring(&out)
    } else {
        // SAFETY: caller asserts `target` is writable for `len + 1` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(out.as_ptr(), target as *mut u8, out.len());
            *target.add(out.len()) = 0;
        }
        target
    }
}

/// `xmlSaveUri(uri)` — serialize a parsed `xmlURI` back into a URI
/// string.  Returns a fresh heap allocation; NULL on NULL input.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSaveUri(uri: *mut xmlURI) -> *mut c_char {
    if uri.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts `uri` came from xmlParseURI / xmlCreateURI.
    let u = unsafe { &*uri };
    let mut out = String::new();
    let pull = |p: *mut c_char| -> Option<&str> {
        if p.is_null() { None }
        else {
            // SAFETY: each string field was alloc_registered_cstring'd
            // by us — NUL-terminated UTF-8.
            unsafe { CStr::from_ptr(p) }.to_str().ok()
        }
    };
    if let Some(s) = pull(u.scheme)    { out.push_str(s); out.push(':'); }
    if let Some(a) = pull(u.authority) { out.push_str("//"); out.push_str(a); }
    if let Some(p) = pull(u.path)      { out.push_str(p); }
    if let Some(q) = pull(u.query)     { out.push('?'); out.push_str(q); }
    if let Some(f) = pull(u.fragment)  { out.push('#'); out.push_str(f); }
    alloc_registered_cstring(out.as_bytes())
}

#[inline]
fn hex_nibble(n: u8) -> u8 {
    match n {
        0..=9   => b'0' + n,
        10..=15 => b'A' + (n - 10),
        _       => b'0',
    }
}

#[inline]
fn nibble_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── PHP-needed extras ───────────────────────────────────────────────────

/// `xmlPathToURI(path)` — convert a filesystem-style path to a
/// `file://`-scheme URI.  Caller frees via xmlFree.  Returns NULL on
/// NULL input or alloc failure.
///
/// libxml2's full implementation does percent-encoding of unsafe
/// characters; we do the minimum (encode space, `?`, `#`) sufficient
/// for typical filesystem paths.  Plain ASCII paths round-trip
/// unchanged.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPathToURI(path: *const c_char) -> *mut c_char {
    if path.is_null() { return ptr::null_mut(); }
    let s = match unsafe { std::ffi::CStr::from_ptr(path) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    // Already a URI?  libxml2 returns a copy in that case.
    if s.contains(':') && s.bytes().take_while(|b| *b != b':').all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
    }) {
        return crate::alloc::alloc_registered_cstring(s.as_bytes());
    }
    let mut out = String::with_capacity(s.len() + 8);
    if s.starts_with('/') {
        out.push_str("file://");
    } else {
        out.push_str("file:");
    }
    for b in s.bytes() {
        match b {
            b' '             => out.push_str("%20"),
            b'?'             => out.push_str("%3F"),
            b'#'             => out.push_str("%23"),
            _                => out.push(b as char),
        }
    }
    crate::alloc::alloc_registered_cstring(out.as_bytes())
}

/// `xmlCanonicPath(path)` — normalize a filesystem path.  We delegate
/// to `std::fs::canonicalize` when the path exists; otherwise return
/// a heap copy of the original (libxml2's "best effort" contract).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCanonicPath(path: *const c_char) -> *mut c_char {
    if path.is_null() { return ptr::null_mut(); }
    let s = match unsafe { std::ffi::CStr::from_ptr(path) }.to_str() {
        Ok(s)  => s,
        Err(_) => return ptr::null_mut(),
    };
    let canonical = std::fs::canonicalize(s)
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| s.to_string());
    crate::alloc::alloc_registered_cstring(canonical.as_bytes())
}

/// `xmlParseURIReference(uri, str)` — parse `str` into an existing
/// `xmlURI` struct (caller-owned).  Returns 0 on success, non-zero
/// on parse failure.
///
/// Today we accept the call but don't populate the fields — consumers
/// that read xmlURI fields after this call will see whatever was in
/// the struct beforehand.  Real consumers expecting parsed components
/// should use [`xmlParseURI`] (returns a new ctxt with fields set).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlParseURIReference(
    uri: *mut xmlURI,
    str: *const c_char,
) -> c_int {
    if uri.is_null() || str.is_null() { return -1; }
    0
}

// ── unit tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn cs(s: &str) -> CString { CString::new(s).unwrap() }

    fn build(rel: &str, base: Option<&str>) -> String {
        let rel_c = cs(rel);
        let base_c = base.map(cs);
        let base_p = base_c.as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null());
        let p = unsafe { xmlBuildURI(rel_c.as_ptr(), base_p) };
        assert!(!p.is_null());
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        unsafe { crate::parse::xml_free_impl(p as *mut std::os::raw::c_void); }
        s
    }

    #[test]
    fn build_uri_rfc3986_normal_examples() {
        // Examples from RFC 3986 § 5.4.1, base = http://a/b/c/d;p?q
        let base = Some("http://a/b/c/d;p?q");
        assert_eq!(build("g:h",   base), "g:h");
        assert_eq!(build("g",     base), "http://a/b/c/g");
        assert_eq!(build("./g",   base), "http://a/b/c/g");
        assert_eq!(build("g/",    base), "http://a/b/c/g/");
        assert_eq!(build("/g",    base), "http://a/g");
        assert_eq!(build("//g",   base), "http://g");
        assert_eq!(build("?y",    base), "http://a/b/c/d;p?y");
        assert_eq!(build("g?y",   base), "http://a/b/c/g?y");
        assert_eq!(build("#s",    base), "http://a/b/c/d;p?q#s");
        assert_eq!(build("g#s",   base), "http://a/b/c/g#s");
        assert_eq!(build("g?y#s", base), "http://a/b/c/g?y#s");
        assert_eq!(build("",      base), "http://a/b/c/d;p?q");
    }

    #[test]
    fn build_uri_dot_dot_segments() {
        let base = Some("http://a/b/c/d;p?q");
        assert_eq!(build("../g",    base), "http://a/b/g");
        assert_eq!(build("../../g", base), "http://a/g");
    }

    #[test]
    fn parse_uri_components() {
        let s = cs("http://user@host:8080/p/a?q=1#frag");
        let u = unsafe { xmlParseURI(s.as_ptr()) };
        assert!(!u.is_null());
        unsafe {
            let r = &*u;
            assert_eq!(CStr::from_ptr(r.scheme).to_str().unwrap(), "http");
            assert_eq!(CStr::from_ptr(r.user).to_str().unwrap(),   "user");
            assert_eq!(CStr::from_ptr(r.server).to_str().unwrap(), "host");
            assert_eq!(r.port, 8080);
            assert_eq!(CStr::from_ptr(r.path).to_str().unwrap(),     "/p/a");
            assert_eq!(CStr::from_ptr(r.query).to_str().unwrap(),    "q=1");
            assert_eq!(CStr::from_ptr(r.fragment).to_str().unwrap(), "frag");
            xmlFreeURI(u);
        }
    }

    #[test]
    fn null_safety() {
        assert!(unsafe { xmlParseURI(ptr::null()) }.is_null());
        assert!(unsafe { xmlBuildURI(ptr::null(), ptr::null()) }.is_null());
        unsafe { xmlFreeURI(ptr::null_mut()); }
    }

    #[test]
    fn parse_uri_rejects_non_rfc3986_characters() {
        // Accept/reject boundary verified against system libxml2's
        // xmlParseURI.  lxml's _uriValidOrRaise turns a NULL return
        // into ValueError, so these are namespace-URI validity cases.
        let accept = [
            "http://abc/", "urn:foo:bar", "foo", "../foo/bar", "/abs/path",
            "http://h/p?q=1#f", "http://h/%41%42", "http://[::1]/p",
            "mailto:a@b.com", "tag:example.com,2001:foo", "",
        ];
        let reject = [
            "http://Ãڀㄠ/",   // raw non-ASCII (the test_unicode case)
            "a b",            // space
            "http://h/<>",    // angle brackets
            "http://h/{x}",   // braces
            "http://h/\x01",  // control char
            "http://h/a^b",   // caret
            "a|b",            // pipe
            "a\\b",           // backslash
            "http://h/%",     // lone percent
            "http://h/%4",    // truncated escape
            "http://h/%zz",   // non-hex escape
        ];
        for s in accept {
            let c = CString::new(s).unwrap();
            let u = unsafe { xmlParseURI(c.as_ptr()) };
            assert!(!u.is_null(), "expected accept: {s:?}");
            unsafe { xmlFreeURI(u); }
        }
        for s in reject {
            let c = CString::new(s).unwrap();
            let u = unsafe { xmlParseURI(c.as_ptr()) };
            assert!(u.is_null(), "expected reject: {s:?}");
        }
    }

    // ── tests for the libxslt-facing helpers ─────────────────────────────

    #[test]
    fn create_uri_returns_empty_struct() {
        let u = unsafe { xmlCreateURI() };
        assert!(!u.is_null());
        let r = unsafe { &*u };
        assert!(r.scheme.is_null() && r.path.is_null() && r.authority.is_null());
        unsafe { xmlFreeURI(u); }
    }

    #[test]
    fn escape_str_percent_encodes_unsafe_chars() {
        let input = CString::new("hello world/?&").unwrap();
        // No extra-safe list.
        let p = unsafe { xmlURIEscapeStr(input.as_ptr(), ptr::null()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "hello%20world%2F%3F%26");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
        // With "/" in the extra list → not escaped.
        let extra = CString::new("/").unwrap();
        let p2 = unsafe { xmlURIEscapeStr(input.as_ptr(), extra.as_ptr()) };
        let s2 = unsafe { CStr::from_ptr(p2) }.to_str().unwrap();
        assert_eq!(s2, "hello%20world/%3F%26");
        unsafe { crate::parse::xml_free_impl(p2 as *mut _); }
    }

    #[test]
    fn unescape_string_decodes_percent_sequences() {
        let input = CString::new("hello%20world%2F").unwrap();
        let p = unsafe { xmlURIUnescapeString(input.as_ptr(), -1, ptr::null_mut()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "hello world/");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
    }

    #[test]
    fn unescape_string_respects_len_window() {
        let input = CString::new("abc%20def").unwrap();
        // Only decode first 5 bytes → "abc%2" (incomplete sequence, kept literal)
        let p = unsafe { xmlURIUnescapeString(input.as_ptr(), 5, ptr::null_mut()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap();
        assert_eq!(s, "abc%2");
        unsafe { crate::parse::xml_free_impl(p as *mut _); }
    }

    #[test]
    fn unescape_string_with_target_buffer_writes_into_it() {
        let input = CString::new("a%20b").unwrap();
        let mut buf = [0u8; 16];
        let p = unsafe {
            xmlURIUnescapeString(
                input.as_ptr(),
                -1,
                buf.as_mut_ptr() as *mut c_char,
            )
        };
        // Returns the same target pointer when one is supplied.
        assert_eq!(p, buf.as_mut_ptr() as *mut c_char);
        let s = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) }.to_str().unwrap();
        assert_eq!(s, "a b");
    }

    #[test]
    fn save_uri_round_trips_parsed_uri() {
        let input = CString::new("http://example.com/path?q=1#frag").unwrap();
        let parsed = unsafe { xmlParseURI(input.as_ptr()) };
        assert!(!parsed.is_null());
        let saved = unsafe { xmlSaveUri(parsed) };
        let s = unsafe { CStr::from_ptr(saved) }.to_str().unwrap();
        assert_eq!(s, "http://example.com/path?q=1#frag");
        unsafe {
            crate::parse::xml_free_impl(saved as *mut _);
            xmlFreeURI(parsed);
        }
    }

    /// Regression: `remove_dot_segments` indexed `&input[1..]` directly,
    /// which panics with "byte index 1 is not a char boundary" whenever
    /// the input begins with a multi-byte UTF-8 character. Reachable
    /// via `xmlBuildURI` when the merged path lacks a leading `/` —
    /// e.g. a relative URI of `"é"` against a scheme-only base like
    /// `mailto:user@example.com` whose path component has no `/`,
    /// causing `merge_paths` to return the relative path unchanged.
    /// `extern "C"` panic-on-abort would tear down the test process,
    /// so we exercise the inner helper directly.
    #[test]
    fn remove_dot_segments_handles_multibyte_leading_char() {
        assert_eq!(remove_dot_segments("éh"), "éh");
        assert_eq!(remove_dot_segments("é"), "é");
        assert_eq!(remove_dot_segments("é/foo"), "é/foo");
        // Existing ASCII behaviour unchanged.
        assert_eq!(remove_dot_segments("/a/b"), "/a/b");
        assert_eq!(remove_dot_segments("a/b"), "a/b");
        assert_eq!(remove_dot_segments(""), "");
    }
}
