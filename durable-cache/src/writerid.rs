// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Writer identity: which backend instance wrote a temporary, and whether
//! that instance may still be running (ADR-STORAGE.WRITERID-01).
//!
//! **The problem.** A store writes its record to a temporary file, and
//! renames it to the object's name once the record is complete
//! (FDS-STORAGE.PUBLISH-01). A process that crashes leaves its temporaries
//! behind, and the scan at the next start removes them
//! (FDS-STORAGE.RECLAIM-01). But more than one process can use a cache
//! directory at the same time: during a hot upgrade, the old process and the
//! new one both store. So the scan may remove a temporary only once the
//! process that wrote it has stopped, and it has to tell that from the
//! temporary's name. A process ID is not enough, because the system reuses
//! process IDs: a stopped writer's ID may already belong to another process.
//! In a container, the cache process often runs as PID 1, and gets that same
//! ID every time it starts.
//!
//! **Leases.** When a backend instance, that is an `FsCache`, is
//! constructed, it creates a *lease*: an empty file
//! `writers/<pid>-<nonce>.lock` under the cache root. `<nonce>` is a random
//! 64-bit number, which makes the name unique; `<pid>` only tells an
//! operator which process the lease belongs to. Each temporary is named
//! after the lease of the instance that writes it:
//! `<hash>.<pid>-<nonce>.<counter>.tmp`, where `<hash>` is the name of the
//! object the temporary will become, and `<counter>` numbers the instance's
//! stores.
//!
//! **Why a lease is locked.** A crashed process cannot remove its lease, so
//! a lease that exists does not prove that its writer is running. A lock
//! does. The instance holds an exclusive `flock` lock on its lease for as
//! long as it lives. The kernel releases the lock when the file is closed,
//! and it closes all of a process's files when the process exits or crashes.
//! So, for each temporary, the scan reads the writer's identity from the
//! temporary's name, and tries to lock that writer's lease:
//!
//! - if the lease is locked, the writer is running: the scan leaves the
//!   temporary alone;
//! - if the scan gets the lock, or the lease is missing, the writer has
//!   stopped: the scan removes the temporary, and later the lease.
//!
//! A scan that found a new lease before it was locked would take it for a
//! stopped writer's lease, and remove it. So a new lease is locked before it
//! gets its final name: it is created as `<pid>-<nonce>.lock.new`, locked,
//! and only then renamed. A scan may still remove the staging file before it
//! is locked, as it removes those a crash leaves behind; but then the lock or
//! the rename fails, and the instance tries again with a new name.
//!
//! The locks work only if all the processes that use a cache directory run
//! on the same host, with the directory on a local file system, so that each
//! sees the others' locks.
//!
//! **Strict names.** A name is parsed back only if it is spelled exactly as
//! this module writes it. The same numbers can be spelled in other ways,
//! such as with leading zeros, but a file named like that was not written by
//! this backend, and the scan must not remove it.

use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;
use std::{fmt, io};

/// The directory under the cache root that holds the leases.
pub const LEASE_DIR: &str = "writers";
/// Extension of a per-writer temporary (FDS-STORAGE.PUBLISH-01):
/// `<hash>.<pid>-<nonce>.<counter>.tmp`.
pub const TEMP_SUFFIX: &str = "tmp";
/// A lease's extension.
const LEASE_SUFFIX: &str = "lock";
/// The extra extension of a lease being created, before it is renamed into
/// place.
const STAGING_SUFFIX: &str = "new";
/// How many fresh names [`WriterLease::acquire`] tries before giving up.
const LEASE_ATTEMPTS: usize = 8;
/// The nonce's width in hex digits, two per byte: names carry every digit,
/// leading zeros included.
const NONCE_DIGITS: usize = 2 * size_of::<u64>();

/// The identity of one backend instance: its process ID and a random nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WriterId {
    pub pid: u32,
    pub nonce: u64,
}

impl fmt::Display for WriterId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:0NONCE_DIGITS$x}", self.pid, self.nonce)
    }
}

impl WriterId {
    /// A fresh identity for this process. `RandomState` is seeded from the
    /// operating system's randomness, and the time and a call counter make
    /// each value differ; exclusive creation of the lease catches the
    /// unlikely collision anyway.
    fn fresh() -> Self {
        static CALLS: AtomicU64 = AtomicU64::new(0);
        let nonce =
            RandomState::new().hash_one((SystemTime::now(), CALLS.fetch_add(1, Ordering::Relaxed)));
        WriterId {
            pid: std::process::id(),
            nonce,
        }
    }

