//! libxml2-compatible error surface.
//!
//! Three logical pieces:
//!
//! 1. **`xmlError` struct mirror** — `#[repr(C)]`, byte-exact layout
//!    of libxml2's `_xmlError` so C callers can read `err->code`,
//!    `err->message`, etc. at the documented offsets.
//!
//! 2. **Thread-local last-error storage** — libxml2 callers expect
//!    `xmlGetLastError()` to return the most recent error on the
//!    current thread.  The strings inside that `xmlError`
//!    (`message`, `file`, etc.) live in a per-thread scratch arena
//!    that survives until the next error or `xmlResetLastError()`.
//!
//! 3. **5 exported `extern "C"` functions** — `xmlGetLastError`,
//!    `xmlSetStructuredErrorFunc`, `xmlSetGenericErrorFunc`,
//!    `xmlResetLastError`, `xmlResetError`.  Plus an internal Rust
//!    function `record_last_error(&XmlError)` that subsystems call
//!    when they want a Rust-side error to surface through the C API.

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::error::{ErrorLevel, XmlError};

// ── xmlError struct mirror ──────────────────────────────────────────────────
//
// Layout from libxml2's `include/libxml/xmlerror.h`.  Verified against
// libxml2 2.9.13 (macOS SDK) and 2.15.3 (Homebrew current).  We do
// not commit to layout compatibility with libxml2 versions we have
// not verified against; future libxml2 updates are an explicit,
// coordinated bump on our side.  The `t-upstream-layout` c-test
// fails the build if the installed libxml2 header disagrees with
// the offsets below.
//
//   typedef struct _xmlError {
//       int               domain;     // offset  0  — xmlErrorDomain
//       int               code;       // offset  4  — xmlParserErrors
//       char             *message;    // offset  8  — heap-allocated; lives until next error
//       xmlErrorLevel     level;      // offset 16  — xmlErrorLevel (int)
//       char             *file;       // offset 24
//       int               line;       // offset 32
//       char             *str1;       // offset 40  — context-dependent extra strings
//       char             *str2;       // offset 48
//       char             *str3;       // offset 56
//       int               int1;       // offset 64
//       int               int2;       // offset 68  — column (despite the name)
//       void             *ctxt;       // offset 72
//       void             *node;       // offset 80
//   } xmlError;
//
// Size: 88 bytes on 64-bit.  Pointer-typed fields are nullable (libxml2
// uses NULL when a piece of information is unavailable).
//
// Internal `_pad_*` fields exist where C's natural padding would put
// them; we make them explicit so the layout doesn't depend on Rust's
// `#[repr(C)]` interpretation matching the C ABI's expectations.

#[repr(C)]
#[derive(Debug)]
pub struct xmlError {
    pub domain:  c_int,                 //  0
    pub code:    c_int,                 //  4
    pub message: *mut c_char,           //  8
    pub level:   c_int,                 // 16  (xmlErrorLevel)
    _pad_level:  u32,                   // 20  (pad to 8-byte alignment for `file`)
    pub file:    *mut c_char,           // 24
    pub line:    c_int,                 // 32
    _pad_line:   u32,                   // 36
    pub str1:    *mut c_char,           // 40
    pub str2:    *mut c_char,           // 48
    pub str3:    *mut c_char,           // 56
    pub int1:    c_int,                 // 64
    pub int2:    c_int,                 // 68  (column)
    pub ctxt:    *mut c_void,           // 72
    pub node:    *mut c_void,           // 80
}

// SAFETY: `xmlError` is plain old data once constructed.  It's stored
// in thread-local cells and read-only by the C side.  Never shared
// across threads (each thread has its own last_error).
unsafe impl Send for xmlError {}

// ── layout assertions ──────────────────────────────────────────────────────
//
// Compile-time guarantees that every field is at libxml2's documented
// offset.  Drift = build break.  Slice 4-onwards will add a C-side
// `_Static_assert` check that compares THIS layout against the
// vendored libxml2 header, catching the case where our offset table
// here disagrees with the actual upstream layout.

