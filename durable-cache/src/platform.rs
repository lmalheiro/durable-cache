// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The backend's calls that differ between Linux and the other Unix systems,
//! for the code that calls them the same way on every platform. Each is one
//! declaration with a `#[cfg]` block per platform. What only Linux does at
//! all, reading from the page cache on the async worker, is in
//! `crate::nowait` and `crate::lookup::page_cache`, and the latter has a
//! stand-in elsewhere that leaves every read to the blocking pool.

use std::fs::File;
use std::io;
use std::path::Path;

use pingora_error::Result;

use crate::FsCacheConfig;

/// How records and temporaries are opened to read. On Linux, with
/// `O_NOATIME` if the configuration asks (`FsCacheConfig::with_noatime`),
/// which [`Opener::new`] has checked the kernel allows; the worker's open
/// (`crate::nowait`) and the pool's take their flags from the same value, so
/// they cannot drift apart. Elsewhere, as `File::open` opens them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Opener {
    #[cfg(target_os = "linux")]
    flags: rustix::fs::OFlags,
}

impl Opener {
    /// The opener `config` asks for. On Linux, with
    /// `FsCacheConfig::with_noatime`, it first checks that the kernel lets
    /// this process open the cache's files without updating their access
    /// time: the kernel allows `O_NOATIME` only to a file's owner, and the
    /// owner of the root stands for the owner of the files under it. Fails
    /// with a `FileOpenError` if not.
    pub(crate) fn new(config: &FsCacheConfig) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use pingora_error::{ErrorType, OrErr};
            use rustix::fs::OFlags;

            let flags = OFlags::RDONLY | OFlags::CLOEXEC;
            let opener = Self {
                flags: if config.noatime() {
                    flags | OFlags::NOATIME
                } else {
                    flags
                },
            };
            if config.noatime() {
                let root = config.root();
                opener.open_blocking(root).map(drop).or_err_with(
                    ErrorType::FileOpenError,
                    || {
                        format!(
                            "opening {} without updating its access time, as \
                             FsCacheConfig::with_noatime asks; switch it off where this \
                             process does not own the cache's files",
                            root.display()
                        )
                    },
                )?;
            }
            Ok(opener)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = config;
            Ok(Self {})
        }
    }

    /// Open `path` to read, looking up every component of it however long
    /// that takes: the open of the blocking pool.
    pub(crate) fn open_blocking(&self, path: &Path) -> io::Result<File> {
        #[cfg(target_os = "linux")]
        {
            use rustix::io::Errno;

            loop {
                match rustix::fs::open(path, self.flags, rustix::fs::Mode::empty()) {
                    Ok(fd) => return Ok(File::from(fd)),
                    Err(errno) if errno == Errno::INTR => {}
                    Err(errno) => return Err(errno.into()),
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            File::open(path)
        }
    }

    /// The flags of every open, for the worker's (`crate::nowait`).
    #[cfg(target_os = "linux")]
    pub(crate) fn flags(&self) -> rustix::fs::OFlags {
        self.flags
    }
}

/// Read `file` into `buf`, past the bytes it holds, until it holds `end`:
/// the byte at index `i` of `buf` is the file's byte at `base + i`. It waits
/// for the device when it must, so it runs on the blocking pool. On Linux,
/// the kernel writes into `buf`'s spare capacity, not zeroed first
/// (`crate::nowait`); elsewhere, into zeroed bytes. The file ending first is
/// an error of kind `UnexpectedEof`.
pub(crate) fn fill_blocking(
    file: &File,
    buf: &mut Vec<u8>,
    end: usize,
    base: u64,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        crate::nowait::fill_blocking(file, buf, end, base)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let start = buf.len();
        buf.resize(end, 0);
        std::os::unix::fs::FileExt::read_exact_at(file, &mut buf[start..], base + start as u64)
    }
}

/// Flush the file system that holds `root`: with `syncfs` on Linux, and
/// elsewhere with `sync`, which flushes every file system, and on some
/// systems returns before the writes complete.
pub(crate) fn sync_file_system(root: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::syncfs(File::open(root)?).map_err(io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        // SAFETY: `sync` takes no arguments and touches no memory of this
        // process.
        unsafe { libc::sync() };
        Ok(())
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    /// The kernel refuses `O_NOATIME` on a file of another owner, and the
    /// check at construction says so, naming the option: `/` belongs to
    /// root.
    #[test]
    fn the_noatime_check_fails_on_a_directory_of_another_owner() {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("root opens any file without updating its access time: nothing to check");
            return;
        }
        let e = Opener::new(&FsCacheConfig::new("/", None).with_noatime(true))
            .expect_err("/ belongs to root");
        assert!(e.to_string().contains("with_noatime"), "{e}");
    }
}
