// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! FDS-STORAGE.RECLAIM-01: classifying what the restart-time scan reclaims
//! — crash-orphaned temporaries and their stopped writers' leases, and
//! damaged, foreign or withdrawn canonical records — and removing it safely
//! under live traffic. The tree walk these ride is
//! `crate::recovery::recover`'s, the same pass FDS-EVICTION.STARTUP-01
//! already needs.

// IMP-STORAGE.RECLAIM-01: satisfy FDS-STORAGE.RECLAIM-01
#[cfg(test)]
use crate::hooks::PausePoint;
use crate::record::FileIdentity;
use crate::sharding;
use crate::writerid;
use crate::FsCache;
use std::fs::File;
use std::path::Path;

/// The class of a file the restart scan reclaims: what `RecoveryReport`
/// counts by, and the label on the reclaim counters
/// (FDS-STORAGE.RECLAIM-01, "Reporting").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimClass {
    /// A crash-orphaned temporary.
    Temporary,
    /// A damaged, foreign or misplaced record.
    Damaged,
    /// A withdrawn record.
    Withdrawn,
}

/// Whether the writer of the temporary `temp` could still be running, in
/// which case the scan leaves it alone: the process's own stores satisfy
/// this (serving may already have begun), and so does a live hot-upgrade
/// peer's. The answer is the writer's lease under `root`
/// (ADR-STORAGE.WRITERID-01).
pub fn temp_writer_may_be_running(root: &Path, temp: &writerid::TempName<'_>) -> bool {
    if !sharding::is_canonical_name(temp.object_name) {
        // Not a name this backend writes: leave it alone.
        return true;
    }
    writerid::writer_may_be_running(root, &temp.writer)
}

/// Unlink a canonical record the scan has classified as reclaimable.
///
/// Never by path alone: a live store may publish a fresh record at this
/// path at any moment, so the file's identity is confirmed under the key's
/// publication lock and the entry is skipped if the path now names a
/// different file. Returns the bytes reclaimed, or `None` if the entry was
/// skipped.
///
/// The caller still has the record open, from reading it, and closes it
/// only after this returns: the kernel then frees the record at that close,
/// after the lock, rather than inside the unlink, under the lock. The scan
/// does both on the blocking pool (FDS-STORAGE.EXECUTOR-01).
pub(crate) fn reclaim_canonical(
    cache: &FsCache,
    name: &str,
    path: &Path,
    identity: FileIdentity,
    size: u64,
) -> Option<u64> {
    test_hook! { cache.hooks.pause(PausePoint::ReclaimBeforeUnlink, path); }

    let reclaimed = {
        // A read-only backend makes no durable change, so a pending reclaim
        // unlink is skipped (FDS-COMPAT.READONLY-01).
        let _change = cache.locks.begin(name)?;
        match std::fs::metadata(path) {
            Ok(current) if FileIdentity::of(&current) == identity => {}
            // Gone already, or a live store replaced it while the scan
            // classified it: leave the new file alone.
            _ => return None,
        }
        match std::fs::remove_file(path) {
            Ok(()) => Some(size),
            Err(e) => {
                log::warn!("could not reclaim {}: {e}", path.display());
                None
            }
        }
    };
    // The lock is released; the caller's close frees the record.
    test_hook! { cache.hooks.pause(PausePoint::BeforeFreeingOldRecord, path); }
    reclaimed
}

/// Remove the leases of writers that have stopped: a lease, or a lease left
/// half-created, whose lock the scan can take (ADR-STORAGE.WRITERID-01).
/// The lock is held across the removal, so a live writer's lease is never
/// removed: its lock is taken before it is renamed into place. Runs after
/// the tree walk, though the order does not matter: a temporary whose lease
/// is missing belongs to a stopped writer too. Returns how many were
/// removed.
pub(crate) fn reclaim_leases(cache: &FsCache) -> u64 {
    let Ok(entries) = std::fs::read_dir(cache.root().join(writerid::LEASE_DIR)) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(writerid::is_lease_name)
        {
            continue;
        }
        let Ok(lease) = File::open(&path) else {
            continue;
        };
        // Held: a live writer's, or one being created.
        if !matches!(writerid::try_lock(&lease), Ok(true)) {
            continue;
        }
        let Some(_change) = cache.locks.begin_keyless() else {
            break;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(e) => log::warn!(
                "could not remove the stopped writer's lease {}: {e}",
                path.display()
            ),
        }
        // `lease` closes here, releasing the lock on the removed file.
    }
    removed
}

/// Unlink a crash-orphaned temporary. No identity check is needed: nothing
/// is ever renamed onto a temporary's name. Nothing holds the temporary of
/// a stopped writer open either, so the unlink frees it, and the scan calls
/// this on the blocking pool (FDS-STORAGE.EXECUTOR-01).
pub(crate) fn reclaim_temp(cache: &FsCache, path: &Path, size: u64) -> Option<u64> {
    test_hook! { cache.hooks.pause(PausePoint::ReclaimBeforeUnlink, path); }
    // Held across the unlink; a read-only backend skips it.
    let _change = cache.locks.begin_keyless()?;
    match std::fs::remove_file(path) {
        Ok(()) => Some(size),
        Err(e) => {
            log::warn!("could not reclaim the temporary {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary whose object name is not a name the backend derives is
    /// left alone, although its writer has no lease.
    #[test]
    fn names_this_backend_does_not_write_are_left_alone() {
        let root = Path::new("/nonexistent");
        let stopped = writerid::WriterId {
            pid: 4711,
            nonce: 1,
        };
        let foreign = writerid::temp_name("notes", &stopped, 1);
        let temp = writerid::parse_temp_name(&foreign).expect("a temporary's name");
        assert!(temp_writer_may_be_running(root, &temp), "{foreign}");
    }

    /// A temporary is left alone while its writer's lease is held, and is
    /// reclaimable once the writer has stopped.
    #[test]
    fn a_held_leases_temporary_is_left_alone_and_a_stopped_ones_is_not() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let lease = writerid::WriterLease::acquire(tmp.path()).expect("a lease");
        let name = writerid::temp_name(&"ab".repeat(16), &lease.id(), 1);
        let temp = writerid::parse_temp_name(&name).expect("a temporary's name");
        assert!(temp_writer_may_be_running(tmp.path(), &temp));
        drop(lease);
        assert!(!temp_writer_may_be_running(tmp.path(), &temp));
    }
}
