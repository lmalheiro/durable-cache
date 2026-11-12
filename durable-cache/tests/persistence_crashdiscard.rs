// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-PERSISTENCE.CRASHDISCARD-01 — Discard after a host restart.
//!
//! A cache that published stores without synchronising them keeps its
//! records after a host crash, and reports one left short as a miss; with
//! the discard on, it serves none of them after a host restart it was not
//! flushed for, and all of them after a process restart or a host restart
//! it was flushed for (FDS-PERSISTENCE.CRASHDISCARD-01).
//!
//! A host restart is a backend constructed with a boot identity other than
//! the one the stores ran under: the test hook `HookConfig::boot_id_path`
//! points each backend at a file holding the identity it is to see.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use durable_cache::hooks::{FaultPoint, HookConfig};
use durable_cache::recovery::recover;
use durable_cache::{Durability, FsCache, FsCacheConfig};
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::storage::Storage;
use pingora_cache::CacheKey;

use super::support;

// IMP-TC.PERSISTENCE.CRASHDISCARD-01: satisfy TC-PERSISTENCE.CRASHDISCARD-01

/// A file holding the boot identity `id`, in `dir`.
fn boot(dir: &Path, id: &str) -> PathBuf {
    let path = dir.join(id);
    std::fs::write(&path, format!("{id}\n")).expect("the boot identity is written");
    path
}

/// A backend on `root` with stores unsynchronised, the discard set as
/// `discard`, and the boot identity read from `boot_id`.
fn unsynced(root: &Path, discard: bool, boot_id: &Path) -> &'static FsCache {
    backend(root, Durability::NoStoreSync, discard, boot_id)
}

fn backend(root: &Path, durability: Durability, discard: bool, boot_id: &Path) -> &'static FsCache {
    let config = FsCacheConfig::new(root, None)
        .with_durability(durability)
        .with_discard_after_host_restart(discard);
    support::leaked_cache_configured(
        config,
        HookConfig {
            boot_id_path: Some(boot_id.to_path_buf()),
            ..HookConfig::default()
        },
    )
}

