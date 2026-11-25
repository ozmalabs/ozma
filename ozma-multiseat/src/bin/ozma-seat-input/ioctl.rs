// SPDX-License-Identifier: AGPL-3.0-only
//! The helper's only `unsafe` module.
//!
//! `EVIOCGRAB` and uinput creation are issued through the safe `evdev` crate.
//! The one remaining platform operation is switching a grabbed evdev fd to
//! non-blocking so the relay thread can observe its stop flag. The two
//! `libc::fcntl` calls below are the complete unsafe surface of this binary.

use std::io;
use std::os::unix::io::RawFd;

/// Set `O_NONBLOCK` on an already-open fd.
///
/// # Safety
///
/// The caller must own a live file descriptor for the duration of the call.
/// This function only queries/modifies file-status flags; it does not close or
/// duplicate the descriptor.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fd` is a live descriptor owned by the caller; F_GETFL/F_SETFL
    // only read or update its file-status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same descriptor and a flag value obtained from F_GETFL.
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
