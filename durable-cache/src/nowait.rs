// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Reads served from the page cache on the async worker itself, without a
//! handoff to the blocking pool (Linux only), when the configuration asks
//! for them (`FsCacheConfig::with_page_cache_reads`, on by default on
//! Linux). `openat2` with `RESOLVE_CACHED` and `preadv2` with `RWF_NOWAIT`
//! report `EAGAIN` instead of waiting for the device, and the caller then
//! does the whole operation on the pool, as FDS-STORAGE.EXECUTOR-01 has it.
//!
//! `FsCache::new` checks once that the kernel and the cache's file system
//! support both calls ([`check_support`]). Should one still report itself
//! missing, it fails with an error of kind `Unsupported`, which the caller
//! reports: a page-cache read never turns into a pool read unnoticed.
//!
//! With `FsCacheConfig::with_noatime`, on by default, records are opened
//! with `O_NOATIME`, here and on the pool ([`open_blocking`]). A read that
//! updates a file's access time dirties its inode, and on ext4 that starts a
//! journal transaction, which can wait: even a read that `RWF_NOWAIT` keeps
//! from waiting for the device updates it. With the default `relatime`, it
//! happens on the first read of every record after it was written, as its
//! writes leave its access time behind its modification time. The kernel
//! refuses `O_NOATIME` on a file of another owner, and the open then fails.

use std::fs::{File, OpenOptions};
use std::io::{self, IoSliceMut};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
use rustix::io::{preadv2, Errno, ReadWriteFlags};

/// The flags a record is opened to read with: with `O_NOATIME` if
/// `noatime`.
fn read_flags(noatime: bool) -> OFlags {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC;
    if noatime {
        flags | OFlags::NOATIME
    } else {
        flags
    }
}

/// Whether `errno` says that the call or its flag is missing (`ENOSYS`,
/// `EINVAL`), or unsupported by the file system (`EOPNOTSUPP`).
fn unsupported(errno: Errno) -> bool {
    errno == Errno::NOSYS || errno == Errno::INVAL || errno == Errno::OPNOTSUPP
}

/// The error of an operation that would wait for the device.
fn would_block() -> io::Error {
    io::ErrorKind::WouldBlock.into()
}

/// The error of `call`, which the kernel or the file system does not
/// support, as `errno` says.
fn not_supported(call: &str, errno: Errno) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{call}: {}", io::Error::from(errno)),
    )
}

/// Check that the kernel and the cache's file system support the calls
/// this module makes: `openat2` with `RESOLVE_CACHED` (Linux 5.12), tried on
/// `root`, and `preadv2` with `RWF_NOWAIT`, tried on `file`, a regular file
/// in the cache. An error of kind `Unsupported` names the call that is
/// missing.
pub(crate) fn check_support(root: &Path, file: &Path) -> io::Result<()> {
    match open(root, false) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    let file = File::open(file)?;
    match read_cached(&file, &mut [0u8; 1], 0) {
        Err(e) if e.kind() != io::ErrorKind::UnexpectedEof => Err(e),
        _ => Ok(()),
    }
}

/// Open `path` read-only, without updating its access time if `noatime`,
/// if every component of it is in the dentry cache; an error of kind
/// `WouldBlock` if not.
pub(crate) fn open(path: &Path, noatime: bool) -> io::Result<File> {
    match openat2(
        CWD,
        path,
        read_flags(noatime),
        Mode::empty(),
        ResolveFlags::CACHED,
    ) {
        Ok(fd) => Ok(File::from(fd)),
        Err(errno) if errno == Errno::AGAIN => Err(would_block()),
        Err(errno) if unsupported(errno) => {
            Err(not_supported("openat2 with RESOLVE_CACHED", errno))
        }
        Err(errno) => Err(errno.into()),
    }
}

/// [`open`] for the blocking pool: the same flags, and every component of
/// `path` looked up however long that takes.
pub(crate) fn open_blocking(path: &Path, noatime: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    if noatime {
        options.custom_flags(OFlags::NOATIME.bits() as i32);
    }
    options.open(path)
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

/// Whether a read may wait for the device.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wait {
    /// It may: a read on the blocking pool.
    Allowed,
    /// It may not: a read on the worker, from the page cache only.
    Never,
}