const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlError, domain)  ==  0, "xmlError::domain @ 0");
    assert!(offset_of!(xmlError, code)    ==  4, "xmlError::code @ 4");
    assert!(offset_of!(xmlError, message) ==  8, "xmlError::message @ 8");
    assert!(offset_of!(xmlError, level)   == 16, "xmlError::level @ 16");
    assert!(offset_of!(xmlError, file)    == 24, "xmlError::file @ 24");
    assert!(offset_of!(xmlError, line)    == 32, "xmlError::line @ 32");
    assert!(offset_of!(xmlError, str1)    == 40, "xmlError::str1 @ 40");
    assert!(offset_of!(xmlError, str2)    == 48, "xmlError::str2 @ 48");
    assert!(offset_of!(xmlError, str3)    == 56, "xmlError::str3 @ 56");
    assert!(offset_of!(xmlError, int1)    == 64, "xmlError::int1 @ 64");
    assert!(offset_of!(xmlError, int2)    == 68, "xmlError::int2 @ 68");
    assert!(offset_of!(xmlError, ctxt)    == 72, "xmlError::ctxt @ 72");
    assert!(offset_of!(xmlError, node)    == 80, "xmlError::node @ 80");
    assert!(std::mem::size_of::<xmlError>() == 88, "xmlError total size = 88");
};

// ── thread-local state ─────────────────────────────────────────────────────
//
// `LAST_ERROR` holds the most recent error on the current thread, plus
// the `CString` allocations that back its `message`/`file` pointers.
// The `CString`s live alongside the `xmlError` so they're freed together
// on the next `xmlResetLastError` or replacement error.
//
// `STRUCTURED_HANDLER` / `GENERIC_HANDLER` are the caller-registered
// callbacks invoked at error-recording time (in addition to populating
// LAST_ERROR).  Both are per-thread by libxml2's documented contract
// (xmlSetStructuredErrorFunc → "the function pointer for the current
// thread").

/// libxml2 callback signature for `xmlSetStructuredErrorFunc`.
pub type StructuredErrorFn = unsafe extern "C" fn(user_data: *mut c_void, err: *const xmlError);

/// libxml2 callback signature for `xmlSetGenericErrorFunc`.  Printf-
/// style — caller supplies a varargs format string.  We don't
/// reconstruct varargs from Rust; we pass a pre-formatted message
/// as a single `%s` arg, which all known libxml2 generic-handlers
/// implementations accept.
pub type GenericErrorFn = unsafe extern "C" fn(user_data: *mut c_void, fmt: *const c_char, ...);

/// Holds the thread's most-recent error plus the owned heap
/// allocations backing its `char*` fields.
///
/// The strings live as **raw pointers** (`CString::into_raw`) rather
/// than owned `CString`s.  Stacked Borrows treats a raw pointer
/// derived from a stack-resident `CString` as invalidated when the
/// `CString` is moved (even though the underlying heap allocation
/// doesn't move) — so storing `CString`s in this struct and exposing
/// their `as_ptr()` results to the C side runs afoul of Miri.
///
/// Using `into_raw` transfers ownership from the `CString` to the
/// raw pointer outright; the buffer is heap-stable for the pointer's
/// entire lifetime.  Drop reclaims via `CString::from_raw`.
struct LastErrorSlot {
    err: xmlError,
    /// Raw pointer owning the buffer that backs `err.message`.
    /// `null` ⇔ no message currently stored.
    message_raw: *mut c_char,
    /// Raw pointer owning the buffer that backs `err.file` (or null).
    file_raw:    *mut c_char,
}

impl LastErrorSlot {
    /// Free the heap-allocated strings (called from Drop and from
    /// the replacement path).
    fn free_strings(&mut self) {
        if !self.message_raw.is_null() {
            // SAFETY: pointer came from `CString::into_raw` earlier
            // and hasn't been freed since.
            unsafe { drop(CString::from_raw(self.message_raw)); }
            self.message_raw = ptr::null_mut();
        }
        if !self.file_raw.is_null() {
            unsafe { drop(CString::from_raw(self.file_raw)); }
            self.file_raw = ptr::null_mut();
        }
    }
}

