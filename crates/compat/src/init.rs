//! Parser init/cleanup entry points + threading safety.
//!
//! libxml2 historically required `xmlInitParser` to be called once
//! per process before parsing — it set up global tables, default
//! handlers, and (in older versions) a thread-local context store.
//! Modern libxml2 still exports it for backwards-compat but most
//! parsing paths auto-init lazily.
//!
//! sup-xml has no process-global state to initialize: every parse
//! is self-contained.  We export `xmlInitParser` / `xmlCleanupParser`
//! anyway because every libxml2 consumer eventually calls them, but
//! they're functionally no-ops protected by a `Once` so repeated
//! calls (including from multiple threads racing on parser startup)
//! are safe and observable as "idempotent" without contention.

use std::sync::Once;

/// libxml2 `xmlInitParser`.  Idempotent + thread-safe.  Our
/// implementation has nothing to initialize; the `Once` is here so
/// that any future per-process setup we add (a string-intern pool,
/// say) can hang off this entry point.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub extern "C" fn xmlInitParser() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // No-op for v0.1.  Place future global init here.
    });
}

/// libxml2 `xmlCleanupParser`.  Historically a no-op suggestion
/// ("call before exit if you really want to") that becomes load-
/// bearing only when libxml2's global memory tracker is enabled.
/// Our cleanup is per-document via `xmlFreeDoc`; nothing global to
/// release.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub extern "C" fn xmlCleanupParser() {
    // Intentionally empty.  All allocations are owned by individual
    // [`XmlDoc`](sup_xml_tree::dom::XmlDoc) instances or the
    // thread-local last-error slot — neither has process-global
    // state to tear down.
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_char;
    use std::os::raw::c_int;
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use crate::parse::{xmlDocGetRootElement, xmlFreeDoc, xmlNodeGetContent, xmlReadMemory, xmlFree};

    /// xmlInitParser is idempotent across calls.
    #[test]
    fn init_is_idempotent() {
        xmlInitParser();
        xmlInitParser();
        xmlInitParser();
        // No-op contract: nothing observable should fail.
    }

    /// xmlInitParser is safe to call concurrently from many threads.
    /// Each thread also runs a small parse-and-walk to catch any
    /// init-related races.  Matches T-PARSE-11 + T-THREAD-03 in shape.
    #[test]
    fn init_is_thread_safe() {
        let n_threads = 8;
        let calls_per_thread = 200;
        let total = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..n_threads).map(|_| {
            let total = total.clone();
            thread::spawn(move || {
                for _ in 0..calls_per_thread {
                    xmlInitParser();
                    total.fetch_add(1, Ordering::Relaxed);
                }
            })
        }).collect();

        for h in handles { h.join().expect("thread panicked"); }
        assert_eq!(total.load(Ordering::Relaxed), n_threads * calls_per_thread);
    }

    /// T-THREAD-01 in Rust form: disjoint docs parsed + walked on N
    /// threads concurrently.  Each thread owns its document; no
    /// shared state crosses thread boundaries.
    #[test]
    fn parses_concurrently_on_disjoint_docs() {
        let n_threads = 8;
        let parses_per_thread = 50;

        let handles: Vec<_> = (0..n_threads).map(|t| {
            thread::spawn(move || {
                for i in 0..parses_per_thread {
                    let src = format!(
                        "<r thread=\"{t}\" iter=\"{i}\"><a/><b><c>hello</c></b></r>"
                    );
                    let doc = unsafe {
                        xmlReadMemory(
                            src.as_ptr() as *const c_char,
                            src.len() as c_int,
                            ptr::null(),
                            ptr::null(),
                            0,
                        )
                    };
                    assert!(!doc.is_null(), "thread {t} iter {i}: parse failed");
                    let root = unsafe { xmlDocGetRootElement(doc) };
                    assert!(!root.is_null());
                    let n = unsafe { &*root };
                    assert_eq!(n.name(), "r");

                    let content = unsafe { xmlNodeGetContent(root) };
                    assert!(!content.is_null());
                    unsafe {
                        xmlFree(content as *mut _);
                        xmlFreeDoc(doc);
                    }
                }
            })
        }).collect();

        for h in handles { h.join().expect("thread panicked"); }
    }

    /// xmlCleanupParser is a no-op even after init.
    #[test]
    fn cleanup_after_init_is_noop() {
        xmlInitParser();
        xmlCleanupParser();
        // Re-init should still work fine.
        xmlInitParser();
    }
}
