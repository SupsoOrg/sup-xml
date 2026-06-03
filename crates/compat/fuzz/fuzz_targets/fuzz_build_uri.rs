#![no_main]

//! Fuzz target — drive `xmlBuildURI`'s Rust core (`build_uri`) with
//! arbitrary `(rel, base)` pairs and assert it never panics.
//!
//! The input bytes are interpreted as UTF-8 (returns early on
//! invalid).  A NUL byte (if any) splits the input into the
//! relative URI and the optional base; without a NUL the whole
//! input is the relative URI and `base` is `None`.  That gives
//! libFuzzer's mutation engine a cheap way to explore both axes.
//!
//! The Rust entry point bypasses the C ABI's CString round-trip so
//! fuzzer cycles aren't consumed on invalid-UTF-8 / interior-NUL
//! errors that the FFI rejects before any URI logic runs.

use libfuzzer_sys::fuzz_target;
use sup_xml_compat::uri::__fuzz_build_uri;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let (rel, base) = match s.find('\0') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None    => (s, None),
    };
    let _ = __fuzz_build_uri(rel, base);
});
