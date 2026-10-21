// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! "Eviction Recovery" (FDS-EVICTION.STARTUP-01): a public API surface
//! distinct from `Storage`, called once per process start — possibly after
//! serving has begun — that rebuilds the eviction manager's view from the
//! published records. Riding the same pass, it reclaims what
//! FDS-STORAGE.RECLAIM-01 classifies as orphaned. No snapshot and no delta
//! log: a clean shutdown and a crash take the identical path.
//!
//! Running alongside live traffic is safe by construction: every admission
//! is re-checked under the key's publication lock, which a live store's
//! rename also takes, and a ghost pass after enumeration drops entries
//! whose file has gone.

use crate::blocking;
use crate::format::HeaderError;
#[cfg(test)]
use crate::hooks::PausePoint;
use crate::key_codec;
use crate::metrics;
use crate::purge;
use crate::reclaim::{self, ReclaimClass};
use crate::record::{self, FileIdentity, ReadError, RecordError, RecordKind};
use crate::sharding;
use crate::writerid;
use crate::FsCache;
use pingora_cache::eviction::{CacheEntryKey, CacheEntryKeyRef, EvictionManager};
use pingora_cache::storage::{PurgeTarget, PurgeType};
use std::fs::{DirEntry, File, FileType, ReadDir};
use std::io;
use std::path::Path;

// IMP-EVICTION.STARTUP-01: satisfy FDS-EVICTION.STARTUP-01

/// What one recovery pass did, for logging and tests. Fields may be added
/// in later versions; so that this breaks no integrator, code outside this
/// crate can read the report but not build it with a struct literal or
/// match all of its fields.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Records the scan admitted to the eviction manager, less those whose
    /// file was gone by the time it finished (evicted meanwhile, say): the
    /// manager no longer holds their entries either.
    pub admitted: usize,
    /// Crash-orphaned temporaries reclaimed.
    pub reclaimed_temps: u64,
    /// The bytes of the temporaries reclaimed.
    pub reclaimed_temp_bytes: u64,
    /// Damaged, foreign and misplaced records reclaimed.
    pub reclaimed_damaged: u64,
    /// The bytes of the damaged, foreign and misplaced records reclaimed.
    pub reclaimed_damaged_bytes: u64,
    /// Withdrawn records reclaimed.
    pub reclaimed_withdrawn: u64,
    /// The bytes of the withdrawn records reclaimed.
    pub reclaimed_withdrawn_bytes: u64,
    /// Directories and entries the scan could not list or read, for a reason
    /// other than their having vanished meanwhile: neither admitted nor
    /// reclaimed, and each named in a warning.
    pub skipped: u64,
}

impl RecoveryReport {
    fn add(&mut self, class: ReclaimClass, bytes: u64) {
        match class {
            ReclaimClass::Temporary => {
                self.reclaimed_temps += 1;
                self.reclaimed_temp_bytes += bytes;
            }
            ReclaimClass::Damaged => {
                self.reclaimed_damaged += 1;
                self.reclaimed_damaged_bytes += bytes;
            }
            ReclaimClass::Withdrawn => {
                self.reclaimed_withdrawn += 1;
                self.reclaimed_withdrawn_bytes += bytes;
            }
        }
    }

    fn counts(&self) -> [(ReclaimClass, u64, u64); 3] {
        [
            (
                ReclaimClass::Temporary,
                self.reclaimed_temps,
                self.reclaimed_temp_bytes,
            ),
            (
                ReclaimClass::Damaged,
                self.reclaimed_damaged,
                self.reclaimed_damaged_bytes,
            ),
            (
                ReclaimClass::Withdrawn,
                self.reclaimed_withdrawn,
                self.reclaimed_withdrawn_bytes,
            ),
        ]
    }
}

