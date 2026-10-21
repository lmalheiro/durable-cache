// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `Storage::purge` (FDS-STORAGE.PURGE-01): unlink the canonical path of
//! exactly the presented target — resolved from its key's object name, so
//! a varianced key purges that variant and a primary key its primary — and
//! never any other file. An invalidation also ends the key's stores in
//! flight, so none of them lands after it (FRE-COMPLIANCE.PURGE-01), and is
//! answered only once the shard directory has been synchronised, so that
//! its answer survives a host crash (FRE-COMPLIANCE.PURGEDURABLE-01). A
//! purge made to evict does neither.

use crate::error::{refuse_read_only, Op};
use crate::publish;
use crate::FsCache;
use pingora_cache::eviction::CacheEntryKey;
use pingora_cache::key::CompactCacheKey;
use pingora_cache::storage::{PurgeOutcome, PurgeTarget, PurgeType};
use pingora_error::{Error, ErrorType, Result};
use std::io;

/// Resolve a purge target to the key whose canonical path it names, or
/// `None` when this backend cannot act on it.
///
/// An `Exact` target carrying an `Identified` entry key names a stored
/// generation, and this backend issues none (ADR-COMPAT.ENTRYID-01), so it
/// can only be a target some other backend's identity scheme produced.
/// Resolving it to the key's canonical path would remove an object the
/// caller did not name; reporting it as absent is the honest answer, and is
/// what `MemCache`, the in-memory backend of `pingora-cache`, does too.
pub(crate) fn resolve(target: PurgeTarget<'_>) -> Option<&CompactCacheKey> {
    match target {
        PurgeTarget::Exact(CacheEntryKey::Identified { .. }) => None,
        _ => Some(target.key()),
    }
}

// IMP-STORAGE.PURGE-01: satisfy FDS-STORAGE.PURGE-01
// IMP-COMPAT.ENTRYID-01: satisfy ADR-COMPAT.ENTRYID-01
/// Purge `target`: unlink the canonical path of the key it names and, for
/// an invalidation, end the key's stores in flight.
///
/// Reports `Purged(None)` once a record is removed or an invalidation has
/// ended a store in flight — `None` because this backend assigns no entry
/// identifier, which is what makes the caller's `eviction.remove` address
/// the same key-only entry it admitted — and `NotFound` when nothing was
/// there (`unlink`'s own `ENOENT` is exactly the absent-key signal, so
/// idempotency needs no bookkeeping of its own —
/// FRE-COMPLIANCE.PURGEABSENT-01) or when the target names a generation
/// ([`resolve`]). An invalidation synchronises the shard directory before
/// it reports either; a purge made to evict does not (`purge_key`). Fails
/// once the backend is read-only, when the unlink fails, and when an
/// invalidation's synchronisation fails.
pub fn purge(
    cache: &FsCache,
    target: PurgeTarget<'_>,
    purge_type: PurgeType,
) -> Result<PurgeOutcome> {
    let Some(key) = resolve(target) else {
        return Ok(PurgeOutcome::NotFound);
    };
    purge_key(cache, key, purge_type)
}

/// [`purge`] with the target already resolved to the key it names, so the
/// caller can hand an owned key to the blocking pool
/// (FDS-STORAGE.EXECUTOR-01).
pub fn purge_key(
    cache: &FsCache,
    key: &CompactCacheKey,
    purge_type: PurgeType,
) -> Result<PurgeOutcome> {
    let name = cache.object_name(key);
    let path = cache.canonical_path(&name)?;
    // An invalidation removes an object on request, and its answer must
    // survive a host crash (FRE-COMPLIANCE.PURGEDURABLE-01). A purge made to
    // evict is the cache's own choice: a crash that undoes it brings back an
    // object that is still valid, which the restart scan admits, and evicts
    // again if the cache is over capacity.
    let invalidation = match purge_type {
        PurgeType::Invalidation => true,
        PurgeType::Eviction => false,
    };

    // The record is held open across the unlink, so the kernel frees it
    // when `purged` is dropped, at the end, rather than inside the unlink,
    // under the lock.
    let (removed, ended, purged) = {
        let Some(_change) = cache.locks.begin(&name) else {
            // Once read-only, this process changes nothing on disk, so a
            // purge fails rather than being reported as applied
            // (FRE-COMPAT.HOTUPGRADE-01).
            return refuse_read_only(Op::Purge);
        };
        let purged = publish::hold_open(&path);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                return Error::e_because(
                    ErrorType::FileWriteError,
                    format!("unlinking {}", path.display()),
                    e,
                )
            }
        };
        // A store of the key in flight may carry the content an
        // invalidation removes, if it began before the change that prompted
        // the purge reached the origin, so the invalidation ends it too. It
        // does so under the lock a store's `finish` takes to publish: a
        // store that published before has had its record removed above, and
        // one still in flight now publishes nothing. A purge made to evict
        // leaves stores alone.
        let ended = invalidation && cache.inflight.purge(&name);
        (removed, ended, purged)
    };

    if invalidation {
        // Outside the lock: only the directory entry's durability is left
        // to settle, and nothing about it depends on the key's state. An
        // invalidation that removed nothing synchronises too: a purge made
        // to evict may have unlinked the record just before, unsynchronised,
        // and a crash undoing that would bring the object back after this
        // purge had reported it absent. A shard directory that does not
        // exist has nothing to synchronise.
        let dir = cache.shard_dir(&name)?;
        if removed || dir.exists() {
            cache.sync_shard_dir(&dir).map_err(|e| {
                log::warn!(
                    "{} of {} could not synchronise {}: {e}",
                    Op::Purge,
                    path.display(),
                    dir.display()
                );
                e
            })?;
        }
    }
    if !removed && !ended {
        return Ok(PurgeOutcome::NotFound);
    }
    // Only now free the record: an invalidation's removal is durable.
    test_hook! {
        cache.hooks.pause(crate::hooks::PausePoint::BeforeFreeingOldRecord, &path);
    }
    drop(purged);
    Ok(PurgeOutcome::Purged(None))
}
