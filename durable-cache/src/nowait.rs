// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Reads served from the page cache on the async worker itself, without a
//! handoff to the blocking pool (Linux only), when the configuration asks
//! for them (`FsCacheConfig::with_page_cache_reads`, on by default on
//! Linux). `openat2` with `RESOLVE_CACHED` and `preadv2` with `RWF_NOWAIT`
//! report `EAGAIN` instead of waiting for the device, and the caller then
//! leaves the operation to the blocking pool (FDS-STORAGE.PAGECACHE-01). A
//! read the page cache misses still starts the device's work: the kernel
//! submits its readahead on the calling thread before it reports `EAGAIN`,
//! so the worker pays for the submission, and the pool's read finds the
//! data on its way.
//!
//! `FsCache::new` checks once that the kernel and the cache's file system
//! support both calls ([`check_support`]). Should one still report itself
//! missing, it fails with an error of kind `Unsupported`, which the caller
//! reports: a page-cache read never turns into a pool read unnoticed.
//!
//! With `FsCacheConfig::with_noatime`, on by default, records are opened
//! with `O_NOATIME`, here and on the pool (`crate::platform::Opener`). A
//! read that updates a file's access time dirties its inode, and on ext4
//! that starts a journal transaction, which can wait: even a read that
//! `RWF_NOWAIT` keeps from waiting for the device updates it. With the
//! default `relatime`, it happens on the first read of every record after
//! it was written, as its writes leave its access time behind its
//! modification time. The kernel refuses `O_NOATIME` on a file of another
//! owner, and the open then fails.

use std::fs::File;
use std::io::{self, IoSliceMut};
use std::os::fd::AsRawFd;
use std::path::Path;

use pingora_error::{ErrorType, OrErr};
use rustix::fs::{openat2, Mode, ResolveFlags, CWD};
use rustix::io::{preadv2, Errno, ReadWriteFlags};

use crate::platform::Opener;
use crate::writerid::{self, WriterLease};
use crate::FsCacheConfig;

/// Whether `errno`, from `openat2`, says that the call or its
/// `RESOLVE_CACHED` flag is missing: `ENOSYS` before Linux 5.6, `EINVAL` for
/// the flag before 5.12, and `EOPNOTSUPP` taken the same way.
fn openat2_lacking(errno: Errno) -> bool {
    errno == Errno::NOSYS || errno == Errno::INVAL || errno == Errno::OPNOTSUPP
}

/// Whether `errno`, from `preadv2`, says that the call or its `RWF_NOWAIT`
/// flag is missing: `ENOSYS`, or `EOPNOTSUPP` for a flag the kernel or the
/// file system lacks. An `EINVAL` from `preadv2` means a bad argument, an
/// error like any other.
fn preadv2_lacking(errno: Errno) -> bool {
    errno == Errno::NOSYS || errno == Errno::OPNOTSUPP
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

/// Check that the kernel and the cache's file system support the calls of
/// page-cache reads: `openat2` with `RESOLVE_CACHED` (Linux 5.12), tried on
/// `root` with `opener`, and `preadv2` with `RWF_NOWAIT`, tried on `file`, a
/// regular file in the cache. What it returns makes those calls, opening
/// with `opener`; an error of kind `Unsupported` names the call that is
/// missing.
pub(crate) fn check_support(
    opener: Opener,
    root: &Path,
    file: &Path,
) -> io::Result<PageCacheReads> {
    match open_cached(opener, root) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(e),
    }
    let file = File::open(file)?;
    match read_cached(&file, &mut [0u8; 1], 0) {
        Err(e) if e.kind() != io::ErrorKind::UnexpectedEof => Err(e),
        _ => Ok(PageCacheReads { opener }),
    }
}

/// That the kernel and the cache's file system support the calls of
/// page-cache reads, as [`check_support`] found: nothing else can make one.
/// Each page-cache read starts with one of its two calls, a lookup's open or
/// a body read's fill, so none can be made unless the check passed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageCacheReads {
    opener: Opener,
}

