//! libxml2 `xmlRegisterInputCallbacks` — custom resource I/O handlers.
//!
//! libxml2 keeps a global table of input handlers, each a quadruple of
//! C function pointers: `match(uri) -> bool`, `open(uri) -> ctx`,
//! `read(ctx, buf, len) -> n`, `close(ctx)`.  When the parser opens any
//! resource by URI (external DTD, external entity, XInclude href) it
//! walks the table most-recently-registered-first; the first handler
//! whose `match` claims the URI loads it through `open`/`read`/`close`.
//! This is how a C consumer teaches the parser to read from a zip, a
//! database, an in-memory map, or a custom protocol.
//!
//! Here the registered handlers are bridged into the parser through the
//! same [`EntityResolver`] abstraction the rest of the crate uses: an
//! [`InputCallbackResolver`] is installed as the parse's external
//! resolver when any handler is registered, so the callbacks fire
//! exactly when the parse's options permit external loading (governed by
//! `XML_PARSE_DTDLOAD` / `XML_PARSE_NOENT`).  This preserves the default
//! "external loading off" security posture — registering a handler
//! supplies the *mechanism*, the parse flags decide *whether* to load.
//!
//! # Safety / trust
//!
//! Registered function pointers are called during parsing.  The consumer
//! is responsible for their validity for as long as they stay
//! registered, exactly as libxml2 requires.  A handler's `open` context
//! is passed verbatim to its `read`/`close` and is closed exactly once.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;

use sup_xml_core::entity_resolver::{EntityResolver, ResolveError};

/// libxml2's `match` callback: does this handler claim `uri`?  Non-zero
/// = yes.
type MatchCb = unsafe extern "C" fn(*const c_char) -> c_int;
/// `open` callback: begin reading `uri`, returning an opaque context (or
/// NULL on failure).
type OpenCb = unsafe extern "C" fn(*const c_char) -> *mut c_void;
/// `read` callback: fill up to `len` bytes; returns bytes read, `0` at
/// EOF, or `< 0` on error.
type ReadCb = unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int;
/// `close` callback: release the context.
type CloseCb = unsafe extern "C" fn(*mut c_void) -> c_int;

/// One registered handler.  Function pointers are `Copy` + `Send` +
/// `Sync`, so a snapshot can be taken out of the registry and the
/// callbacks driven without holding the lock.
#[derive(Clone, Copy)]
struct InputCallbackSet {
    match_fn: MatchCb,
    open_fn:  OpenCb,
    read_fn:  ReadCb,
    close_fn: Option<CloseCb>,
}

/// Dispatchable handlers, in registration order (oldest first).
static REGISTRY: Mutex<Vec<InputCallbackSet>> = Mutex::new(Vec::new());

/// libxml2 reports `xmlRegisterInputCallbacks` slot indices relative to
/// its four built-in handlers (file, http, ftp, memory).  We don't model
/// those as callbacks — we load files directly — but we mirror the count
/// so a consumer that fingerprints the build (lxml does) sees the
/// expected base.
const DEFAULT_SLOTS: usize = 4;
static SLOTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(DEFAULT_SLOTS);

/// Upper bound on the bytes a single handler may stream for one
/// resource.  A backstop against a buggy or hostile `read` callback that
/// never signals EOF — not a tuning knob; real DTDs/entities are tiny.
const MAX_RESOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// Register a handler and return the new slot index (libxml2's contract),
/// or the current count for an all-NULL call.  A handler is dispatchable
/// only with at least `match` + `open` + `read`; `close` is optional.
///
/// # Safety
///
/// The four pointers must be NULL or real C callbacks of the libxml2
/// `xmlInput{Match,Open,Read,Close}Callback` signatures, valid for as
/// long as they remain registered.
pub(crate) unsafe fn register(
    match_fn: *mut c_void,
    open_fn:  *mut c_void,
    read_fn:  *mut c_void,
    close_fn: *mut c_void,
) -> c_int {
    use std::sync::atomic::Ordering;

    let any = !match_fn.is_null() || !open_fn.is_null()
           || !read_fn.is_null()  || !close_fn.is_null();
    if !any {
        return SLOTS.load(Ordering::Relaxed) as c_int;
    }
    if !match_fn.is_null() && !open_fn.is_null() && !read_fn.is_null() {
        // SAFETY: per this function's contract these are real callbacks
        // of the matching signatures.
        let set = unsafe {
            InputCallbackSet {
                match_fn: std::mem::transmute::<*mut c_void, MatchCb>(match_fn),
                open_fn:  std::mem::transmute::<*mut c_void, OpenCb>(open_fn),
                read_fn:  std::mem::transmute::<*mut c_void, ReadCb>(read_fn),
                close_fn: if close_fn.is_null() {
                    None
                } else {
                    Some(std::mem::transmute::<*mut c_void, CloseCb>(close_fn))
                },
            }
        };
        REGISTRY.lock().expect("input callback registry poisoned").push(set);
    }
    (SLOTS.fetch_add(1, Ordering::Relaxed) + 1) as c_int
}