/// Rebuild `eviction`'s view of the cache from what is on disk, reclaiming
/// orphaned files as it goes. Safe to call on a first launch, a post-crash
/// restart, or the incoming side of a hot upgrade; on the last, the
/// integrator calls it only once its server holds the listening sockets and
/// the upgrade's close timeout has elapsed (FDS-COMPAT.READONLY-01).
///
/// A directory or file the scan cannot list or read is skipped, named in a
/// warning and counted: one bad entry must not stop the admission of the
/// rest. The pass itself does not fail today; the `Result` keeps room for a
/// failure that would stop it.
///
/// `cache` is `'static`, as it already is to serve as a `Storage`: what the
/// scan removes, it removes on the blocking pool.
pub async fn recover(
    cache: &'static FsCache,
    eviction: &dyn EvictionManager,
) -> pingora_error::Result<RecoveryReport> {
    recover_until(cache, eviction, &|| false).await
}

/// [`recover`], stopping at its next entry once `shutting_down` returns
/// true: the server the hot-upgrade helper runs it in has started shutting
/// down, and the eviction manager it fills is about to be discarded
/// (FDS-COMPAT.READONLY-01). Its summary line then says so.
pub(crate) async fn recover_until(
    cache: &'static FsCache,
    eviction: &dyn EvictionManager,
    shutting_down: &(dyn Fn() -> bool + Sync),
) -> pingora_error::Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    // The entry of every record the walk admitted, for the ghost pass. Its
    // name is derived again from its key there, rather than kept beside it:
    // the walk admits a record only at its own key's path.
    let mut admitted: Vec<CacheEntryKey> = Vec::new();
    // Whether the scan stopped for its server shutting down.
    let mut shut_down = false;

    // The walk. A record lives two directory levels below the root, at
    // `<root>/<first>/<second>/<name>` (SDS-LAYOUT.SHARDING-01), where
    // `<first>` and `<second>` are its name's first two pairs of hex digits.
    // So the outer loop does not list the root: it visits the 256
    // first-level directories there can be, whether they exist yet or not,
    // which keeps the walk out of everything else at the root, the writers'
    // leases among them.
    'walk: for first_dir in sharding::first_level_dirs(cache.root()) {
        let Some(second_dirs) = list(&first_dir, &mut report) else {
            continue;
        };
        // Each second-level directory in this first-level one. Every
        // directory here is walked, whatever its name: a record found in
        // the wrong one is reclaimed as misplaced (FDS-STORAGE.RECLAIM-01).
        for second in second_dirs {
            let Some(second) = listed(second, &first_dir, &mut report) else {
                continue;
            };
            if !has_type(&second, FileType::is_dir, &mut report) {
                continue;
            }
            let second_dir = second.path();
            let Some(files) = list(&second_dir, &mut report) else {
                continue;
            };
            // Each file in this second-level directory: a record, a store's
            // temporary, or a file this backend did not write, which
            // `process_entry` tells apart by name. Anything but a regular
            // file is passed over.
            for entry in files {
                let Some(entry) = listed(entry, &second_dir, &mut report) else {
                    continue;
                };
                if !has_type(&entry, FileType::is_file, &mut report) {
                    continue;
                }
                // The walk is blocking file-system work, and the integrator
                // may be awaiting it on a runtime that is also serving
                // traffic (FDS-EVICTION.STARTUP-01 allows exactly that). One
                // entry -- a header, a key, and a metadata region -- is the
                // unit of work between yields, so a scan of a large tree
                // never holds the worker for more than a single record's
                // reads (FDS-STORAGE.EXECUTOR-01). Unlike the request path,
                // those reads stay on the worker rather than move to the
                // blocking pool: `recover` borrows the eviction manager for
                // the length of the call, so its admissions cannot be sent
                // to another thread. What the scan removes goes to the pool,
                // since freeing a large file takes far longer.
                tokio::task::yield_now().await;
                // Once read-only, the scan neither reclaims nor admits
                // (FDS-COMPAT.READONLY-01), so it stops rather than read the
                // rest of the tree for nothing. It stops as well once its
                // server starts shutting down, for the eviction manager it
                // fills is about to be discarded.
                if cache.is_read_only() {
                    break 'walk;
                }
                if shutting_down() {
                    shut_down = true;
                    break 'walk;
                }
                process_entry(cache, &entry.path(), eviction, &mut admitted, &mut report).await;
            }
        }
    }

    test_hook! { cache.hooks.pause(PausePoint::RecoverBeforeGhostPass, cache.root()); }

    // The ghost pass (FDS-EVICTION.STARTUP-01). While the walk runs beside
    // live traffic, it can leave the eviction manager an entry whose record
    // is gone, a ghost:
    //
    // 1. A live store's admission makes the manager return a victim, V, and
    //    the manager drops V's entry at once.
    // 2. `HttpCache` purges V's record only afterwards, in a task of its own.
    // 3. In between, the walk reaches V's record, still on disk, finds it
    //    unchanged under V's lock, and admits it: V's entry is back.
    // 4. The purge then removes V's record. V's entry outlives it, counting
    //    V's size against the budget until V is evicted, or stored, again.
    //
    // So once the walk is done, this pass checks, under each admitted
    // record's lock, that the record still exists, and removes the entry of
    // any that does not. The records the walk's own admissions evicted are
    // gone too: the manager has already dropped their entries, so for them
    // the pass only takes them out of `admitted`. A purge leaves no ghost,
    // since `HttpCache` removes a purged object's entry itself, after the
    // purge; and a key live traffic republishes after its check is admitted
    // by the live path.
    for entry in &admitted {
        // One record at a time, as in the walk: the pass visits every
        // record the walk admitted (FDS-STORAGE.EXECUTOR-01).
        tokio::task::yield_now().await;
        // And, as in the walk, not once the server is shutting down.
        if shutting_down() {
            shut_down = true;
            break;
        }
        let name = cache.object_name(entry.key());
        let Ok(path) = cache.canonical_path(&name) else {
            continue;
        };
        // Once read-only, the remaining checks are skipped: the eviction
        // manager they would update is discarded when the process exits
        // (FDS-COMPAT.READONLY-01).
        let Some(_change) = cache.locks.begin(&name) else {
            break;
        };
        if !path.exists() {
            // The borrowed form of the very identity admitted below: a
            // key-only entry, since this backend assigns no entry
            // identifier (ADR-COMPAT.ENTRYID-01). A manager is required to
            // match the complete identity, so anything else would not
            // remove what the scan admitted.
            eviction.remove(CacheEntryKeyRef::from(entry));
            report.admitted -= 1;
        }
    }

    // The leases of writers that have stopped, whose temporaries the walk
    // has just reclaimed (ADR-STORAGE.WRITERID-01). Left, too, for the next
    // start, by a scan stopped for its server shutting down.
    if !shut_down {
        let leases = reclaim::reclaim_leases(cache);
        if leases > 0 {
            log::debug!("removed {leases} stopped writers' leases");
        }
    }

    report_scan(cache, &report, shut_down);
    Ok(report)
}