/// The unsynchronised-store markers under `root`, by file name.
fn markers(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join("writers"))
        .expect("the lease directory exists")
        .map(|e| {
            e.expect("an entry can be read")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .filter(|name| name.ends_with(".unsynced"))
        .collect();
    names.sort();
    names
}

/// What a host restart does to `cache`'s instance: it stops, and its lease
/// is no longer held. The test's backends are leaked, so the lease file is
/// removed instead, which the liveness check reads the same way.
fn stop(cache: &FsCache) {
    std::fs::remove_file(support::lease_path(cache.root(), &cache.lease.id()))
        .expect("the instance's lease is there");
}

/// Whether a directory of discarded shards is left under `root`.
fn discarded_left(root: &Path) -> bool {
    std::fs::read_dir(root)
        .expect("the root can be listed")
        .any(|e| {
            e.expect("an entry can be read")
                .file_name()
                .to_string_lossy()
                .starts_with("discarded-")
        })
}

/// Store `count` objects named after `prefix`; their keys and bodies.
async fn store_some(
    cache: &'static FsCache,
    prefix: &str,
    count: usize,
) -> Vec<(CacheKey, Vec<u8>)> {
    let mut stored = Vec::new();
    for i in 0..count {
        let key = support::key(&format!("/{prefix}-{i}"));
        let body = support::incompressible(1000 + 700 * i, i as u64).into_bytes();
        support::store(cache, &key, &support::meta(), &body).await;
        stored.push((key, body));
    }
    stored
}

/// Every object in `stored` is a hit on `cache`, served as stored.
async fn assert_all_hits(cache: &'static FsCache, stored: &[(CacheKey, Vec<u8>)], step: &str) {
    for (key, body) in stored {
        let (_meta, got) = support::lookup_body(cache, key)
            .await
            .unwrap_or_else(|| panic!("{step}: a stored object is a hit"));
        assert_eq!(&got, body, "{step}: served as stored");
    }
}

/// Steps 1 to 3: unsynchronised stores write the marker; a process restart
/// keeps them; a host restart the cache was not flushed for discards them,
/// with the in-flight store's temporary, and leaves no marker and, once the
/// background removal ends, no discarded tree.
#[tokio::test]
async fn tc_persistence_crashdiscard_01_a_host_restart_discards_unflushed_stores() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let (boot_a, boot_b) = (boot(boots.path(), "boot-a"), boot(boots.path(), "boot-b"));

    // Step 1: every synchronisation of a temporary fails, so a store that
    // reached one would fail.
    let first = unsynced(tmp.path(), true, &boot_a);
    first.hooks().inject_fault(FaultPoint::SyncTemp, 1000);
    let stored = store_some(first, "discard", 6).await;
    let trace = support::inactive_trace();
    let unfinished_key = support::key("/discard-unfinished");
    let mut unfinished = first
        .get_miss_handler(&unfinished_key, &support::meta(), &trace)
        .await
        .expect("a store opens");
    unfinished
        .write_body(Bytes::from_static(b"never finished"), false)
        .await
        .expect("a chunk is written");
    assert_eq!(
        markers(tmp.path()),
        [format!("{}.unsynced", first.lease.id())],
        "step 1: the instance's marker"
    );
    let marker = tmp
        .path()
        .join("writers")
        .join(format!("{}.unsynced", first.lease.id()));
    assert_eq!(
        std::fs::read_to_string(&marker).expect("the marker reads"),
        "boot-a\n",
        "step 1: the marker names the boot"
    );

    // Step 2: a process restart, under the same boot.
    let second = unsynced(tmp.path(), true, &boot_a);
    assert_all_hits(second, &stored, "step 2").await;
    assert_eq!(markers(tmp.path()).len(), 1, "step 2: the marker stays");

    // Step 3: a host restart, which stopped both instances.
    stop(first);
    stop(second);
    let third = unsynced(tmp.path(), true, &boot_b);
    for (key, _) in &stored {
        assert!(
            support::lookup_body(third, key).await.is_none(),
            "step 3: every key is a miss"
        );
    }
    assert!(markers(tmp.path()).is_empty(), "step 3: no marker remains");
    let report = recover(third, &Manager::new(usize::MAX))
        .await
        .expect("the restart scan runs");
    assert_eq!(report.admitted, 0, "step 3: the restart scan finds nothing");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while discarded_left(tmp.path()) {
        assert!(
            std::time::Instant::now() < deadline,
            "step 3: the discarded tree is removed in the background"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(unfinished);
}

/// Step 4: once flushed, a cache keeps what it stored across a host restart.
#[tokio::test]
async fn tc_persistence_crashdiscard_01_a_flushed_cache_survives_a_host_restart() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let cache = unsynced(tmp.path(), true, &boot(boots.path(), "boot-c"));
    let stored = store_some(cache, "flushed", 4).await;
    assert_eq!(markers(tmp.path()).len(), 1, "the stores wrote the marker");
    cache.flush().expect("the cache flushes");
    assert!(
        markers(tmp.path()).is_empty(),
        "the flush removed the marker"
    );

    stop(cache);
    let restarted = unsynced(tmp.path(), true, &boot(boots.path(), "boot-d"));
    assert_all_hits(restarted, &stored, "step 4").await;

    // A store after the flush writes the marker again.
    store_some(cache, "after-flush", 1).await;
    assert_eq!(
        markers(tmp.path()).len(),
        1,
        "a later store writes the marker again"
    );
}

/// Step 4, through the hot-upgrade helper: it flushes the cache once it has
/// made the backend read-only, and once the server announces the end of a
/// shutdown's grace period (FDS-PERSISTENCE.CRASHDISCARD-01).
#[tokio::test]
async fn tc_persistence_crashdiscard_01_the_helper_flushes_on_upgrade_and_shutdown() {
    use durable_cache::hotupgrade::UpgradeWatcher;
    use pingora_core::server::ExecutionPhase;
    use pingora_core::services::background::BackgroundService;

    let boots = support::tempdir();
    for phase in [
        ExecutionPhase::GracefulUpgradeTransferringFds,
        ExecutionPhase::ShutdownRuntimes,
    ] {
        let tmp = support::tempdir();
        let cache = unsynced(tmp.path(), true, &boot(boots.path(), "boot-e"));
        let stored = store_some(cache, "helper", 3).await;
        assert_eq!(markers(tmp.path()).len(), 1, "{phase:?}");

        let (phases, receiver) = tokio::sync::broadcast::channel(4);
        let watcher = UpgradeWatcher::watching(cache, receiver);
        let (_shutdown, shutdown_watch) = tokio::sync::watch::channel(false);
        let watching = tokio::spawn(async move { watcher.start(shutdown_watch).await });
        let name = format!("{phase:?}");
        phases.send(phase).expect("the watcher listens");
        phases
            .send(ExecutionPhase::ShutdownRuntimes)
            .expect("the watcher listens");
        tokio::time::timeout(Duration::from_secs(10), watching)
            .await
            .expect("the watcher returns at the end of the shutdown")
            .expect("the watcher does not panic");
        assert!(markers(tmp.path()).is_empty(), "{name}: the marker is gone");

        stop(cache);
        let restarted = unsynced(tmp.path(), true, &boot(boots.path(), "boot-f"));
        assert_all_hits(restarted, &stored, "after the helper flushed").await;
    }
}

/// The marker is written once, however many unsynchronised stores reach
/// it together (FDS-PERSISTENCE.CRASHDISCARD-01, "The marker").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_persistence_crashdiscard_01_concurrent_stores_write_the_marker_once() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let cache = unsynced(tmp.path(), true, &boot(boots.path(), "boot-k"));
    let stores: Vec<_> = (0..16)
        .map(|i| {
            tokio::spawn(async move {
                let key = support::key(&format!("/concurrent-{i}"));
                support::store(cache, &key, &support::meta(), b"stored together").await;
            })
        })
        .collect();
    for store in stores {
        store.await.expect("a store completes");
    }
    assert_eq!(cache.hooks().marker_writes(), 1, "one marker write");
    assert_eq!(markers(tmp.path()).len(), 1);
}