/// Read the file into `buf`, past the bytes it holds, until it holds `end`:
/// the byte at index `i` of `buf` is the file's byte at `base + i`. The
/// kernel writes straight into `buf`'s spare capacity, so the bytes need no
/// zeroing first, as a read into a slice would. With [`Wait::Never`] it
/// reads from the page cache only, and stops with an error of kind
/// `WouldBlock` at the first byte that would wait for the device. Whatever
/// it read stays in `buf`, for the pool to carry on from. The file ending
/// first is an error of kind `UnexpectedEof`.
pub(crate) fn fill(
    file: &File,
    buf: &mut Vec<u8>,
    end: usize,
    base: u64,
    wait: Wait,
) -> io::Result<()> {
    buf.reserve(end.saturating_sub(buf.len()));
    while buf.len() < end {
        let start = buf.len();
        let Ok(offset) = libc::off_t::try_from(base + start as u64) else {
            // Beyond what this platform's `off_t` can name: the standard
            // library's read, into zeroed bytes, on the pool.
            if wait == Wait::Never {
                return Err(would_block());
            }
            buf.resize(end, 0);
            return file.read_exact_at(&mut buf[start..], base + start as u64);
        };
        let spare = &mut buf.spare_capacity_mut()[..end - start];
        // SAFETY: `spare` is memory that `buf` owns and nothing else refers
        // to. The kernel writes at most `spare.len()` bytes into it, and
        // returns how many it wrote.
        let read = unsafe {
            match wait {
                Wait::Allowed => libc::pread(
                    file.as_raw_fd(),
                    spare.as_mut_ptr().cast(),
                    spare.len(),
                    offset,
                ),
                Wait::Never => {
                    let iov = libc::iovec {
                        iov_base: spare.as_mut_ptr().cast(),
                        iov_len: spare.len(),
                    };
                    libc::preadv2(file.as_raw_fd(), &iov, 1, offset, libc::RWF_NOWAIT)
                }
            }
        };
        match read {
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            read if read > 0 => {
                // SAFETY: the kernel has initialised these `read` bytes,
                // which lie within the capacity reserved above.
                unsafe { buf.set_len(start + read as usize) };
            }
            _ => {
                let error = io::Error::last_os_error();
                let errno = Errno::from_raw_os_error(error.raw_os_error().unwrap_or(0));
                if errno == Errno::INTR {
                    continue;
                }
                if wait == Wait::Never && errno == Errno::AGAIN {
                    return Err(would_block());
                }
                if wait == Wait::Never && unsupported(errno) {
                    return Err(not_supported("preadv2 with RWF_NOWAIT", errno));
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

/// Fill `buf` from the page cache, up to the first byte that would wait for
/// the device: how many bytes it read. The file ending first is an error
/// of kind `UnexpectedEof`.
fn read_cached(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
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
            Err(errno) if unsupported(errno) => {
                return Err(not_supported("preadv2 with RWF_NOWAIT", errno))
            }
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

    /// A file of `len` bytes that differ from their neighbours.
    fn file_of(dir: &Path, len: usize) -> (File, Vec<u8>) {
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let path = dir.join("body");
        std::fs::write(&path, &bytes).expect("the file is written");
        (File::open(&path).expect("the file opens"), bytes)
    }

    /// `fill` reads the range asked for, and carries on from the bytes a
    /// buffer already holds, waiting or not.
    #[test]
    fn fill_reads_the_range_and_carries_on_from_what_it_holds() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (file, bytes) = file_of(dir.path(), 300_000);

        let mut buf = Vec::new();
        fill(&file, &mut buf, 100_000, 50_000, Wait::Allowed).expect("the range reads");
        assert_eq!(buf, &bytes[50_000..150_000]);

        let mut buf = bytes[1000..1500].to_vec();
        fill(&file, &mut buf, 200_000, 1000, Wait::Allowed).expect("the rest reads");
        assert_eq!(buf, &bytes[1000..201_000]);

        // The file was just written, so its pages are cached. A file system
        // without non-blocking reads, such as tmpfs, reports them
        // unsupported, and nothing is read.
        let mut buf = vec![bytes[7]];
        match fill(&file, &mut buf, 90_000, 7, Wait::Never) {
            Ok(()) => assert_eq!(buf, &bytes[7..90_007]),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => assert_eq!(buf, [bytes[7]]),
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
                assert_eq!(buf, &bytes[7..7 + buf.len()], "what it read is kept");
            }
        }
    }

    /// The file ending before the range is an error of kind
    /// `UnexpectedEof`, and the bytes read until then stay in the buffer.
    #[test]
    fn fill_past_the_end_of_the_file_is_unexpected_eof() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (file, bytes) = file_of(dir.path(), 10_000);
        let mut buf = Vec::new();
        let e = fill(&file, &mut buf, 5000, 8000, Wait::Allowed).expect_err("the file ends first");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(buf, &bytes[8000..]);
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
        open_blocking(&path, true)
            .and_then(|f| f.read_exact_at(&mut buf, 0))
            .expect("the record reads on the pool");
        // The open just made puts the path in the dentry cache.
        if let Ok(file) = open(&path, true) {
            let _ = read_exact_at(&file, &mut buf, 4096);
        }
        assert_eq!(accessed(&path), before, "no read updated the access time");
    }
}
