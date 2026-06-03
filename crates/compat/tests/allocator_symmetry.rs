//! The C-ABI allocator must stay internally consistent even when the
//! surrounding Rust process installs a non-default global allocator.
//!
//! The shim's `xmlMalloc` / `xmlStrdup` / `xmlRealloc` / `xmlFree`
//! family is backed by libc `malloc` / `realloc` / `free`.  If any
//! registry allocation crossed into the Rust global allocator (e.g. a
//! `CString` free of a libc-malloc'd pointer), then with mimalloc
//! installed as `#[global_allocator]` that free would route a
//! libc-owned pointer through mimalloc's heap — corruption.
//!
//! Pinning mimalloc here makes the two heaps genuinely distinct, so
//! these round-trips pass only because every registry pointer is
//! allocated and freed through libc, independent of the Rust allocator.

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use sup_xml_compat::alloc::{alloc_registered_cstring, xmlMemFree, xmlMemMalloc, xmlMemRealloc};

#[test]
fn xml_malloc_freed_through_xml_free() {
    // libc malloc → xmlFree (libc free).  Under mimalloc-global this
    // crashed on the pre-fix code, which freed through the Rust heap.
    unsafe {
        let p = xmlMemMalloc(128);
        assert!(!p.is_null());
        xmlMemFree(p);
    }
}

#[test]
fn registered_cstring_realloced_then_freed() {
    unsafe {
        let p = alloc_registered_cstring(b"mixed-allocator probe");
        assert!(!p.is_null());
        // Grow through xmlRealloc (libc realloc), then free (libc free).
        let grown = xmlMemRealloc(p as *mut c_void, 256);
        assert!(!grown.is_null());
        let s = CStr::from_ptr(grown as *const c_char).to_str().unwrap();
        assert_eq!(s, "mixed-allocator probe");
        xmlMemFree(grown);
    }
}
