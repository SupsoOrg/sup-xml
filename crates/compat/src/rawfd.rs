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
