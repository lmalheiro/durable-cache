// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! In-memory shard-directory cache, built once at construction
//! (FDS-LAYOUT.STARTUP-01), so an ordinary store never re-checks the
//! file system for a shard directory it has already seen.
//!
//! One bit per possible shard (`sharding::SHARD_COUNT`), held in atomic
//! `u64` words: the 8 KiB bitset SDS-LAYOUT.SHARDING-01 calls for. Inserts
//! are lock-free `fetch_or`s, and the set is purely additive: a shard
//! directory, once created, is never removed, so nothing here is ever
//! cleared or re-checked against disk.

use crate::sharding::{self, FIRST_LEVEL_COUNT, SHARD_COUNT};
use pingora_error::{Error, ErrorType, OrErr, Result};
use std::io;
use std::mem::size_of;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Bits per word of the bitset.
const WORD_BITS: usize = u64::BITS as usize;
/// Words in the bitset: one bit per shard.
const WORD_COUNT: usize = SHARD_COUNT / WORD_BITS;

// The bitset is a whole number of words. `WORD_COUNT` rounds down, so with
// a shard count that was not a multiple of the word size, the last shards
// would have no bit, and marking one would index past the end of `bits`.
const _: () = assert!(SHARD_COUNT % WORD_BITS == 0);
// The bitset takes the 8 KiB SDS-LAYOUT.SHARDING-01 states. A change to the
// shard scheme that changed its size fails the build here, so the spec is
// revised along with it.
const _: () = assert!(WORD_COUNT * size_of::<u64>() == 8 * 1024);

// IMP-LAYOUT.STARTUP-01: satisfy FDS-LAYOUT.STARTUP-01
pub struct ShardDirCache {
    bits: Vec<AtomicU64>,
}

impl ShardDirCache {
    /// Ensure every first-level directory exists (creating any missing),
    /// then list each one's existing second-level subdirectories into a
    /// fresh in-memory cache. Cost is proportional only to the number of
    /// first/second-level directories present — never to the object count.
    pub fn build(root: &Path) -> io::Result<Self> {
        let bits: Vec<AtomicU64> = (0..WORD_COUNT).map(|_| AtomicU64::new(0)).collect();
        let cache = ShardDirCache { bits };

        for first in 0..FIRST_LEVEL_COUNT {
            let first_dir = sharding::first_level_dir(root, first);
            std::fs::create_dir_all(&first_dir)?;

            for entry in std::fs::read_dir(&first_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                // Only a directory named as `sharding` names shards is one;
                // anything else in the tree is not this backend's.
                let Some(second) = entry.file_name().to_str().and_then(sharding::level_index)
                else {
                    continue;
                };
                cache.mark_present(sharding::shard_index_of(first, second));
            }
        }

        Ok(cache)
    }

    fn mark_present(&self, index: usize) {
        let word = index / WORD_BITS;
        let bit = index % WORD_BITS;
        // bitwise "OR" with the current value:
        self.bits[word].fetch_or(1 << bit, Ordering::AcqRel);
    }

    fn is_present(&self, index: usize) -> bool {
        let word = index / WORD_BITS;
        let bit = index % WORD_BITS;
        (self.bits[word].load(Ordering::Acquire) & (1 << bit)) != 0
    }