impl Drop for LastErrorSlot {
    fn drop(&mut self) { self.free_strings(); }
}

impl Default for LastErrorSlot {
    fn default() -> Self {
        Self {
            err: zeroed_xml_error(),
            message_raw: ptr::null_mut(),
            file_raw:    ptr::null_mut(),
        }
    }
}

fn zeroed_xml_error() -> xmlError {
    xmlError {
        domain:  0,
        code:    0,
        message: ptr::null_mut(),
        level:   0,
        _pad_level: 0,
        file:    ptr::null_mut(),
        line:    0,
        _pad_line:  0,
        str1:    ptr::null_mut(),
        str2:    ptr::null_mut(),
        str3:    ptr::null_mut(),
        int1:    0,
        int2:    0,
        ctxt:    ptr::null_mut(),
        node:    ptr::null_mut(),
    }
}

struct HandlerSlot {
    structured: Option<StructuredErrorFn>,
    structured_data: *mut c_void,
    generic: Option<GenericErrorFn>,
    generic_data: *mut c_void,
}

impl Default for HandlerSlot {
    fn default() -> Self {
        Self {
            structured: None,
            structured_data: ptr::null_mut(),
            generic: None,
            generic_data: ptr::null_mut(),
        }
    }
}

thread_local! {
    static LAST_ERROR: RefCell<LastErrorSlot> = RefCell::new(LastErrorSlot::default());
    /// `true` while no error has been recorded since the last reset (or
    /// thread start).  We need this because the err struct alone can't
    /// distinguish "no error yet" from "error with all-zero fields" —
    /// libxml2's contract says `xmlGetLastError()` returns NULL in the
    /// former case.
    static LAST_ERROR_PRESENT: RefCell<bool> = const { RefCell::new(false) };
    static HANDLERS: RefCell<HandlerSlot> = RefCell::new(HandlerSlot::default());
}

// ── internal: Rust-side error → thread-local C-shape error ────────────────
//
// Subsystems call `record_last_error(&xml_err)` whenever they want a
// Rust-side error to surface through the C API.  This:
//
// 1. Converts `XmlError` → `xmlError`, allocating CStrings for the
//    `message`/`file` fields (kept alive in the LAST_ERROR slot).
// 2. Invokes the structured handler (if registered) with a pointer to
//    the resulting C struct.
// 3. Invokes the generic handler (if registered) with a printf-style
//    "%s" + the message.
//
// Thread-local — recording an error on one thread is invisible on others.

