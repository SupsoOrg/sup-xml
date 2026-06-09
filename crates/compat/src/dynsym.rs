//! Resolve a C symbol that may be provided by another module already
//! loaded into this process.
//!
//! The shim soft-links to libxslt: it calls libxslt's functions (e.g.
//! `xsltLoadDocument`, `xsltCheckRead`) when a consumer such as lxml has
//! libxslt loaded, but does not link libxslt itself — libxslt depends on
//! libxml2, which *is* this crate's cdylib, so a build-time link would be
//! circular.  On Unix the lookup is `dlsym(RTLD_DEFAULT, …)`; Windows has
//! no RTLD_DEFAULT, so we enumerate the process's loaded modules and probe
//! each with `GetProcAddress`.

use std::os::raw::{c_char, c_void};

/// Resolve `name` (a NUL-terminated C string) to a symbol pointer if any
/// module currently loaded in this process exports it, else null.
#[cfg(unix)]
pub(crate) fn lookup(name: *const c_char) -> *mut c_void {
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, sym: *const c_char) -> *mut c_void;
    }
    // RTLD_DEFAULT searches every loaded image — the handle libxslt's own
    // callers use.  -2 on macOS; the platforms we target accept it as the
    // search-all sentinel.
    let rtld_default = -2isize as usize as *mut c_void;
    // SAFETY: dlsym with RTLD_DEFAULT and a valid C string has no
    // preconditions; it returns null when the symbol is absent.
    unsafe { dlsym(rtld_default, name) }
}

/// Resolve `name` (a NUL-terminated C string) to a symbol pointer if any
/// module currently loaded in this process exports it, else null.
#[cfg(windows)]
pub(crate) fn lookup(name: *const c_char) -> *mut c_void {
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32EnumProcessModules(
            process: *mut c_void,
            modules: *mut *mut c_void,
            cb: u32,
            needed: *mut u32,
        ) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }
    // SAFETY: the module handles come straight from K32EnumProcessModules
    // and GetProcAddress reports a missing symbol as null; both calls are
    // standard Win32 API uses with pointer-sized return values.
    unsafe {
        let process = GetCurrentProcess();
        // First pass: discover how many bytes of module handles exist.
        let mut needed: u32 = 0;
        if K32EnumProcessModules(process, std::ptr::null_mut(), 0, &mut needed) == 0 || needed == 0 {
            return std::ptr::null_mut();
        }
        let count = needed as usize / std::mem::size_of::<*mut c_void>();
        let mut modules = vec![std::ptr::null_mut::<c_void>(); count];
        let bytes = (modules.len() * std::mem::size_of::<*mut c_void>()) as u32;
        if K32EnumProcessModules(process, modules.as_mut_ptr(), bytes, &mut needed) == 0 {
            return std::ptr::null_mut();
        }
        for &module in &modules {
            let p = GetProcAddress(module, name);
            if !p.is_null() {
                return p;
            }
        }
        std::ptr::null_mut()
    }
}