/// Are any dispatchable handlers registered?  Gate for installing
/// [`InputCallbackResolver`] on a parse.
pub(crate) fn has_callbacks() -> bool {
    !REGISTRY.lock().expect("input callback registry poisoned").is_empty()
}

/// Drop all registered handlers and reset the slot count
/// (`xmlCleanupInputCallbacks`).
pub(crate) fn clear() {
    REGISTRY.lock().expect("input callback registry poisoned").clear();
    SLOTS.store(DEFAULT_SLOTS, std::sync::atomic::Ordering::Relaxed);
}

/// [`EntityResolver`] that loads a URI through the first registered
/// handler whose `match` claims it, falling back to a local-file read
/// when none does (mirroring libxml2's built-in file handler).
#[derive(Debug)]
pub(crate) struct InputCallbackResolver;

impl EntityResolver for InputCallbackResolver {
    fn resolve(
        &self,
        _public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let uri = CString::new(system_id)
            .map_err(|_| ResolveError::Io("system id has interior NUL".into()))?;

        // Snapshot most-recently-registered-first and drop the lock
        // before calling any C callback (which may itself parse and
        // re-enter the registry).
        let sets: Vec<InputCallbackSet> = {
            let reg = REGISTRY.lock().expect("input callback registry poisoned");
            reg.iter().rev().copied().collect()
        };

        for set in sets {
            // SAFETY: registered callbacks are the consumer's, valid per
            // `register`'s contract.
            if unsafe { (set.match_fn)(uri.as_ptr()) } == 0 {
                continue;
            }
            return unsafe { drive_handler(set, uri.as_ptr(), system_id) };
        }

        // No handler claimed the URI — default to local-file loading,
        // the way libxml2's built-in file handler would.  The parser
        // only reaches this resolver when its options already permit
        // external loading.
        let path = system_id
            .strip_prefix("file://")
            .map(|r| r.strip_prefix("localhost").unwrap_or(r))
            .or_else(|| system_id.strip_prefix("file:"))
            .unwrap_or(system_id);
        std::fs::read(path)
            .map_err(|e| ResolveError::Io(format!("input callbacks: {system_id:?}: {e}")))
    }
}