impl PageCacheReads {
    /// The page-cache reads `config` asks for, opening with `opener`, or
    /// `None` with them switched off. It first checks that the kernel and
    /// the cache's file system support their calls ([`check_support`]): on
    /// the root, and on `lease`'s file, the one file the cache is sure to
    /// hold. Fails with an `InternalError` if not.
    pub(crate) fn new(
        config: &FsCacheConfig,
        lease: &WriterLease,
        opener: Opener,
    ) -> pingora_error::Result<Option<Self>> {
        if !config.page_cache_reads() {
            return Ok(None);
        }
        let root = config.root();
        let lease_file = writerid::lease_path(root, &lease.id());
        let checked = check_support(opener, root, &lease_file);
        checked.map(Some).or_err_with(ErrorType::InternalError, || {
            format!(
                "checking the page-cache reads that FsCacheConfig::with_page_cache_reads asks \
                 for under {}; switch them off where the kernel or the file system lacks them, \
                 as tmpfs and overlayfs do",
                root.display()
            )
        })
    }

    /// Open `path` if every component of it is in the dentry cache
    /// (`openat2` with `RESOLVE_CACHED`); an error of kind `WouldBlock` if
    /// not.
    pub(crate) fn open(&self, path: &Path) -> io::Result<File> {
        open_cached(self.opener, path)
    }

    /// [`fill`] from the page cache only: an error of kind `WouldBlock` at
    /// the first byte that would wait for the device, with what was read
    /// kept in `buf`.
    pub(crate) fn fill(
        &self,
        file: &File,
        buf: &mut Vec<u8>,
        end: usize,
        base: u64,
    ) -> io::Result<()> {
        fill(file, buf, end, base, Wait::Never)
    }
}

