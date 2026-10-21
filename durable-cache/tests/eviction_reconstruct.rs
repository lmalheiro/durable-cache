// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-EVICTION.RECONSTRUCT-01 — Reconstruction converges exactly.
//!
//! When `recover()` returns, the manager reflects the published set
//! exactly, with record sizes — whether serving waited for it or ran
//! alongside it — with no snapshot or delta log involved.

use std::time::Duration;

use durable_cache::hooks::PausePoint;
use durable_cache::recovery::recover;
use durable_cache::FsCache;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;

use super::support;

// IMP-TC.EVICTION.RECONSTRUCT-01: satisfy TC-EVICTION.RECONSTRUCT-01

/// The manager's totals must equal the count and aggregate record size of
/// the files actually published.
fn assert_matches_disk(cache: &FsCache, manager: &Manager, keys: &[&pingora_cache::CacheKey]) {
    let present: Vec<_> = keys
        .iter()
        .filter(|k| support::canonical_path(cache, k).exists())
        .collect();
    let total_size: u64 = present
        .iter()
        .map(|k| {
            std::fs::metadata(support::canonical_path(cache, k))
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(manager.total_items(), present.len(), "total_items");
    assert_eq!(manager.total_size() as u64, total_size, "total_size");
    for key in keys {
        let should_be_there = support::canonical_path(cache, key).exists();
        assert_eq!(
            manager.peek(&support::entry(key)),
            should_be_there,
            "the manager and the disk must agree about every key"
        );
    }
}

/// Step 1: a populated cache, recovered without serving.
#[tokio::test]
async fn tc_eviction_reconstruct_01_totals_match_the_published_set() {
    let (cache, tmp) = support::leaked_cache();
    let keys: Vec<_> = (0..12)
        .map(|i| support::key(&format!("/reconstruct-{i}")))
        .collect();
    for (i, key) in keys.iter().enumerate() {
        let body = vec![b'x'; 100 * (i + 1)];
        support::store(cache, key, &support::meta(), &body).await;
    }
    // A variant, and an object purged before the restart.
    let variant = support::variant(&keys[0], [0x33; 16]);
    support::store(cache, &variant, &support::meta(), b"variant").await;
    support::purge_active(cache, &keys[1]).await;

    // A "restart": a new backend instance over the same directory.
    let restarted = support::leaked_cache_at(tmp.path());
    let manager = Manager::new(usize::MAX);
    let report = recover(restarted, &manager).await.unwrap();

    let mut all: Vec<&pingora_cache::CacheKey> = keys.iter().collect();
    all.push(&variant);
    assert_eq!(
        report.admitted,
        all.iter()
            .filter(|k| support::canonical_path(restarted, k).exists())
            .count()
    );
    assert_matches_disk(restarted, &manager, &all);
}

/// Step 2: live traffic replaces an object after the scan has read it but
/// before it is admitted. The scan must not admit the stale size; the live
/// store's own admission stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_eviction_reconstruct_01_skips_a_record_replaced_before_admission() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/reconstruct-replaced");
    support::store(cache, &key, &support::meta(), b"small").await;
    let path = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::RecoverBeforeAdmit, Some(&path));
    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        let report = recover(cache, &manager).await.unwrap();
        (report, manager)
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the scan must pause"
    );

    // A replacement lands while the scan holds the record it just read.
    support::store(
        cache,
        &key,
        &support::meta(),
        b"a much larger body than before",
    )
    .await;
    pause.release();

    let (report, manager) = scanning.await.unwrap();
    assert_eq!(
        report.admitted, 0,
        "the scan must skip the record it no longer holds"
    );
    assert!(
        !manager.peek(&support::entry(&key)),
        "the stale size is never admitted; the live store admits its own"
    );
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"a much larger body than before"
    );
}