async fn process_entry(
    cache: &'static FsCache,
    path: &Path,
    eviction: &dyn EvictionManager,
    admitted: &mut Vec<CacheEntryKey>,
    report: &mut RecoveryReport,
) {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if let Some(temp) = writerid::parse_temp_name(file_name) {
        if !reclaim::temp_writer_may_be_running(cache.root(), &temp) {
            let size = match path.symlink_metadata() {
                Ok(meta) => meta.len(),
                // Vanished since the directory listing.
                Err(e) if e.kind() == io::ErrorKind::NotFound => return,
                Err(e) => return skip(report, path, &e),
            };
            // On the blocking pool: nothing holds a stopped writer's
            // temporary open, so the unlink frees it.
            let temp_path = path.to_path_buf();
            match blocking::run(move || Ok(reclaim::reclaim_temp(cache, &temp_path, size))).await {
                Ok(Some(bytes)) => report.add(ReclaimClass::Temporary, bytes),
                Ok(None) => {}
                Err(e) => log::warn!("could not reclaim the temporary {}: {e}", path.display()),
            }
        }
        return;
    }
    if !sharding::is_canonical_name(file_name) {
        return; // not a file this backend wrote
    }
    let name = file_name;

    // Open until this returns, or `reclaim_record` takes it: a record the
    // scan reclaims is freed when `file` is closed, after the key's lock.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => return skip(report, path, &e),
    };
    // The size of the very file read, not of whatever the path named a
    // moment before.
    let (identity, size) = match file.metadata() {
        Ok(meta) => (FileIdentity::of(&meta), meta.len()),
        Err(e) => return skip(report, path, &e),
    };
    // A withdrawn record holds no object: a metadata update withdrew it, and
    // a store of its key would have replaced it. Lookups leave it in place,
    // so the scan reclaims it.
    // A damaged or foreign record cannot be admitted — its key and
    // freshness deadline cannot be trusted or decoded — and lookups leave
    // it in place too.
    let mut read = record::read_record_blocking(&file, RecordKind::Published, None);
    // Checksums that kept failing may be an in-place metadata update caught
    // between its two writes. Every in-place update holds the key's
    // publication lock from its first write to its last, so one more read
    // under that lock finds the record whole, and only a mismatch then is
    // damage. A lookup re-reads the same way (SDS-LAYOUT.OBJECTFILE-01), but
    // it reports damage as a miss and leaves the file for a later store of
    // the key to replace; the scan reclaims the file, since that store may
    // never come. The read changes nothing, so it takes the lock alone,
    // outside the read-only gate, and releases it before any reclaim takes
    // it again.
    if matches!(&read, Err(ReadError::Record(e)) if e.is_retryable()) {
        let _key = cache.locks.lock_key(name);
        read = record::read_record_once(&file, RecordKind::Published, None);
    }
    let parsed = match read {
        Ok(parsed) => parsed,
        Err(ReadError::Record(RecordError::Header(HeaderError::Withdrawn))) => {
            if let Some(bytes) = reclaim_record(cache, name, path, file, identity, size).await {
                report.add(ReclaimClass::Withdrawn, bytes);
            }
            return;
        }
        Err(ReadError::Record(e)) => {
            log::debug!("reclaiming {} ({e})", path.display());
            if let Some(bytes) = reclaim_record(cache, name, path, file, identity, size).await {
                report.add(ReclaimClass::Damaged, bytes);
            }
            return;
        }
        Err(ReadError::Io(e)) => return skip(report, path, &e),
    };
    let (Ok(key), Ok(record_meta)) = (
        key_codec::decode(&parsed.key_bytes),
        cache.decode_meta(&parsed.internal_bytes, &parsed.header_bytes),
    ) else {
        if let Some(bytes) = reclaim_record(cache, name, path, file, identity, size).await {
            report.add(ReclaimClass::Damaged, bytes);
        }
        return;
    };
    // A record found anywhere but at its own key's path, put there by hand
    // or by a defect, is never served: a lookup of its key reads that other
    // path, and a lookup of the key this path belongs to finds a key
    // mismatch. So it is reclaimed, as a damaged record is.
    let own_path = cache.canonical_path(&cache.object_name(&key));
    if own_path.as_deref().ok() != Some(path) {
        log::debug!(
            "reclaiming {}: the record it holds belongs elsewhere",
            path.display()
        );
        if let Some(bytes) = reclaim_record(cache, name, path, file, identity, size).await {
            report.add(ReclaimClass::Damaged, bytes);
        }
        return;
    }

    // A `Vary` variant is admitted like any other record, whatever its
    // primary holds: an integrator's cache key may carry a variance of its
    // own, so a lookup can reach a variant no primary was ever stored for,
    // and a variant no lookup can reach is evicted like any record no
    // request reaches (ADR-STORAGE.RECLAIM-01).
    test_hook! { cache.hooks.pause(PausePoint::RecoverBeforeAdmit, path); }

    // Admit under the key's publication lock, after re-checking that the
    // path still names the file just read: live traffic may have removed
    // it, or published and will admit its replacement itself.
    // The same shape `HttpCache` builds for a live admission, so the scan
    // and live traffic address one entry rather than two: the cache key
    // alone, with no storage-assigned identifier (ADR-COMPAT.ENTRYID-01).
    let entry = CacheEntryKey::key_only(key);
    let victims = {
        // Skipped once read-only, as the ghost pass is.
        let Some(_change) = cache.locks.begin(name) else {
            return;
        };
        let Ok(current) = std::fs::metadata(path) else {
            return;
        };
        if FileIdentity::of(&current) != identity {
            return;
        }
        // The file's current length, still under the lock, is the record
        // size FRE-COMPAT.EVICTION-01 defines.
        eviction.admit(
            entry.clone(),
            current.len() as usize,
            record_meta.fresh_until(),
        )
    };
    // Outside the lock: a victim's own publication lock may be the same
    // stripe as this key's. Each victim is named by the exact entry the
    // manager returned, as `HttpCache` names them after a live admission,
    // and purged on the blocking pool, as `HttpCache`'s purges are: the
    // purge frees the victim's record.
    for victim in victims {
        let purged = blocking::run(move || {
            purge::purge(cache, PurgeTarget::Exact(&victim), PurgeType::Eviction)
        })
        .await;
        if let Err(e) = purged {
            log::warn!("could not purge an eviction victim: {e}");
        }
    }
    admitted.push(entry);
    report.admitted += 1;

    test_hook! { cache.hooks.pause(PausePoint::RecoverAfterAdmit, path); }
}

