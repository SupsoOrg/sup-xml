//! Bridge a caller-supplied C `int` file descriptor to a Rust [`File`]
//! without taking ownership of it.
//!
//! libxml2's fd-based entry points (`xmlReadFd`, `xmlReaderForFd`,
//! `xmlSaveToFd`, …) accept an `int` descriptor and, by contract, leave
//! ownership with the caller — the shim must never `close()` it.  On Unix
//! that `int` is a native file descriptor [`File`] adopts directly.  On
//! Windows it is a C runtime (MSVCRT/UCRT) descriptor, which `std` will
//! not turn into a [`File`]; it must first be resolved to the underlying
//! Win32 `HANDLE` via `_get_osfhandle`.
//!
//! The returned [`File`] is wrapped in [`ManuallyDrop`] so the descriptor
//! is never closed when the wrapper goes out of scope.

use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::raw::c_int;

/// Borrow `fd` as a [`File`] that will not close the descriptor on drop,
/// or `None` if `fd` does not name an open descriptor.
#[cfg(unix)]
pub(crate) fn borrow_fd(fd: c_int) -> Option<ManuallyDrop<File>> {
    use std::os::unix::io::FromRawFd;
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is non-negative and the caller asserts it is open;
    // ManuallyDrop suppresses the close on drop so ownership stays with
    // the caller.
    Some(ManuallyDrop::new(unsafe { File::from_raw_fd(fd) }))
}

/// Borrow `fd` as a [`File`] that will not close the descriptor on drop,
/// or `None` if `fd` does not name an open descriptor.
#[cfg(windows)]
pub(crate) fn borrow_fd(fd: c_int) -> Option<ManuallyDrop<File>> {
    use std::os::windows::io::{FromRawHandle, RawHandle};
    // A Windows `File` adopts a Win32 HANDLE, not the CRT `int` descriptor
    // libxml2 callers pass; translate one to the other through the CRT.
    unsafe extern "C" {
        fn _get_osfhandle(fd: c_int) -> isize;
    }
    // SAFETY: `_get_osfhandle` is a pure CRT table lookup with no
    // preconditions; it reports a bad descriptor through its return value.
    let handle = unsafe { _get_osfhandle(fd) };
    // -1 (INVALID_HANDLE_VALUE) and -2 (no stream associated) mean the
    // descriptor has no usable handle.
    if handle == -1 || handle == -2 {
        return None;
    }
    // SAFETY: `handle` is a live Win32 file handle owned by the caller;
    // ManuallyDrop suppresses the CloseHandle on drop so ownership stays
    // with the caller.
    Some(ManuallyDrop::new(unsafe {
        File::from_raw_handle(handle as RawHandle)
    }))
}

/// Test-only helpers that open and drive a real C `int` descriptor through
/// the CRT, so the fd-based entry points can be exercised on every platform.
///
/// They go through `libc::open`/`read`/`write`/`lseek`/`close` (mapped to
/// `_open`/`_read`/… on Windows) rather than `File::as_raw_fd`, which is
/// Unix-only.  The descriptor is fully owned by the caller — no `File`/Win32
/// `HANDLE` aliasing — so `close` is unambiguous on both platforms.  The
/// per-OS signature differences (`size_t` vs `c_uint`, the Windows
/// `O_BINARY` flag) are contained here.
#[cfg(test)]
pub(crate) mod testfd {
    use std::ffi::CString;
    use std::os::raw::c_int;
    use std::path::Path;

    fn cpath(path: &Path) -> CString {
        CString::new(path.to_str().expect("utf-8 temp path")).unwrap()
    }

    /// Open `path` read-only (binary) and return its descriptor.
    pub(crate) fn open_ro(path: &Path) -> c_int {
        let c = cpath(path);
        #[cfg(unix)]
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
        #[cfg(windows)]
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_BINARY) };
        assert!(fd >= 0, "open_ro({}) failed", path.display());
        fd
    }

    /// Create/truncate `path` for writing (binary) and return its descriptor.
    pub(crate) fn open_w(path: &Path) -> c_int {
        let c = cpath(path);
        #[cfg(unix)]
        let fd = unsafe {
            libc::open(c.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o644)
        };
        #[cfg(windows)]
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_BINARY,
                libc::S_IWRITE,
            )
        };
        assert!(fd >= 0, "open_w({}) failed", path.display());
        fd
    }

    /// Read up to `buf.len()` bytes from `fd` at its current offset.
    pub(crate) fn read(fd: c_int, buf: &mut [u8]) -> isize {
        let ptr = buf.as_mut_ptr() as *mut std::os::raw::c_void;
        #[cfg(unix)]
        let n = unsafe { libc::read(fd, ptr, buf.len()) };
        #[cfg(windows)]
        let n = unsafe { libc::read(fd, ptr, buf.len() as u32) as isize };
        n
    }

    /// Write `bytes` to `fd` at its current offset; returns bytes written.
    pub(crate) fn write(fd: c_int, bytes: &[u8]) -> isize {
        let ptr = bytes.as_ptr() as *const std::os::raw::c_void;
        #[cfg(unix)]
        let n = unsafe { libc::write(fd, ptr, bytes.len()) };
        #[cfg(windows)]
        let n = unsafe { libc::write(fd, ptr, bytes.len() as u32) as isize };
        n
    }

    /// Seek `fd` back to the start.
    pub(crate) fn rewind(fd: c_int) {
        let r = unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        assert!(r >= 0, "lseek failed");
    }

    /// Close `fd`.
    pub(crate) fn close(fd: c_int) {
        unsafe { libc::close(fd) };
    }
}