/// Step 2, continued: a record removed after the scan admitted it is
/// dropped again by the ghost pass, so no entry survives for a file that is
/// gone. That is what a live eviction leaves when its purge lands after the
/// scan admitted its victim again; the purge here, made directly through
/// `Storage`, leaves the manager alone as that one does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_eviction_reconstruct_01_ghost_pass_drops_purged_entries() {
    let (cache, _tmp) = support::leaked_cache();
    let purged = support::key("/reconstruct-ghost");
    let kept = support::key("/reconstruct-kept");
    support::store(cache, &purged, &support::meta(), b"purged later").await;
    support::store(cache, &kept, &support::meta(), b"kept").await;

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::RecoverBeforeGhostPass, Some(cache.root()));
    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        let report = recover(cache, &manager).await.unwrap();
        (report, manager)
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the scan must pause"
    );

    // Purged after it was admitted, before the ghost pass.
    support::purge_active(cache, &purged).await;
    pause.release();

    let (report, manager) = scanning.await.unwrap();
    assert_eq!(report.admitted, 1, "the ghost was un-admitted again");
    assert!(!manager.peek(&support::entry(&purged)));
    assert!(manager.peek(&support::entry(&kept)));
    assert_matches_disk(cache, &manager, &[&purged, &kept]);
}

/// Step 2, continued: objects the scan never reaches — stored after it has
/// passed their shard — are admitted by the live path, and a key
/// republished after the ghost check stays admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_eviction_reconstruct_01_converges_with_stores_the_scan_never_saw() {
    let (cache, _tmp) = support::leaked_cache();
    let seen = support::key("/reconstruct-seen");
    support::store(cache, &seen, &support::meta(), b"stored before the scan").await;

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.unwrap();
    assert_eq!(report.admitted, 1);

    // Stored after the scan returned: the live path admits it, and the
    // totals stay exact.
    let after = support::key("/reconstruct-after");
    let size = support::store(cache, &after, &support::meta(), b"stored after the scan").await;
    manager.admit(support::entry(&after), size, support::meta().fresh_until());

    assert_matches_disk(cache, &manager, &[&seen, &after]);
}

/// Step 4: no snapshot file and no append-only log is read or written at
/// any point — the tree holds records, and besides them only this
/// instance's lease, an empty file whose lock is all it carries
/// (ADR-STORAGE.WRITERID-01).
#[tokio::test]
async fn tc_eviction_reconstruct_01_uses_no_snapshot_or_log() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/reconstruct-no-snapshot");
    support::store(cache, &key, &support::meta(), b"body").await;

    let manager = Manager::new(usize::MAX);
    recover(cache, &manager).await.unwrap();

    // The only files anywhere under the root are records in leaf shards and
    // the lease.
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                files(&entry.path(), out);
            } else {
                out.push(entry.path());
            }
        }
    }
    let mut found = Vec::new();
    files(cache.root(), &mut found);
    found.sort();
    let lease = support::lease_path(cache.root(), &cache.lease.id());
    let mut expected = vec![support::canonical_path(cache, &key), lease.clone()];
    expected.sort();
    assert_eq!(found, expected);
    assert_eq!(
        std::fs::metadata(&lease).unwrap().len(),
        0,
        "the lease holds no state"
    );
}

/// Step 5: once the backend is read-only, the restart scan stops at once.
/// It reads no record, admits nothing and changes nothing
/// (FDS-COMPAT.READONLY-01): the eviction manager it would update is
/// discarded when the process exits, and the records stay on disk for the
/// process that takes over. Were a record read, the scan would stop at the
/// pause armed after the read, and the timeout would fail the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_eviction_reconstruct_01_a_read_only_scan_stops_at_once() {
    let (cache, _tmp) = support::leaked_cache();
    let keys = [support::key("/read-only-a"), support::key("/read-only-b")];
    for key in &keys {
        support::store(cache, key, &support::meta(), b"body").await;
    }
    cache.set_read_only();
    let read = cache
        .hooks()
        .arm_pause(PausePoint::RecoverBeforeAdmit, None);

    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        let report = recover(cache, &manager).await.expect("the scan completes");
        (report, manager)
    });
    let (report, manager) = tokio::time::timeout(Duration::from_secs(5), scanning)
        .await
        .expect("the scan stops without reading a record")
        .expect("the scanning task completes");
    assert!(!read.wait_reached(Duration::ZERO), "no record is read");
    assert_eq!(report.admitted, 0, "nothing admitted");
    assert_eq!(manager.total_items(), 0, "the manager is empty");
    for key in &keys {
        assert!(
            support::canonical_path(cache, key).exists(),
            "the records stay on disk"
        );
    }
}