/// The entries of `dir`, or `None` when it cannot be listed, counted as
/// skipped unless it does not exist: a first-level directory is made only
/// when a key first needs it.
fn list(dir: &Path, report: &mut RecoveryReport) -> Option<ReadDir> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            skip(report, dir, &e);
            None
        }
    }
}

/// A directory entry, or `None` when `dir` could not be read on, counted as
/// skipped.
fn listed(
    entry: io::Result<DirEntry>,
    dir: &Path,
    report: &mut RecoveryReport,
) -> Option<DirEntry> {
    entry.map_err(|e| skip(report, dir, &e)).ok()
}

/// Whether `entry` has the type `is` accepts. An entry whose type cannot be
/// told is counted as skipped, unless it has vanished.
fn has_type(entry: &DirEntry, is: fn(&FileType) -> bool, report: &mut RecoveryReport) -> bool {
    match entry.file_type() {
        Ok(file_type) => is(&file_type),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            skip(report, &entry.path(), &e);
            false
        }
    }
}

/// Count what the scan could not list or read, and name it in a warning:
/// it is neither admitted nor reclaimed (FDS-STORAGE.RECLAIM-01).
fn skip(report: &mut RecoveryReport, path: &Path, e: &dyn std::fmt::Display) {
    log::warn!("restart scan skipped {}: {e}", path.display());
    report.skipped += 1;
}

