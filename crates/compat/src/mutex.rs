//! libxml2 mutex API.
//!
//! libxml2 exposes its own opaque `xmlMutex*` handle plus
//! `xmlNewMutex` / `xmlMutexLock` / `xmlMutexUnlock` / `xmlFreeMutex`
//! for callers that want a libxml2-allocated mutex.  libxslt uses
//! these internally to serialize access to its global stylesheet
//! cache.
//!
//! Implementation: an `AtomicBool` spinlock.  We can't stash a
//! `std::sync::MutexGuard` outside the lock call because the guard
//! is `!Send`, and `xmlMutex` is fundamentally a stateful handle
//! (Lock returns nothing, Unlock takes the same handle).  Spinlocks
//! are fine here: libxslt's mutex usage is short-lived (microsecond-
//! scale critical sections protecting global init/cache lookups),
//! and lxml's GIL serializes most Python-side calls, so contention
//! in practice is near zero.
//!
//! `xmlLockLibrary` / `xmlUnlockLibrary` operate on a process-wide
//! singleton mutex used by libxml2 to serialize global state init —
//! we implement it with the same spinlock backing one static slot.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

#[repr(C)]
struct XmlMutex {
    locked: AtomicBool,
}

/// `xmlNewMutex()` — allocate a fresh mutex on the heap.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNewMutex() -> *mut c_void {
    let m = Box::new(XmlMutex { locked: AtomicBool::new(false) });
    Box::into_raw(m) as *mut c_void
}

/// `xmlFreeMutex(mutex)` — release a mutex allocated by
/// [`xmlNewMutex`].  NULL is a silent no-op (matches libxml2).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFreeMutex(mutex: *mut c_void) {
    if mutex.is_null() {
        return;
    }
    // SAFETY: caller asserts `mutex` came from xmlNewMutex.
    let _ = unsafe { Box::from_raw(mutex as *mut XmlMutex) };
}

/// Spin until we observe the lock free, then take it.  Yields the
/// thread on contention so we don't hammer the CPU.
fn acquire(slot: &AtomicBool) {
    loop {
        if slot.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }
        // Spin-then-yield: cheap retry for the common no-contention
        // case, then hand off to the scheduler if the other holder
        // is taking a while.
        for _ in 0..40 {
            std::hint::spin_loop();
            if !slot.load(Ordering::Relaxed) {
                break;
            }
        }
        std::thread::yield_now();
    }
}

/// `xmlMutexLock(mutex)` — acquire the lock.  Blocks (spins-then-
/// yields) if another thread holds it.  NULL is a silent no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMutexLock(mutex: *mut c_void) {
    if mutex.is_null() {
        return;
    }
    // SAFETY: caller asserts `mutex` is a live xmlNewMutex result.
    let m = unsafe { &*(mutex as *const XmlMutex) };
    acquire(&m.locked);
}

/// `xmlMutexUnlock(mutex)` — release the lock.  NULL is a silent
/// no-op.  Calling on an unlocked mutex is a silent no-op (matches
/// libxml2's tolerance).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlMutexUnlock(mutex: *mut c_void) {
    if mutex.is_null() {
        return;
    }
    let m = unsafe { &*(mutex as *const XmlMutex) };
    m.locked.store(false, Ordering::Release);
}

// ── library-wide singleton lock ───────────────────────────────────────────

static LIBRARY_LOCK: AtomicBool = AtomicBool::new(false);

/// `xmlLockLibrary()` — acquire libxml2's library-wide singleton
/// mutex.  Used by consumers that want to serialize their access to
/// global state libxml2 manages (parser-internals dictionaries,
/// catalog tables, etc.).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLockLibrary() {
    acquire(&LIBRARY_LOCK);
}

/// `xmlUnlockLibrary()` — release the library-wide singleton mutex.
/// Silent no-op when not locked.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlUnlockLibrary() {
    LIBRARY_LOCK.store(false, Ordering::Release);
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn new_then_free_does_not_crash() {
        let m = unsafe { xmlNewMutex() };
        assert!(!m.is_null());
        unsafe { xmlFreeMutex(m); }
    }

    #[test]
    fn null_args_are_no_ops() {
        unsafe {
            xmlMutexLock(ptr::null_mut());
            xmlMutexUnlock(ptr::null_mut());
            xmlFreeMutex(ptr::null_mut());
        }
    }

    #[test]
    fn lock_then_unlock_works() {
        let m = unsafe { xmlNewMutex() };
        unsafe { xmlMutexLock(m); }
        unsafe { xmlMutexUnlock(m); }
        unsafe { xmlMutexLock(m); }
        unsafe { xmlMutexUnlock(m); }
        unsafe { xmlFreeMutex(m); }
    }

    #[test]
    fn lock_excludes_other_threads() {
        use std::sync::atomic::{AtomicU64, Ordering as AOrdering};
        use std::thread;
        use std::time::Duration;

        let m = unsafe { xmlNewMutex() };
        let m_addr = m as usize;
        let touched = std::sync::Arc::new(AtomicU64::new(0));
        unsafe { xmlMutexLock(m); }

        let t_touched = touched.clone();
        let t = thread::spawn(move || {
            unsafe { xmlMutexLock(m_addr as *mut c_void); }
            t_touched.store(1, AOrdering::SeqCst);
            unsafe { xmlMutexUnlock(m_addr as *mut c_void); }
        });

        thread::sleep(Duration::from_millis(50));
        assert_eq!(touched.load(AOrdering::SeqCst), 0,
            "child thread should still be blocked on the lock");
        unsafe { xmlMutexUnlock(m); }
        t.join().unwrap();
        assert_eq!(touched.load(AOrdering::SeqCst), 1);
        unsafe { xmlFreeMutex(m); }
    }

    #[test]
    fn library_lock_is_idempotent_after_unlock() {
        unsafe { xmlLockLibrary(); }
        unsafe { xmlUnlockLibrary(); }
        unsafe { xmlLockLibrary(); }
        unsafe { xmlUnlockLibrary(); }
    }

    #[test]
    fn unlock_without_lock_is_safe() {
        let m = unsafe { xmlNewMutex() };
        unsafe { xmlMutexUnlock(m); }
        unsafe { xmlFreeMutex(m); }
    }
}