    /// Ensure the second-level shard directory for `name` exists,
    /// consulting the in-memory cache first. Two callers racing to create
    /// the same new shard both succeed: the directory is created once, since
    /// `create_dir`'s `AlreadyExists` from the losing racer is swallowed, not
    /// propagated, and each caller sets the shard's bit.
    pub fn ensure_shard_dir(&self, root: &Path, name: &str) -> Result<()> {
        let index = sharding::shard_index(name)
            .or_err(ErrorType::InternalError, "deriving a shard index")?;
        if self.is_present(index) {
            return Ok(());
        }
        // Built from `shard_dir` rather than `shard_path(..).parent()`, so
        // there is no `Option` to unwrap on the way.
        let dir = sharding::shard_dir(root, name)
            .or_err(ErrorType::InternalError, "deriving a shard path")?;
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Error::e_because(
                    ErrorType::FileCreateError,
                    format!("creating shard directory {}", dir.display()),
                    e,
                )
            }
        }
        self.mark_present(index);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, name: &str) -> bool {
        // A malformed name has no shard, so it is trivially not present;
        // reporting `false` makes the caller do the (failing) real work and
        // surface a proper error there, instead of panicking here.
        sharding::shard_index(name).is_ok_and(|index| self.is_present(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// Construction creates every first-level directory, so a store only
    /// ever has to create a second-level one.
    #[test]
    fn creates_every_first_level_dir() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        ShardDirCache::build(tmp.path()).expect("construction succeeds");
        let count = std::fs::read_dir(tmp.path())
            .expect("the root can be listed")
            .count();
        assert_eq!(count, FIRST_LEVEL_COUNT);
    }

    /// The cache built at construction records exactly the shard
    /// directories already on disk (TC-LAYOUT.DIRCACHE-01, step 1).
    #[test]
    fn matches_prepopulated_second_level_dirs_exactly() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        // Shard directories made directly on disk, bypassing the cache.
        std::fs::create_dir_all(tmp.path().join("a1").join("b2"))
            .expect("a shard directory can be made");
        std::fs::create_dir_all(tmp.path().join("00").join("ff"))
            .expect("a shard directory can be made");

        let cache = ShardDirCache::build(tmp.path()).expect("construction succeeds");
        assert!(cache.contains("a1b2xxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
        assert!(cache.contains("00ffxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
        assert!(!cache.contains("a1a1xxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
    }

    /// A directory not named as a shard directory is not recorded as one:
    /// otherwise `+f` would stand for shard `0f`, and `B2` for shard `b2`, as
    /// a plain `from_str_radix` reads them.
    #[test]
    fn ignores_directories_not_named_as_shards() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        for second in ["+f", "B2", "b", "b2c"] {
            std::fs::create_dir_all(tmp.path().join("00").join(second))
                .expect("a stray directory can be made");
        }

        let cache = ShardDirCache::build(tmp.path()).expect("construction succeeds");
        assert!(!cache.contains("000fxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), "+f");
        assert!(
            !cache.contains("00b2xxxxxxxxxxxxxxxxxxxxxxxxxxxx"),
            "B2, b2c"
        );
        assert!(!cache.contains("000bxxxxxxxxxxxxxxxxxxxxxxxxxxxx"), "b");
    }

    /// Stores racing into a new shard all succeed, the shard is recorded,
    /// and its directory is created once, at the path its name gives
    /// (TC-LAYOUT.DIRCACHE-01). A barrier releases every thread at once, so
    /// their `create_dir` calls really race, and the shard's two levels
    /// differ, so a mix-up of them would show in the directory's name.
    #[test]
    fn concurrent_first_time_stores_into_same_shard_all_succeed_once() {
        const THREADS: usize = 64;
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let cache = Arc::new(ShardDirCache::build(tmp.path()).expect("construction succeeds"));
        let root = tmp.path().to_path_buf();
        let name = "abcdxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let start = Arc::new(Barrier::new(THREADS));

        let racers: Vec<_> = (0..THREADS)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let root = root.clone();
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    cache.ensure_shard_dir(&root, name)
                })
            })
            .collect();
        for racer in racers {
            racer
                .join()
                .expect("a racing thread does not panic")
                .expect("every racing store succeeds");
        }

        assert!(cache.contains(name), "the shard is recorded");
        let second_level: Vec<String> = std::fs::read_dir(root.join("ab"))
            .expect("the first-level directory exists")
            .map(|entry| {
                entry
                    .expect("a directory entry can be read")
                    .file_name()
                    .into_string()
                    .expect("a UTF-8 name")
            })
            .collect();
        assert_eq!(
            second_level,
            ["cd"],
            "one directory, named for the second level"
        );
    }

    /// A fresh construction, as after a restart, finds the shard
    /// directories earlier stores created (TC-LAYOUT.DIRCACHE-01, step 3).
    #[test]
    fn restart_rebuilds_without_recreating_existing_shards() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let name = "fedcxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        let cache = ShardDirCache::build(tmp.path()).expect("construction succeeds");
        cache
            .ensure_shard_dir(tmp.path(), name)
            .expect("the shard directory is created");

        let restarted = ShardDirCache::build(tmp.path()).expect("construction succeeds");
        assert!(restarted.contains(name));
    }

    /// The losing racer's path, without depending on thread timing: the
    /// shard's directory already exists although its bit is not set, as when
    /// another thread or process created it after construction. Creating it
    /// again fails with `AlreadyExists`, which `ensure_shard_dir` must take
    /// as success, and the shard is then recorded. The racing test above
    /// reaches this path only when two threads run at once.
    #[test]
    fn a_shard_directory_created_meanwhile_is_accepted_and_recorded() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let cache = ShardDirCache::build(tmp.path()).expect("construction succeeds");
        let name = "abcdxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        std::fs::create_dir(tmp.path().join("ab").join("cd"))
            .expect("the directory is made without the cache");
        assert!(!cache.contains(name), "the cache has not seen it");

        cache
            .ensure_shard_dir(tmp.path(), name)
            .expect("a directory that already exists is not an error");
        assert!(cache.contains(name), "the shard is recorded");
    }
}