    /// The identity `s` names, if it is spelled exactly as [`Display`]
    /// writes one. Checking the round trip rejects every other spelling of
    /// the same numbers, which the parsers alone accept: a sign, leading
    /// zeros, an uppercase or a short nonce.
    ///
    /// [`Display`]: fmt::Display
    fn parse(s: &str) -> Option<Self> {
        let (pid, nonce) = s.split_once('-')?;
        let id = WriterId {
            pid: pid.parse().ok()?,
            nonce: u64::from_str_radix(nonce, 16).ok()?,
        };
        (id.to_string() == s).then_some(id)
    }
}

/// A backend instance's lease, locked for as long as it is held.
#[derive(Debug)]
pub struct WriterLease {
    id: WriterId,
    /// Keeping the descriptor open is what keeps the lock.
    _file: File,
    /// The instance's store counter, which numbers its temporaries. The
    /// identity already keeps instances' names apart, so the counter only
    /// has to keep this instance's stores apart.
    counter: AtomicU64,
}

impl WriterLease {
    /// Create and lock a fresh lease under `root`. It is locked under a
    /// staging name and only then renamed into place, so no scan ever finds
    /// it unlocked under its final name.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(root.join(LEASE_DIR))?;
        let mut last = io::Error::other("no lease acquired");
        for _ in 0..LEASE_ATTEMPTS {
            let id = WriterId::fresh();
            let lease = lease_path(root, &id);
            let staging = staging_path(&lease);
            let file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staging)
            {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    last = e;
                    continue;
                }
                Err(e) => return Err(e),
            };
            // A scan that found the staging file unlocked in the instant
            // before this lock holds it now, and removes it: try another
            // name.
            if !try_lock(&file)? {
                last = io::Error::other("a scan took the new lease's lock");
                continue;
            }
            match std::fs::rename(&staging, &lease) {
                Ok(()) => {
                    return Ok(WriterLease {
                        id,
                        _file: file,
                        counter: AtomicU64::new(0),
                    })
                }
                // A scan removed the staging file before this lock: try
                // another name.
                Err(e) if e.kind() == io::ErrorKind::NotFound => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    pub fn id(&self) -> WriterId {
        self.id
    }

    /// The next value of this instance's store counter.
    pub fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

/// The path of writer `id`'s lease under `root`.
pub(crate) fn lease_path(root: &Path, id: &WriterId) -> PathBuf {
    root.join(LEASE_DIR).join(format!("{id}.{LEASE_SUFFIX}"))
}

/// The staging name a lease is created under: its final path with the
/// staging extension added, so the rename into place only drops it.
fn staging_path(lease: &Path) -> PathBuf {
    let mut path = lease.as_os_str().to_owned();
    path.push(format!(".{STAGING_SUFFIX}"));
    PathBuf::from(path)
}

/// Whether `name` is a lease, or a lease still being created: the only
/// files the scan removes from the lease directory.
pub fn is_lease_name(name: &str) -> bool {
    let id = name
        .strip_suffix(&format!(".{LEASE_SUFFIX}"))
        .or_else(|| name.strip_suffix(&format!(".{LEASE_SUFFIX}.{STAGING_SUFFIX}")));
    id.is_some_and(|id| WriterId::parse(id).is_some())
}

/// Try to take the exclusive lock on `file` without waiting: `true` if this
/// call took it, `false` if another open file description holds it, in this
/// process or another.
pub fn try_lock(file: &File) -> io::Result<bool> {
    // SAFETY: `flock` only reads the descriptor, which `file` keeps open for
    // the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Whether writer `id` may still be running. The answer is:
///
/// - `true` if its lease under `root` is locked;
/// - `false` if the lease is missing or unlocked;
/// - `true` if the check itself fails, for example because the lease cannot
///   be opened: the scan must never remove a temporary when it is unsure.
pub fn writer_may_be_running(root: &Path, id: &WriterId) -> bool {
    let file = match File::open(lease_path(root, id)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    // Taking the lock means no one held it; closing `file` releases it.
    !matches!(try_lock(&file), Ok(true))
}

/// A parsed temporary name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TempName<'a> {
    /// The name of the object the temporary will be published as.
    pub object_name: &'a str,
    pub writer: WriterId,
    pub counter: u64,
}

/// A temporary's file name.
pub fn temp_name(object_name: &str, writer: &WriterId, counter: u64) -> String {
    format!("{object_name}.{writer}.{counter}.{TEMP_SUFFIX}")
}

/// Parse a temporary's file name; `None` for anything else, including a
/// canonical name, and for any name not spelled exactly as [`temp_name`]
/// writes it: the round trip rejects, say, a counter with a sign or leading
/// zeros. Whether the object's name is one the backend derives is for the
/// caller to check.
pub fn parse_temp_name(name: &str) -> Option<TempName<'_>> {
    let mut parts = name.split('.');
    let object_name = parts.next()?;
    let writer = WriterId::parse(parts.next()?)?;
    let counter = parts.next()?.parse().ok()?;
    let parsed = TempName {
        object_name,
        writer,
        counter,
    };
    (!object_name.is_empty() && temp_name(object_name, &writer, counter) == name).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITER: WriterId = WriterId {
        pid: 4711,
        nonce: 0x6fd5_a44b_0000_0001,
    };

    /// A temporary's name, as FDS-STORAGE.PUBLISH-01 spells it, parses back
    /// into the parts it was written from.
    #[test]
    fn name_round_trips() {
        let name = temp_name("a1b2c3d4e5f6", &WRITER, 17);
        assert_eq!(name, "a1b2c3d4e5f6.4711-6fd5a44b00000001.17.tmp");
        let parsed = parse_temp_name(&name).expect("a name this module wrote");
        assert_eq!(parsed.object_name, "a1b2c3d4e5f6");
        assert_eq!(parsed.writer, WRITER);
        assert_eq!(parsed.counter, 17);
    }

    /// A canonical name and other files are not temporaries: a missing
    /// nonce, another extension, an extra field.
    #[test]
    fn rejects_canonical_and_other_names() {
        assert!(parse_temp_name("a1b2c3d4").is_none());
        assert!(parse_temp_name("a1b2c3d4.4711.17.tmp").is_none());
        assert!(parse_temp_name("a1b2c3d4.4711-6fd5a44b00000001.17.bak").is_none());
        assert!(parse_temp_name("a1b2c3d4.6fd5a44b.4711.123456.17.tmp").is_none());
    }

    /// Only the one spelling this module writes parses: the same numbers
    /// spelled another way, which the integer parsers alone accept, belong
    /// to a file this backend never wrote. That covers a sign, leading zeros,
    /// and an uppercase or short nonce, in an identity or a counter.
    #[test]
    fn only_the_written_spelling_parses() {
        assert_eq!(WriterId::parse("4711-6fd5a44b00000001"), Some(WRITER));
        for id in [
            "+4711-6fd5a44b00000001",
            "04711-6fd5a44b00000001",
            "4711-6FD5A44B00000001",
            "4711-6fd5a44b0000001",
            "4711-+fd5a44b00000001",
        ] {
            assert_eq!(WriterId::parse(id), None, "{id}");
        }
        for name in [
            "a1b2.4711-6fd5a44b00000001.+17.tmp",
            "a1b2.4711-6fd5a44b00000001.017.tmp",
            "a1b2.4711-6FD5A44B00000001.17.tmp",
            ".4711-6fd5a44b00000001.17.tmp",
        ] {
            assert!(parse_temp_name(name).is_none(), "{name}");
        }
    }

    /// The restart scan's liveness test: a lease is held while its instance
    /// lives, even by this process, and is not once released or removed.
    #[test]
    fn a_held_lease_is_running_and_a_released_or_missing_one_is_not() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let lease = WriterLease::acquire(tmp.path()).expect("a lease");
        let id = lease.id();
        assert!(lease_path(tmp.path(), &id).exists());
        assert!(
            writer_may_be_running(tmp.path(), &id),
            "held, even by this process"
        );
        drop(lease);
        assert!(!writer_may_be_running(tmp.path(), &id), "released");
        std::fs::remove_file(lease_path(tmp.path(), &id)).expect("the lease file is there");
        assert!(!writer_may_be_running(tmp.path(), &id), "missing");
    }

    /// Two instances get different identities, each lease ends up under its
    /// final name with no staging file left behind, and the scan recognises
    /// leases, staging or not, and nothing else.
    #[test]
    fn leases_are_unique_and_leave_no_staging_file() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let a = WriterLease::acquire(tmp.path()).expect("a lease");
        let b = WriterLease::acquire(tmp.path()).expect("another lease");
        assert_ne!(a.id(), b.id());
        let mut names: Vec<String> = std::fs::read_dir(tmp.path().join(LEASE_DIR))
            .expect("the lease directory exists")
            .map(|e| {
                e.expect("an entry can be read")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 name")
            })
            .collect();
        names.sort();
        let mut expected = vec![
            format!("{}.{LEASE_SUFFIX}", a.id()),
            format!("{}.{LEASE_SUFFIX}", b.id()),
        ];
        expected.sort();
        assert_eq!(names, expected);
        assert!(names.iter().all(|n| is_lease_name(n)));
        assert!(is_lease_name(&format!(
            "{}.{LEASE_SUFFIX}.{STAGING_SUFFIX}",
            a.id()
        )));
        assert!(!is_lease_name("notes.txt"));
    }

    /// Each instance numbers its own temporaries, from zero.
    #[test]
    fn each_lease_has_its_own_counter() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let a = WriterLease::acquire(tmp.path()).expect("a lease");
        let b = WriterLease::acquire(tmp.path()).expect("another lease");
        assert_eq!((a.next_counter(), a.next_counter()), (0, 1));
        assert_eq!(b.next_counter(), 0, "b's counter is its own");
    }
}
