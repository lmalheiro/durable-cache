// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Reads served from the page cache on the async worker itself, without a
//! handoff to the blocking pool (Linux only). `openat2` with `RESOLVE_CACHED`
//! and `preadv2` with `RWF_NOWAIT` report `EAGAIN` instead of waiting for
//! the device, and the caller then does the whole operation on the pool, as
//! FDS-STORAGE.EXECUTOR-01 has it. A kernel that lacks the call or the flag,
//! or a file system that does not support the flag, reports an error that
//! this module remembers: from then on, the operation goes to the pool
//! without trying again. The memory is the process's, as a process normally
//! keeps one cache, on one file system.
//!
//! Records are opened with `O_NOATIME`, here and on the pool
//! ([`open_blocking`]). A read that updates a file's access time dirties its
//! inode, and on ext4 that starts a journal transaction, which can wait:
//! even a read that `RWF_NOWAIT` keeps from waiting for the device updates
//! it. With the default `relatime`, it happens on the first read of every
//! record after it was written, as its writes leave its access time behind
//! its modification time. Nothing in this crate reads access times. The
//! kernel refuses `O_NOATIME` on a file of another owner; once refused, the
//! opens go without it.

use std::fs::{File, OpenOptions};
use std::io::{self, IoSliceMut};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
use rustix::io::{preadv2, Errno, ReadWriteFlags};

/// Set once the kernel has refused `O_NOATIME`: the cache's files belong to
/// another user than this process's.
static NOATIME_REFUSED: AtomicBool = AtomicBool::new(false);

/// Set once `openat2` or its `RESOLVE_CACHED` (Linux 5.12) turned out
/// missing.
static NO_CACHED_OPEN: AtomicBool = AtomicBool::new(false);

/// Set once `preadv2` or its `RWF_NOWAIT` (Linux 4.14) turned out missing,
/// or unsupported by the cache's file system.
static NO_NOWAIT_READ: AtomicBool = AtomicBool::new(false);

/// The flags a record is opened to read with.
fn read_flags() -> OFlags {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC;
    if NOATIME_REFUSED.load(Ordering::Relaxed) {
        flags
    } else {
        flags | OFlags::NOATIME
    }
}

/// Whether `errno` says that the call or its flag is missing (`ENOSYS`,
/// `EINVAL`), or unsupported by the file system (`EOPNOTSUPP`).
fn unsupported(errno: Errno) -> bool {
    errno == Errno::NOSYS || errno == Errno::INVAL || errno == Errno::OPNOTSUPP
}

/// The error of an operation that would wait for the device, or that this
/// system cannot tell would not.
fn would_block() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}

/// Open `path` read-only, if every component of it is in the dentry cache;
/// an error of kind `WouldBlock` if not.
pub(crate) fn open(path: &Path) -> io::Result<File> {
    if NO_CACHED_OPEN.load(Ordering::Relaxed) {
        return Err(would_block());
    }
    loop {
        let flags = read_flags();
        match openat2(CWD, path, flags, Mode::empty(), ResolveFlags::CACHED) {
            Ok(fd) => return Ok(File::from(fd)),
            Err(errno) if errno == Errno::PERM && flags.contains(OFlags::NOATIME) => {
                NOATIME_REFUSED.store(true, Ordering::Relaxed);
            }
            Err(errno) if errno == Errno::AGAIN => return Err(would_block()),
            Err(errno) if unsupported(errno) => {
                NO_CACHED_OPEN.store(true, Ordering::Relaxed);
                return Err(would_block());
            }
            Err(errno) => return Err(errno.into()),
        }
    }
}

/// [`open`] for the blocking pool: the same flags, and every component of
/// `path` looked up however long that takes.
pub(crate) fn open_blocking(path: &Path) -> io::Result<File> {
    if !NOATIME_REFUSED.load(Ordering::Relaxed) {
        let noatime = OFlags::NOATIME.bits() as i32;
        match OpenOptions::new()
            .read(true)
            .custom_flags(noatime)
            .open(path)
        {
            Err(e) if e.raw_os_error() == Some(Errno::PERM.raw_os_error()) => {
                NOATIME_REFUSED.store(true, Ordering::Relaxed);
            }
            opened => return opened,
        }
    }
    File::open(path)
}

/// `FileExt::read_exact_at` from the page cache only: an error of kind
/// `WouldBlock` as soon as a read would wait for the device.
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    if read_cached(file, buf, offset)? == buf.len() {
        Ok(())
    } else {
        Err(would_block())
    }
}

/// The start of `buf` filled from the page cache, as far as the bytes at
/// `offset` are there: how many were read, short of `buf.len()` if the rest
/// would wait for the device. `WouldBlock` if none could be read.
pub(crate) fn read_at_most(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    match read_cached(file, buf, offset)? {
        0 if !buf.is_empty() => Err(would_block()),
        read => Ok(read),
    }
}

/// Fill `buf` from the page cache, up to the first byte that would wait for
/// the device: how many bytes it read. The file ending first is an error
/// of kind `UnexpectedEof`.
fn read_cached(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() && !NO_NOWAIT_READ.load(Ordering::Relaxed) {
        match preadv2(
            file,
            &mut [IoSliceMut::new(&mut buf[read..])],
            offset + read as u64,
            ReadWriteFlags::NOWAIT,
        ) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => read += n,
            Err(errno) if errno == Errno::INTR => {}
            Err(errno) if errno == Errno::AGAIN => break,
            Err(errno) if unsupported(errno) => NO_NOWAIT_READ.store(true, Ordering::Relaxed),
            Err(errno) => return Err(errno.into()),
        }
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;
    use std::os::unix::fs::FileExt;
    use std::time::{Duration, SystemTime};

    use super::*;

    /// A file written now, its access time an hour behind its modification
    /// time, as a store leaves it: `relatime` updates it on the next read.
    fn written_an_hour_after_access(path: &Path) {
        std::fs::write(path, vec![7u8; 8192]).expect("the file is written");
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(path)
            .and_then(|f| f.set_times(FileTimes::new().set_accessed(an_hour_ago)))
            .expect("the access time is set");
    }

    fn accessed(path: &Path) -> SystemTime {
        std::fs::metadata(path)
            .and_then(|m| m.accessed())
            .expect("the access time reads")
    }

    /// Reading a record, on the worker or on the pool, leaves its access
    /// time as it was, where a plain open and read would update it.
    #[test]
    fn reading_a_record_leaves_its_access_time() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut buf = [0u8; 4096];

        // On a file system mounted without access times, nothing can tell.
        let control = dir.path().join("control");
        written_an_hour_after_access(&control);
        let before = accessed(&control);
        File::open(&control)
            .and_then(|f| f.read_exact_at(&mut buf, 0))
            .expect("the control file reads");
        if accessed(&control) == before {
            eprintln!("this file system does not record access times: nothing to check");
            return;
        }

        let path = dir.path().join("record");
        written_an_hour_after_access(&path);
        let before = accessed(&path);
        open_blocking(&path)
            .and_then(|f| f.read_exact_at(&mut buf, 0))
            .expect("the record reads on the pool");
        // The open just made puts the path in the dentry cache.
        if let Ok(file) = open(&path) {
            let _ = read_exact_at(&file, &mut buf, 4096);
        }
        assert_eq!(accessed(&path), before, "no read updated the access time");
    }
}