/// Record an `XmlError` as the thread's most recent error.  Invokes
/// any registered handlers.  Called by subsystem entry points after
/// their Rust-side `Result<_, XmlError>` returns `Err`.
pub fn record_last_error(rust_err: &XmlError) {
    // Build the CStrings outside the RefCell borrow, then transfer
    // ownership into raw pointers via `into_raw`.  The resulting
    // pointers are stable for the slot's entire lifetime — no
    // CString moves happen after we take the pointer (which is what
    // tripped Stacked Borrows in earlier attempts).
    let msg_c = CString::new(rust_err.message.as_bytes())
        .unwrap_or_else(|_| CString::new("error message contained interior NUL byte").unwrap());
    let msg_raw = msg_c.into_raw();
    let file_raw = match rust_err.file.as_ref() {
        Some(f) => {
            let c = CString::new(f.as_bytes())
                .unwrap_or_else(|_| CString::new("<file>").unwrap());
            c.into_raw()
        }
        None => ptr::null_mut(),
    };

    let c_err = xmlError {
        domain:     rust_err.domain as i32,
        code:       rust_err.code as i32,
        message:    msg_raw,
        level:      level_as_i32(rust_err.level),
        _pad_level: 0,
        file:       file_raw,
        line:       rust_err.line.unwrap_or(0) as c_int,
        _pad_line:  0,
        str1:       ptr::null_mut(),
        str2:       ptr::null_mut(),
        str3:       ptr::null_mut(),
        int1:       0,
        int2:       rust_err.column.unwrap_or(0) as c_int,
        ctxt:       ptr::null_mut(),
        node:       ptr::null_mut(),
    };

    LAST_ERROR.with(|slot| {
        // Free any prior allocations before overwriting.
        slot.borrow_mut().free_strings();
        *slot.borrow_mut() = LastErrorSlot {
            err: c_err,
            message_raw: msg_raw,
            file_raw,
        };
    });
    LAST_ERROR_PRESENT.with(|p| *p.borrow_mut() = true);

    // Invoke handlers AFTER updating LAST_ERROR — a handler can call
    // xmlGetLastError() to inspect the just-recorded error.
    let handler_snapshot = HANDLERS.with(|h| {
        let h = h.borrow();
        (h.structured, h.structured_data, h.generic, h.generic_data)
    });

    if let (Some(fp), data) = (handler_snapshot.0, handler_snapshot.1) {
        // Grab a pointer to the slot's err for the callback.  The
        // callback runs synchronously inside this function, so the
        // slot's storage is guaranteed live for its duration.
        LAST_ERROR.with(|slot| {
            let err_ptr: *const xmlError = &slot.borrow().err;
            // SAFETY: fp is a caller-supplied extern "C" function
            // pointer; err_ptr is valid for the call's duration.
            unsafe { fp(data, err_ptr); }
        });
    }
    if let (Some(fp), data) = (handler_snapshot.2, handler_snapshot.3) {
        // Generic handler: format-string + va_args.  We use "%s\n" +
        // the slot's message pointer (re-derived from the now-installed
        // CString) as a single arg; vfprintf and libxml2's
        // xmlGenericErrorDefaultFunc both accept this.
        let fmt = c"%s\n".as_ptr();
        LAST_ERROR.with(|slot| {
            let msg_ptr = slot.borrow().err.message;
            // SAFETY: as above.
            unsafe { fp(data, fmt, msg_ptr); }
        });
    }
}

#[inline]
fn level_as_i32(l: ErrorLevel) -> c_int {
    l as i32
}

// ── exported extern "C" functions ──────────────────────────────────────────

/// libxml2 `xmlGetLastError`.  Returns a pointer to the thread's most
/// recent error, or NULL if no error has been recorded since the last
/// `xmlResetLastError()` (or thread start).
///
/// The returned pointer is valid until the next error is recorded on
/// the same thread (or until `xmlResetLastError` is called).  The
/// `message`/`file` strings inside the struct have the same lifetime.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub extern "C" fn xmlGetLastError() -> *const xmlError {
    let present = LAST_ERROR_PRESENT.with(|p| *p.borrow());
    if !present {
        return ptr::null();
    }
    LAST_ERROR.with(|slot| {
        // SAFETY: returning a raw pointer to thread-local storage is
        // sound as long as the caller doesn't use it across yields.
        // libxml2 documents the same contract.
        &slot.borrow().err as *const xmlError
    })
}

/// libxml2 `xmlCtxtGetLastError(ctxt)` — return the last error
/// recorded against `ctxt`, or NULL when none.
///
/// libxml2 stores `lastError` inline on the parser context.  Our
/// context is a 752-byte opaque buffer (see [`crate::parsectx`]) so
/// we don't carry a stable per-ctxt error slot today; instead we
/// route through the thread-local last-error machinery used by
/// [`xmlGetLastError`].  In practice consumers sequence
/// `xmlParseDocument` (or similar) followed by `xmlCtxtGetLastError`,
/// and the thread-local was just updated by the same parse, so the
/// answer matches.
///
/// When `ctxt` is NULL, follows libxml2 and returns NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlCtxtGetLastError(
    ctxt: *mut crate::parsectx::XmlParserCtxt,
) -> *const xmlError {
    if ctxt.is_null() { return ptr::null(); }
    xmlGetLastError()
}

/// libxml2 `xmlResetLastError`.  Clears the thread's most recent
/// error.  After this, `xmlGetLastError()` returns NULL until the
/// next error is recorded.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub extern "C" fn xmlResetLastError() {
    LAST_ERROR_PRESENT.with(|p| *p.borrow_mut() = false);
    LAST_ERROR.with(|slot| {
        *slot.borrow_mut() = LastErrorSlot::default();
    });
}