/// Reclaim the canonical record at `path`, read from `file`, on the
/// blocking pool: the unlink, under the key's lock, and then the close that
/// frees the record, which for a large record would hold the worker far
/// longer than reading it did (FDS-STORAGE.EXECUTOR-01). Returns the bytes
/// reclaimed, or `None` if the record was left in place.
async fn reclaim_record(
    cache: &'static FsCache,
    name: &str,
    path: &Path,
    file: File,
    identity: FileIdentity,
    size: u64,
) -> Option<u64> {
    let (name, owned_path) = (name.to_owned(), path.to_path_buf());
    let reclaimed = blocking::run(move || {
        let reclaimed = reclaim::reclaim_canonical(cache, &name, &owned_path, identity, size);
        // After the key's lock, which `reclaim_canonical` has released.
        drop(file);
        Ok(reclaimed)
    })
    .await;
    reclaimed.unwrap_or_else(|e| {
        log::warn!("could not reclaim {}: {e}", path.display());
        None
    })
}

/// One summary line per scan, plus the counters when the `metrics` feature
/// is enabled (FDS-STORAGE.RECLAIM-01, "Reporting"). A scan that found the
/// backend read-only, or that `shut_down` for its server, stopped short,
/// and says why.
fn report_scan(cache: &FsCache, report: &RecoveryReport, shut_down: bool) {
    let outcome = if cache.is_read_only() {
        "stopped, the backend being read-only"
    } else if shut_down {
        "stopped, the server shutting down"
    } else {
        "complete"
    };
    log::info!(
        "restart scan {outcome}: admitted {}; skipped {} it could not read; \
         reclaimed {} temporary ({} bytes), {} damaged ({} bytes), \
         {} withdrawn ({} bytes)",
        report.admitted,
        report.skipped,
        report.reclaimed_temps,
        report.reclaimed_temp_bytes,
        report.reclaimed_damaged,
        report.reclaimed_damaged_bytes,
        report.reclaimed_withdrawn,
        report.reclaimed_withdrawn_bytes,
    );
    for (class, files, bytes) in report.counts() {
        metrics::record_reclaimed(class, files, bytes);
    }
}