/// Open `path` with `opener`'s flags if every component of it is in the
/// dentry cache (`openat2` with `RESOLVE_CACHED`); an error of kind
/// `WouldBlock` if not. Only [`PageCacheReads`] and [`check_support`] call
/// it.
fn open_cached(opener: Opener, path: &Path) -> io::Result<File> {
    match openat2(
        CWD,
        path,
        opener.flags(),
        Mode::empty(),
        ResolveFlags::CACHED,
    ) {
        Ok(fd) => Ok(File::from(fd)),
        Err(errno) if errno == Errno::AGAIN => Err(would_block()),
        Err(errno) if openat2_lacking(errno) => {
            Err(not_supported("openat2 with RESOLVE_CACHED", errno))
        }
        Err(errno) => Err(errno.into()),
    }
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
/// would wait for the device. `WouldBlock` if none could be read. The file
/// ending first is an error of kind `UnexpectedEof`, whatever was read.
pub(crate) fn read_at_most(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    match read_cached(file, buf, offset)? {
        0 if !buf.is_empty() => Err(would_block()),
        read => Ok(read),
    }
}

/// Whether a read may wait for the device.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
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
fn fill(file: &File, buf: &mut Vec<u8>, end: usize, base: u64, wait: Wait) -> io::Result<()> {
    buf.reserve(end.saturating_sub(buf.len()));
    while buf.len() < end {
        let start = buf.len();
        let Ok(offset) = libc::off64_t::try_from(base + start as u64) else {
            // Past the end of any file: Linux caps file sizes below 2^63.
            return Err(io::ErrorKind::UnexpectedEof.into());
        };
        let spare = &mut buf.spare_capacity_mut()[..end - start];
        // SAFETY: `spare` is memory that `buf` owns and nothing else refers
        // to. The kernel writes at most `spare.len()` bytes into it, and
        // returns how many it wrote.
        let read = unsafe {
            match wait {
                Wait::Allowed => libc::pread64(
                    file.as_raw_fd(),
                    spare.as_mut_ptr().cast(),
                    spare.len(),
                    offset,
                ),
                Wait::Never => pread_nowait(
                    file.as_raw_fd(),
                    spare.as_mut_ptr().cast(),
                    spare.len(),
                    offset,
                ),
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
                if wait == Wait::Never && preadv2_lacking(errno) {
                    return Err(not_supported("preadv2 with RWF_NOWAIT", errno));
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

/// [`fill`] on the blocking pool, waiting for the device when it must.
pub(crate) fn fill_blocking(
    file: &File,
    buf: &mut Vec<u8>,
    end: usize,
    base: u64,
) -> io::Result<()> {
    fill(file, buf, end, base, Wait::Allowed)
}

/// `pread64` from the page cache only: `preadv2` with `RWF_NOWAIT`, which
/// fails with `EAGAIN` instead of waiting for the device.
///
/// # Safety
///
/// As for `pread64`: `buf` is valid for writes of `count` bytes.
unsafe fn pread_nowait(
    fd: libc::c_int,
    buf: *mut libc::c_void,
    count: libc::size_t,
    offset: libc::off64_t,
) -> libc::ssize_t {
    let iov = libc::iovec {
        iov_base: buf,
        iov_len: count,
    };
    // The libc crate declares `preadv64v2` for glibc only, whose `preadv2`
    // takes an `off_t`, 32 bits on its 32-bit targets; musl's `preadv2`
    // takes an `off_t` of 64 bits everywhere.
    #[cfg(target_env = "gnu")]
    {
        libc::preadv64v2(fd, &iov, 1, offset, libc::RWF_NOWAIT)
    }
    #[cfg(not(target_env = "gnu"))]
    {
        libc::preadv2(fd, &iov, 1, offset, libc::RWF_NOWAIT)
    }
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
            Err(errno) if preadv2_lacking(errno) => {
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

    /// The opener of a cache at `root`, with `O_NOATIME` if `noatime`.
    fn opener(root: &Path, noatime: bool) -> Opener {
        Opener::new(&FsCacheConfig::new(root, None).with_noatime(noatime)).expect("the opener")
    }

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

    /// Where [`uncache_from`] drops a file's pages from, in the tests that
    /// keep a cached prefix: a whole number of pages.
    const CACHED: u64 = 64 * 1024;

    /// Drop `file`'s pages from `offset` on out of the page cache: written,
    /// synchronised and so clean, they go, and a read of them with
    /// `RWF_NOWAIT` usually fails with `EAGAIN`. Not always: a read that
    /// misses starts the device's work, and a device that answers within
    /// the call, as a virtual machine's disk can, lets the read go on; so
    /// does a page cache that keeps the file in units larger than a page.
    /// The tests that use it hold to what is true either way, and drop the
    /// pages again before each read.
    fn uncache_from(file: &File, offset: u64) {
        file.sync_data().expect("the file is synchronised");
        rustix::fs::fadvise(file, offset, None, rustix::fs::Advice::DontNeed)
            .expect("the pages are dropped");
    }

    /// Whether the file system holding `file`, just written, has
    /// non-blocking reads; where it has not, a test that needs them says so
    /// and checks nothing.
    fn non_blocking_reads(file: &File) -> bool {
        match read_cached(file, &mut [0u8; 1], 0) {
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                eprintln!("no non-blocking reads here: nothing to check");
                false
            }
            _ => true,
        }
    }

    /// On the worker, `fill` reads what the page cache holds, at least the
    /// pages before [`CACHED`], and stops with `WouldBlock` where it would
    /// wait (see [`uncache_from`] for where it goes further), keeping what
    /// it read; the pool's `fill` carries on from there.
    #[test]
    fn fill_on_the_worker_keeps_what_it_read_for_the_pool() {
        let dir = crate::integration_tests::support::tempdir();
        let (file, bytes) = file_of(dir.path(), 200_000);
        if !non_blocking_reads(&file) {
            return;
        }
        uncache_from(&file, CACHED);
        let mut buf = Vec::new();
        if let Err(e) = fill(&file, &mut buf, bytes.len(), 0, Wait::Never) {
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
        }
        assert!(
            buf.len() >= CACHED as usize,
            "the cached bytes at least; read {}",
            buf.len()
        );
        assert_eq!(buf, &bytes[..buf.len()], "the file's bytes, from its start");
        fill(&file, &mut buf, bytes.len(), 0, Wait::Allowed).expect("the pool reads the rest");
        assert_eq!(buf, bytes);
    }

    /// The reads of a record on the worker read what the page cache holds:
    /// `read_at_most` at least the cached prefix; with nothing cached,
    /// `read_at_most` and `read_exact_at` fail with `WouldBlock`, unless the
    /// device answers within the call (see [`uncache_from`]). What they
    /// read is the file's bytes.
    #[test]
    fn record_reads_on_the_worker_read_what_the_page_cache_holds() {
        let dir = crate::integration_tests::support::tempdir();
        let (file, bytes) = file_of(dir.path(), 200_000);
        if !non_blocking_reads(&file) {
            return;
        }
        let mut buf = vec![0u8; 100_000];
        uncache_from(&file, CACHED);
        let read = read_at_most(&file, &mut buf, 0).expect("the start is cached");
        assert!(
            read >= CACHED as usize,
            "the cached prefix at least; read {read}"
        );
        assert_eq!(&buf[..read], &bytes[..read]);

        uncache_from(&file, 0);
        match read_at_most(&file, &mut buf, 0) {
            Ok(read) => assert_eq!(&buf[..read], &bytes[..read], "answered within the call"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}"),
        }

        uncache_from(&file, 0);
        match read_exact_at(&file, &mut buf, 0) {
            Ok(()) => assert_eq!(buf, &bytes[..buf.len()], "answered within the call"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}"),
        }
    }

    /// The reads of a record on the worker return the file's bytes at the
    /// offset asked for, not from its start.
    #[test]
    fn record_reads_on_the_worker_read_at_their_offset() {
        let dir = crate::integration_tests::support::tempdir();
        let (file, bytes) = file_of(dir.path(), 100_000);
        if !non_blocking_reads(&file) {
            return;
        }
        let mut buf = vec![0u8; 1000];
        read_exact_at(&file, &mut buf, 12_345).expect("the bytes are cached");
        assert_eq!(buf, &bytes[12_345..13_345]);
        let mut buf = vec![0u8; 1000];
        let read = read_at_most(&file, &mut buf, 54_321).expect("the bytes are cached");
        assert_eq!(read, buf.len());
        assert_eq!(buf, &bytes[54_321..55_321]);
    }

    /// On a file system without non-blocking reads, each read on the worker
    /// fails with an error of kind `Unsupported` naming the call, whichever
    /// kind the standard library gives `EOPNOTSUPP` (`Uncategorized` in Rust
    /// 1.85). `/dev/shm` is a tmpfs, which lacks them as of Linux 6.12; where
    /// it has them, or there is no `/dev/shm`, nothing can tell.
    #[test]
    fn reads_the_file_system_lacks_are_unsupported() {
        let Ok(dir) = tempfile::tempdir_in("/dev/shm") else {
            eprintln!("no /dev/shm: nothing to check");
            return;
        };
        let (file, _) = file_of(dir.path(), 8192);
        let probe = preadv2(
            &file,
            &mut [IoSliceMut::new(&mut [0u8; 1])],
            0,
            ReadWriteFlags::NOWAIT,
        );
        if !matches!(probe, Err(errno) if errno == Errno::OPNOTSUPP) {
            eprintln!("this tmpfs has non-blocking reads: nothing to check");
            return;
        }
        let unsupported = |call: &str, e: io::Error| {
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{call}: {e}");
            assert!(
                e.to_string().contains("preadv2 with RWF_NOWAIT"),
                "{call}: {e}"
            );
        };
        let mut buf = Vec::new();
        unsupported(
            "fill",
            fill(&file, &mut buf, 100, 0, Wait::Never).expect_err("fill fails"),
        );
        unsupported(
            "read_exact_at",
            read_exact_at(&file, &mut [0u8; 100], 0).expect_err("read_exact_at fails"),
        );
        unsupported(
            "read_at_most",
            read_at_most(&file, &mut [0u8; 100], 0).expect_err("read_at_most fails"),
        );
    }

    /// The check at construction passes on a file none of whose pages are
    /// cached: a read that would wait is no missing call.
    #[test]
    fn check_support_passes_on_a_file_not_cached() {
        let dir = crate::integration_tests::support::tempdir();
        let (file, _) = file_of(dir.path(), 8192);
        if !non_blocking_reads(&file) {
            return;
        }
        uncache_from(&file, 0);
        check_support(
            opener(dir.path(), false),
            dir.path(),
            &dir.path().join("body"),
        )
        .expect("the calls are supported");
    }

    /// `fill` reads the range asked for, and carries on from the bytes a
    /// buffer already holds, waiting or not.
    #[test]
    fn fill_reads_the_range_and_carries_on_from_what_it_holds() {
        let dir = crate::integration_tests::support::tempdir();
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
        let dir = crate::integration_tests::support::tempdir();
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
        let dir = crate::integration_tests::support::tempdir();
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
        opener(dir.path(), true)
            .open_blocking(&path)
            .and_then(|f| f.read_exact_at(&mut buf, 0))
            .expect("the record reads on the pool");
        // The open just made puts the path in the dentry cache, and the
        // write put the file in the page cache.
        let file =
            open_cached(opener(dir.path(), true), &path).expect("the path is in the dentry cache");
        match read_exact_at(&file, &mut buf, 4096) {
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                eprintln!("no non-blocking reads here: only the pool's read is checked");
            }
            read => read.expect("the page is in the page cache"),
        }
        assert_eq!(accessed(&path), before, "no read updated the access time");
    }
}