/// Drive one matched handler to EOF: `open` → repeated `read` → `close`.
/// The context from `open` is closed exactly once, on every exit path.
///
/// # Safety
/// `set`'s callbacks must be valid and `uri` a live NUL-terminated URI.
unsafe fn drive_handler(
    set: InputCallbackSet,
    uri: *const c_char,
    system_id: &str,
) -> Result<Vec<u8>, ResolveError> {
    let ctx = unsafe { (set.open_fn)(uri) };
    if ctx.is_null() {
        return Err(ResolveError::Io(format!(
            "input callback open() returned NULL for {system_id:?}"
        )));
    }

    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let result = loop {
        if out.len() as u64 > MAX_RESOURCE_BYTES {
            break Err(ResolveError::Io(format!(
                "input callback resource exceeds {MAX_RESOURCE_BYTES} bytes: {system_id:?}"
            )));
        }
        let n = unsafe {
            (set.read_fn)(ctx, chunk.as_mut_ptr() as *mut c_char, chunk.len() as c_int)
        };
        if n < 0 {
            break Err(ResolveError::Io(format!(
                "input callback read() failed for {system_id:?}"
            )));
        }
        if n == 0 {
            break Ok(out);
        }
        let n = (n as usize).min(chunk.len());
        out.extend_from_slice(&chunk[..n]);
    };

    // SAFETY: `ctx` came from this handler's `open` and is closed once.
    if let Some(close_fn) = set.close_fn {
        unsafe { close_fn(ctx); }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicU32, Ordering};

    // The registry is process-global, so registry-touching tests run
    // serially and clear() around themselves.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    static MATCH_CALLS: AtomicU32 = AtomicU32::new(0);
    static OPEN_CALLS:  AtomicU32 = AtomicU32::new(0);
    static READ_CALLS:  AtomicU32 = AtomicU32::new(0);
    static CLOSE_CALLS: AtomicU32 = AtomicU32::new(0);

    const SERVED: &[u8] = b"<!ELEMENT r EMPTY>";

    fn reset_counts() {
        for c in [&MATCH_CALLS, &OPEN_CALLS, &READ_CALLS, &CLOSE_CALLS] {
            c.store(0, Ordering::SeqCst);
        }
    }

    unsafe extern "C" fn t_match(uri: *const c_char) -> c_int {
        MATCH_CALLS.fetch_add(1, Ordering::SeqCst);
        let s = unsafe { CStr::from_ptr(uri) }.to_str().unwrap_or("");
        if s.contains("cbtest") { 1 } else { 0 }
    }

    unsafe extern "C" fn t_open(_uri: *const c_char) -> *mut c_void {
        OPEN_CALLS.fetch_add(1, Ordering::SeqCst);
        // Context = a heap-boxed read offset.
        Box::into_raw(Box::new(0usize)) as *mut c_void
    }

    unsafe extern "C" fn t_read(ctx: *mut c_void, buf: *mut c_char, len: c_int) -> c_int {
        READ_CALLS.fetch_add(1, Ordering::SeqCst);
        let off = unsafe { &mut *(ctx as *mut usize) };
        let remaining = &SERVED[(*off).min(SERVED.len())..];
        if remaining.is_empty() {
            return 0;
        }
        let n = remaining.len().min(len as usize);
        unsafe { std::ptr::copy_nonoverlapping(remaining.as_ptr(), buf as *mut u8, n); }
        *off += n;
        n as c_int
    }

    unsafe extern "C" fn t_close(ctx: *mut c_void) -> c_int {
        CLOSE_CALLS.fetch_add(1, Ordering::SeqCst);
        drop(unsafe { Box::from_raw(ctx as *mut usize) });
        0
    }

    fn register_test_handler() -> c_int {
        // SAFETY: the t_* fns are valid callbacks for this process.
        unsafe {
            register(
                t_match as *mut c_void,
                t_open as *mut c_void,
                t_read as *mut c_void,
                t_close as *mut c_void,
            )
        }
    }

    #[test]
    fn resolver_drives_all_four_callbacks() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        reset_counts();

        let idx = register_test_handler();
        assert_eq!(idx, 5, "first registration sits past the 4 built-ins");
        assert!(has_callbacks());

        let bytes = InputCallbackResolver
            .resolve(None, "cbtest://thing", None)
            .expect("resolver should load via the handler");
        assert_eq!(bytes, SERVED);

        assert!(MATCH_CALLS.load(Ordering::SeqCst) >= 1, "match() not called");
        assert_eq!(OPEN_CALLS.load(Ordering::SeqCst), 1, "open() called once");
        assert!(READ_CALLS.load(Ordering::SeqCst) >= 1, "read() not called");
        assert_eq!(CLOSE_CALLS.load(Ordering::SeqCst), 1, "close() called once");

        clear();
    }

    #[test]
    fn non_matching_uri_falls_back_to_file() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        reset_counts();
        register_test_handler();

        // matchFn rejects this URI → fall back to a file read, which
        // fails for a path that does not exist.
        let r = InputCallbackResolver.resolve(None, "/no/such/sup-xml/file", None);
        assert!(r.is_err(), "non-matching URI should not be served by the handler");
        assert!(MATCH_CALLS.load(Ordering::SeqCst) >= 1, "match() should still be consulted");
        assert_eq!(OPEN_CALLS.load(Ordering::SeqCst), 0, "open() must not run on a non-match");

        clear();
    }

    #[test]
    fn all_null_registration_is_a_noop() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        // SAFETY: all-NULL is the documented no-op form.
        let n = unsafe {
            register(std::ptr::null_mut(), std::ptr::null_mut(),
                     std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(n, DEFAULT_SLOTS as c_int);
        assert!(!has_callbacks());
        clear();
    }

    #[test]
    fn callbacks_fire_during_a_real_parse() {
        let _g = TEST_LOCK.lock().unwrap();
        clear();
        reset_counts();
        register_test_handler();

        // External DTD by SYSTEM id; XML_PARSE_DTDLOAD (1<<2) makes the
        // parser load it — through our InputCallbackResolver.
        const XML_PARSE_DTDLOAD: c_int = 1 << 2;
        let src = br#"<!DOCTYPE r SYSTEM "cbtest://dtd"><r/>"#;
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                std::ptr::null(),
                std::ptr::null(),
                XML_PARSE_DTDLOAD,
            )
        };
        assert!(!doc.is_null(), "parse should succeed");
        assert!(MATCH_CALLS.load(Ordering::SeqCst) >= 1, "match() should run during the parse");
        assert!(OPEN_CALLS.load(Ordering::SeqCst) >= 1, "open() should run during the parse");
        assert!(CLOSE_CALLS.load(Ordering::SeqCst) >= 1, "close() should run during the parse");
        unsafe { crate::parse::xmlFreeDoc(doc); }

        clear();
    }
}
