// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Reads served from the page cache on the async worker itself, without a
//! handoff to the blocking pool (Linux only). `openat2` with `RESOLVE_CACHED`
//! and `preadv2` with `RWF_NOWAIT` report `EAGAIN` instead of waiting for
//! the device, and the caller then does the whole operation on the pool, as
//! FDS-STORAGE.EXECUTOR-01 has it. A kernel that lacks the call or the flag
//! reports an error this module treats the same way, so it only ever takes
//! the pool path.

use std::fs::File;
use std::io::{self, IoSliceMut};
use std::path::Path;

use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
use rustix::io::{preadv2, Errno, ReadWriteFlags};

/// Whether `errno` means that the call would have waited, or that this
/// kernel cannot tell: `EAGAIN`, or the `ENOSYS`, `EINVAL` or `EOPNOTSUPP`
/// of a kernel without `openat2`, `RESOLVE_CACHED` (Linux 5.12) or
/// `RWF_NOWAIT` (Linux 4.14).
fn would_wait(errno: Errno) -> bool {
    errno == Errno::AGAIN
        || errno == Errno::NOSYS
        || errno == Errno::INVAL
        || errno == Errno::OPNOTSUPP
}

/// Open `path` read-only, if every component of it is in the dentry cache;
/// an error of kind `WouldBlock` if not.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    match openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::CACHED,
    ) {
        Ok(fd) => Ok(File::from(fd)),
        Err(errno) if would_wait(errno) => Err(io::ErrorKind::WouldBlock.into()),
        Err(errno) => Err(errno.into()),
    }
}

/// `FileExt::read_exact_at` from the page cache only: an error of kind
/// `WouldBlock` as soon as a read would wait for the device.
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        match preadv2(
            file,
            &mut [IoSliceMut::new(buf)],
            offset,
            ReadWriteFlags::NOWAIT,
        ) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(errno) if errno == Errno::INTR => {}
            Err(errno) if would_wait(errno) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(())
}