/// libxml2 `xmlResetError`.  Clears a caller-supplied `xmlError`
/// struct.  Used by callers who keep their own error buffers
/// (`xmlError local; ...; xmlResetError(&local);`).
///
/// # Safety
///
/// `err` must be a valid pointer to a writeable `xmlError`, OR NULL
/// (which is a no-op per libxml2's documentation).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlResetError(err: *mut xmlError) {
    if err.is_null() { return; }
    // SAFETY: caller asserts `err` is a valid writeable `xmlError`.
    unsafe { *err = zeroed_xml_error(); }
}

/// libxml2 `xmlSetStructuredErrorFunc`.  Register a callback to be
/// invoked when an error is recorded on the current thread.
///
/// Passing `None`/NULL `handler` unregisters the current callback.
/// `user_data` is opaque to us; we pass it back to the callback verbatim.
///
/// # Safety
///
/// `handler` must be a valid C function pointer for the lifetime of
/// any errors recorded after this call (or NULL).  `user_data` must
/// outlive that same lifetime.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetStructuredErrorFunc(
    user_data: *mut c_void,
    handler: Option<StructuredErrorFn>,
) {
    HANDLERS.with(|h| {
        let mut h = h.borrow_mut();
        h.structured = handler;
        h.structured_data = user_data;
    });
}