/// A marker whose instance still runs is never taken for another boot's,
/// even by a backend that cannot read the boot identity: that instance was
/// started in this boot, and its stores are in the page cache.
#[tokio::test]
async fn tc_persistence_crashdiscard_01_a_running_instances_marker_is_never_discarded() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let running = unsynced(tmp.path(), true, &boot(boots.path(), "boot-l"));
    let stored = store_some(running, "running", 3).await;
    assert_eq!(markers(tmp.path()).len(), 1);

    let without_boot_id = unsynced(tmp.path(), false, &boots.path().join("no-boot-id"));
    assert_all_hits(without_boot_id, &stored, "beside a running instance").await;
    assert_eq!(
        markers(tmp.path()).len(),
        1,
        "the running instance's marker stays"
    );
}

/// Step 5: strict stores write no marker, so a host restart keeps them.
#[tokio::test]
async fn tc_persistence_crashdiscard_01_strict_stores_write_no_marker() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let cache = backend(
        tmp.path(),
        Durability::Strict,
        true,
        &boot(boots.path(), "boot-g"),
    );
    let stored = store_some(cache, "strict", 3).await;
    assert!(markers(tmp.path()).is_empty(), "step 5: no marker");

    let restarted = unsynced(tmp.path(), true, &boot(boots.path(), "boot-h"));
    assert_all_hits(restarted, &stored, "step 5").await;
}

/// Step 6: with the discard on, a boot identity that cannot be read fails
/// construction, with an error naming where it was to be read.
#[test]
fn tc_persistence_crashdiscard_01_an_unreadable_boot_identity_fails_construction() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let missing = boots.path().join("no-boot-id");
    let config = FsCacheConfig::new(tmp.path(), None)
        .with_durability(Durability::NoStoreSync)
        .with_discard_after_host_restart(true);
    let hooks = HookConfig {
        boot_id_path: Some(missing.clone()),
        ..HookConfig::default()
    };
    let e = match FsCache::with_test_hooks(config, hooks) {
        Ok(_) => panic!("step 6: construction fails"),
        Err(e) => e,
    };
    assert!(
        e.to_string().contains(&missing.display().to_string()),
        "step 6: the error names the boot identity's source: {e}"
    );
}

/// Step 7: with the discard off, no marker is written, and after a host
/// restart a record its crash left short is a miss, every other one a hit.
#[tokio::test]
async fn tc_persistence_crashdiscard_01_without_the_discard_a_short_record_is_a_miss() {
    let tmp = support::tempdir();
    let boots = support::tempdir();
    let cache = unsynced(tmp.path(), false, &boot(boots.path(), "boot-i"));
    let stored = store_some(cache, "no-discard", 4).await;
    assert!(markers(tmp.path()).is_empty(), "step 7: no marker");
    let (short_key, _) = &stored[2];
    support::truncate_by(&support::canonical_path(cache, short_key), 100);

    stop(cache);
    let restarted = unsynced(tmp.path(), false, &boot(boots.path(), "boot-j"));
    for (key, body) in &stored {
        let found = support::lookup_body(restarted, key).await;
        if key.to_compact() == short_key.to_compact() {
            assert!(found.is_none(), "step 7: the short record is a miss");
        } else {
            let (_meta, got) = found.expect("step 7: every other object is a hit");
            assert_eq!(&got, body, "step 7: served as stored");
        }
    }
}