/// libxml2 `xmlSetGenericErrorFunc`.  Register a printf-style callback.
///
/// See [`StructuredErrorFn`] vs [`GenericErrorFn`] — most consumers
/// register a structured handler (preferred); the generic handler is
/// legacy.  Passing `None` unregisters.
///
/// # Safety
///
/// Same as [`xmlSetStructuredErrorFunc`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlSetGenericErrorFunc(
    user_data: *mut c_void,
    handler: Option<GenericErrorFn>,
) {
    HANDLERS.with(|h| {
        let mut h = h.borrow_mut();
        h.generic = handler;
        h.generic_data = user_data;
    });
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_core::error::{ErrorCode, ErrorDomain};

    fn reset() {
        xmlResetLastError();
        unsafe {
            xmlSetStructuredErrorFunc(ptr::null_mut(), None);
            xmlSetGenericErrorFunc(ptr::null_mut(), None);
        }
    }

    #[test]
    fn fresh_thread_has_no_last_error() {
        reset();
        assert!(xmlGetLastError().is_null());
    }

    #[test]
    fn record_then_get() {
        reset();
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "bad char")
            .with_code(ErrorCode::InvalidChar)
            .at("doc.xml", 7, 13, 312);
        record_last_error(&e);

        let p = xmlGetLastError();
        assert!(!p.is_null());
        // SAFETY: just verified non-null; valid until next record/reset.
        let last = unsafe { &*p };
        assert_eq!(last.domain, ErrorDomain::Parser as i32);
        assert_eq!(last.code,   ErrorCode::InvalidChar as i32);
        assert_eq!(last.code,   9);     // the libxml2 constant
        assert_eq!(last.level,  ErrorLevel::Fatal as i32);
        assert_eq!(last.line,   7);
        assert_eq!(last.int2,   13);    // column lives in int2
        let msg = unsafe { std::ffi::CStr::from_ptr(last.message) };
        assert_eq!(msg.to_str().unwrap(), "bad char");
        let file = unsafe { std::ffi::CStr::from_ptr(last.file) };
        assert_eq!(file.to_str().unwrap(), "doc.xml");
    }

    #[test]
    fn reset_clears_last() {
        reset();
        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Fatal, "x");
        record_last_error(&e);
        assert!(!xmlGetLastError().is_null());
        xmlResetLastError();
        assert!(xmlGetLastError().is_null());
    }

    #[test]
    fn reset_struct_zeros_caller_buffer() {
        let mut buf = xmlError {
            domain: 5, code: 9, message: ptr::null_mut(), level: 3,
            _pad_level: 0, file: ptr::null_mut(), line: 42, _pad_line: 0,
            str1: ptr::null_mut(), str2: ptr::null_mut(), str3: ptr::null_mut(),
            int1: 0, int2: 0, ctxt: ptr::null_mut(), node: ptr::null_mut(),
        };
        unsafe { xmlResetError(&mut buf); }
        assert_eq!(buf.domain, 0);
        assert_eq!(buf.code,   0);
        assert_eq!(buf.line,   0);
    }

    #[test]
    fn reset_struct_null_is_noop() {
        // Passing NULL to xmlResetError must not crash (libxml2's contract).
        unsafe { xmlResetError(ptr::null_mut()); }
    }

    /// Test the structured handler callback by capturing what it
    /// receives into a static cell.  The static cell is per-test
    /// (each test runs serially in cargo test, so contention is fine).
    #[test]
    fn structured_handler_invoked() {
        reset();

        use std::cell::Cell;
        thread_local! {
            static CAPTURED: Cell<Option<(c_int, c_int, c_int)>> = const { Cell::new(None) };
        }

        unsafe extern "C" fn handler(_data: *mut c_void, err: *const xmlError) {
            // SAFETY: caller (record_last_error) provides a valid err.
            let e = unsafe { &*err };
            CAPTURED.with(|c| c.set(Some((e.domain, e.code, e.line))));
        }

        unsafe {
            xmlSetStructuredErrorFunc(ptr::null_mut(), Some(handler));
        }

        let e = XmlError::new(ErrorDomain::Namespace, ErrorLevel::Error, "ns err")
            .with_code(ErrorCode::NsErrUndefinedNamespace)
            .at("x.xml", 3, 4, 56);
        record_last_error(&e);

        let got = CAPTURED.with(|c| c.get()).expect("handler was invoked");
        assert_eq!(got.0, ErrorDomain::Namespace as i32);
        assert_eq!(got.1, ErrorCode::NsErrUndefinedNamespace as i32);
        assert_eq!(got.1, 201);  // libxml2 numeric value (xmlerror.h XML_NS_ERR_UNDEFINED_NAMESPACE)
        assert_eq!(got.2, 3);
    }

    /// The generic handler fires.  The handler body ignores the
    /// variadic args (walking a `va_list` from Rust is nightly-only
    /// via `c_variadic`); a C test in `c-tests/` exercises the
    /// printf-style variadic path end-to-end.
    ///
    /// On stable we transmute a non-variadic handler to the variadic
    /// `GenericErrorFn` type — ABI-compatible on every real platform
    /// (the variadic args are simply unread).  Under Miri that
    /// transmute is rejected as strict UB, so under `cfg(miri)` we
    /// define a real variadic handler instead, which only nightly's
    /// `c_variadic` feature permits — and Miri implies nightly.
    #[test]
    fn generic_handler_invoked() {
        reset();

        use std::cell::Cell;
        thread_local! {
            static CALLED: Cell<bool> = const { Cell::new(false) };
        }

        // Under Miri: real variadic signature — no transmute, Miri-clean.
        #[cfg(miri)]
        unsafe extern "C" fn handler(
            _data: *mut c_void,
            _fmt:  *const c_char,
            mut _ap: ...,
        ) {
            CALLED.with(|c| c.set(true));
        }

        // On stable: define non-variadic and transmute below.
        #[cfg(not(miri))]
        unsafe extern "C" fn handler(
            _data: *mut c_void,
            _fmt:  *const c_char,
        ) {
            CALLED.with(|c| c.set(true));
        }

        #[cfg(miri)]
        let typed: GenericErrorFn = handler;

        // SAFETY: variadic and non-variadic extern "C" fn pointers
        // share calling convention when the receiver ignores its
        // variadic args.  Holds on every real ABI we target; the
        // Miri-rejected path is gated above.
        #[cfg(not(miri))]
        let typed: GenericErrorFn = unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(*mut c_void, *const c_char),
                GenericErrorFn,
            >(handler)
        };

        unsafe {
            xmlSetGenericErrorFunc(ptr::null_mut(), Some(typed));
        }

        let e = XmlError::new(ErrorDomain::Parser, ErrorLevel::Error, "boom");
        record_last_error(&e);

        assert!(CALLED.with(|c| c.get()), "generic handler should have fired");
    }
}
