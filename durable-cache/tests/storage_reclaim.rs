// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.RECLAIM-01 — Orphan reclaim at restart.
//!
//! The restart-time scan reclaims a temporary exactly when its writer is
//! not running, and a damaged, foreign or withdrawn record — never
//! admitting what it reclaims, never reclaiming a `Vary` variant, whatever
//! its primary holds, never unlinking a record a live store has just
//! replaced, and reporting what it reclaimed.

use super::support;

use durable_cache::hooks::PausePoint;
use durable_cache::recovery::recover;
use durable_cache::writerid::WriterLease;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::Storage;
use std::time::{Duration, SystemTime};

// IMP-TC.STORAGE.RECLAIM-01: satisfy TC-STORAGE.RECLAIM-01

/// Steps 1 and 2: a temporary is reclaimed exactly when its writer is not
/// running — its lease missing, or present but no longer locked, as a crash
/// leaves it — and one whose writer is running is left alone until it stops.
/// A stopped writer's lease goes with its temporaries.
#[tokio::test]
async fn tc_storage_reclaim_01_temporaries_follow_writer_liveness() {
    let (cache, _tmp) = support::leaked_cache();
    let hash = "abcd0000000000000000000000000000";

    let no_lease = support::forge_temp(cache, hash, &support::writer_without_lease(), 1);
    // A writer that crashed: its lease is still there, and no one holds it.
    let crashed_id = WriterLease::acquire(cache.root()).unwrap().id();
    let crashed = support::forge_temp(cache, hash, &crashed_id, 2);
    // A live peer: a lease this test holds, as another instance would.
    let peer = WriterLease::acquire(cache.root()).unwrap();
    let peer_id = peer.id();
    let running = support::forge_temp(cache, hash, &peer_id, 3);
    let mine = support::forge_temp(cache, hash, &cache.lease.id(), 4);

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.unwrap();

    assert_eq!(
        report.reclaimed_temps, 2,
        "the two stopped writers' temporaries"
    );
    assert!(!no_lease.exists());
    assert!(!crashed.exists());
    assert!(
        !support::lease_path(cache.root(), &crashed_id).exists(),
        "and the stopped writer's lease"
    );
    assert!(running.exists(), "a live peer's temporary is left alone");
    assert!(support::lease_path(cache.root(), &peer_id).exists());
    assert!(mine.exists(), "this instance's own temporary is left alone");
    assert!(support::lease_path(cache.root(), &cache.lease.id()).exists());
    assert_eq!(
        manager.total_items(),
        0,
        "nothing reclaimed is ever admitted"
    );

    // Once that writer stops, a later scan reclaims its temporary and lease.
    drop(peer);
    let report = recover(cache, &manager).await.unwrap();
    assert_eq!(report.reclaimed_temps, 1);
    assert!(!running.exists());
    assert!(!support::lease_path(cache.root(), &peer_id).exists());
    assert!(mine.exists());
}

/// Step 3: a damaged record, a foreign-tagged one and a withdrawn one are
/// all reclaimed, the withdrawn one under its own class, and none is
/// admitted.
#[tokio::test]
async fn tc_storage_reclaim_01_damaged_foreign_and_withdrawn_records_are_reclaimed() {
    let (cache, _tmp) = support::leaked_cache();
    let damaged = support::key("/reclaim-damaged");
    let foreign = support::key("/reclaim-foreign");
    let withdrawn = support::key("/reclaim-withdrawn");
    let intact = support::key("/reclaim-intact");
    support::store(cache, &damaged, &support::meta(), b"damaged body").await;
    support::store(cache, &foreign, &support::meta(), b"foreign body").await;
    support::store(cache, &withdrawn, &support::meta(), b"withdrawn body").await;
    support::store(cache, &intact, &support::meta(), b"intact body").await;

    support::flip_magic_byte(&support::canonical_path(cache, &damaged));
    support::set_format_version(
        &support::canonical_path(cache, &foreign),
        durable_cache::format::FORMAT_VERSION + 1,
    );
    // Resealed, as a metadata update's withdrawal leaves it.
    support::set_format_version(
        &support::canonical_path(cache, &withdrawn),
        durable_cache::format::WITHDRAWN_VERSION,
    );

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.unwrap();

    assert_eq!(
        report.reclaimed_damaged, 2,
        "the damaged and the foreign record"
    );
    assert!(report.reclaimed_damaged_bytes > 0);
    assert!(!support::canonical_path(cache, &damaged).exists());
    assert!(!support::canonical_path(cache, &foreign).exists());
    assert!(!manager.peek(&support::entry(&damaged)));
    assert!(!manager.peek(&support::entry(&foreign)));
    assert_eq!(
        report.reclaimed_withdrawn, 1,
        "the withdrawn record, under its own class"
    );
    assert!(report.reclaimed_withdrawn_bytes > 0);
    assert!(!support::canonical_path(cache, &withdrawn).exists());
    assert!(!manager.peek(&support::entry(&withdrawn)));
    assert_eq!(report.admitted, 1, "only the intact record");
    assert!(manager.peek(&support::entry(&intact)));
}

/// Step 3, continued: a record found anywhere but at its own key's path is
/// reclaimed as damaged, and not admitted: one key's record at another
/// key's path, and a record under its own name in a shard directory that
/// name does not select. The records at their own paths are admitted.
#[tokio::test]
async fn tc_storage_reclaim_01_misplaced_records_are_reclaimed() {
    let (cache, _tmp) = support::leaked_cache();
    let moved = support::key("/reclaim-moved");
    let strayed = support::key("/reclaim-strayed");
    let unstored = support::key("/reclaim-unstored");
    for key in [&moved, &strayed] {
        support::store(cache, key, &support::meta(), b"body").await;
    }

    // `moved`'s record, copied to the path of a key that was never stored.
    let at_other_path = support::canonical_path(cache, &unstored);
    std::fs::create_dir_all(at_other_path.parent().expect("a shard directory"))
        .expect("the other key's shard directory is created");
    std::fs::copy(support::canonical_path(cache, &moved), &at_other_path)
        .expect("the record is copied to the other key's path");

    // `strayed`'s record, copied under its own name into the second-level
    // directory next to the one its name selects.
    let own_path = support::canonical_path(cache, &strayed);
    let name = own_path.file_name().expect("a file name");
    let own_dir = own_path.parent().expect("a shard directory");
    let first = own_dir.parent().expect("a first-level directory");
    let second = own_dir
        .file_name()
        .and_then(|s| s.to_str())
        .expect("a second-level directory name");
    let other_second = if second == "00" { "01" } else { "00" };
    let in_other_dir = first.join(other_second).join(name);
    std::fs::create_dir_all(in_other_dir.parent().expect("a shard directory"))
        .expect("the other shard directory is created");
    std::fs::copy(&own_path, &in_other_dir).expect("the record is copied to the other directory");

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.expect("the scan completes");

    assert_eq!(report.reclaimed_damaged, 2, "both misplaced records");
    assert!(!at_other_path.exists());
    assert!(!in_other_dir.exists());
    assert!(!manager.peek(&support::entry(&unstored)));
    assert_eq!(report.admitted, 2, "the two records at their own paths");
    for key in [&moved, &strayed] {
        assert!(support::canonical_path(cache, key).exists());
        assert!(manager.peek(&support::entry(key)));
    }
}

/// Step 4: a variant is admitted like any other record, whatever its
/// primary holds: absent, damaged, created later than the variant, or
/// created no later. Only the damaged primary is reclaimed.
#[tokio::test]
async fn tc_storage_reclaim_01_variants_are_admitted_whatever_their_primary() {
    let (cache, _tmp) = support::leaked_cache();
    let now = SystemTime::now();

    // A: primary removed out of band (a completed purge or eviction).
    let a = support::key("/reclaim-no-primary");
    support::store(cache, &a, &support::meta_created(now), b"primary-a").await;
    let a_variant = support::variant(&a, [0xA1; 16]);
    support::store(cache, &a_variant, &support::meta_created(now), b"variant-a").await;
    std::fs::remove_file(support::canonical_path(cache, &a)).expect("the primary is removed");

    // B: primary damaged.
    let b = support::key("/reclaim-damaged-primary");
    support::store(cache, &b, &support::meta_created(now), b"primary-b").await;
    let b_variant = support::variant(&b, [0xB2; 16]);
    support::store(cache, &b_variant, &support::meta_created(now), b"variant-b").await;
    support::flip_magic_byte(&support::canonical_path(cache, &b));

    // C: primary created later than the variant.
    let c = support::key("/reclaim-older-variant");
    support::store(cache, &c, &support::meta_created(now), b"primary-c").await;
    let c_variant = support::variant(&c, [0xC3; 16]);
    support::store(
        cache,
        &c_variant,
        &support::meta_created(now - Duration::from_secs(10)),
        b"variant-c",
    )
    .await;

    // D: primary created no later than the variant.
    let d = support::key("/reclaim-newer-variant");
    support::store(cache, &d, &support::meta_created(now), b"primary-d").await;
    let d_variant = support::variant(&d, [0xD4; 16]);
    support::store(
        cache,
        &d_variant,
        &support::meta_created(now + Duration::from_secs(10)),
        b"variant-d",
    )
    .await;

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.expect("the scan completes");

    assert_eq!(report.reclaimed_damaged, 1, "only B's damaged primary");
    assert!(!support::canonical_path(cache, &b).exists());
    let admitted = [&c, &d, &a_variant, &b_variant, &c_variant, &d_variant];
    assert_eq!(report.admitted, admitted.len());
    for key in admitted {
        assert!(
            support::canonical_path(cache, key).exists(),
            "{} is kept",
            key.combined()
        );
        assert!(
            manager.peek(&support::entry(key)),
            "{} is admitted",
            key.combined()
        );
    }
    // With the same size a directly stored object would receive.
    assert_eq!(
        manager.total_size(),
        admitted
            .iter()
            .map(|k| std::fs::metadata(support::canonical_path(cache, k))
                .expect("an admitted record")
                .len() as usize)
            .sum::<usize>(),
        "every admitted entry carries its record file's length"
    );
}

/// Step 5: a record a live store publishes while the scan is paused between
/// classifying and unlinking survives the resumed scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_storage_reclaim_01_never_unlinks_a_record_a_live_store_replaced() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/reclaim-race");
    support::store(cache, &key, &support::meta(), b"damaged body").await;
    let path = support::canonical_path(cache, &key);
    support::flip_magic_byte(&path);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::ReclaimBeforeUnlink, Some(&path));
    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        recover(cache, &manager).await.unwrap()
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the scan must reach its pause point"
    );

    // A live store publishes a fresh record at that very path.
    support::store(cache, &key, &support::meta(), b"freshly published").await;
    pause.release();

    let report = scanning.await.unwrap();
    assert_eq!(
        report.reclaimed_damaged, 0,
        "the entry is skipped, not unlinked"
    );
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"freshly published",
        "the live store's record survives the resumed scan"
    );
}

/// Step 5, continued: a record the scan finds in the middle of an in-place
/// metadata update is admitted, not reclaimed. With the update stopped
/// between its two writes, every read the scan makes fails the checksum, so
/// it reads once more holding the key's publication lock, which the update
/// holds until its header is written, and then finds the record whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_storage_reclaim_01_a_record_read_mid_update_is_admitted() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/reclaim-mid-update");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"a body").await;
    let path = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::InPlaceBetweenWrites, Some(&path));
    let updating = {
        let key = key.clone();
        let update = support::revalidation(&stored, &support::meta_with_header_filler(200));
        tokio::spawn(async move {
            cache
                .update_meta(&key, &update, &support::inactive_trace())
                .await
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the update stops between its writes"
    );

    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        let report = recover(cache, &manager).await.expect("the scan completes");
        (report, manager)
    });
    // Were the scan kept off the processor for the whole sleep, this check
    // would pass without testing anything, but it can never fail falsely.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !scanning.is_finished(),
        "the scan waits for the update to finish"
    );

    pause.release();
    assert!(
        updating
            .await
            .expect("the updating task completes")
            .expect("the update succeeds"),
        "the update is applied"
    );
    let (report, manager) = scanning.await.expect("the scanning task completes");
    assert_eq!(report.reclaimed_damaged, 0, "the record is not reclaimed");
    assert!(path.exists());
    assert!(
        manager.peek(&support::entry(&key)),
        "the record is admitted"
    );
}

/// The scan frees a record it reclaims only after releasing the key's
/// publication lock. Paused just before the record is closed, the scan has
/// unlinked it, a store of the key goes through, so the lock is free, and
/// the record is still open, so the kernel has not freed it. Once the scan
/// completes, the record is closed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scan_frees_a_reclaimed_record_after_releasing_the_lock() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/reclaim-then-free");
    support::store(cache, &key, &support::meta(), b"damaged body").await;
    let path = support::canonical_path(cache, &key);
    support::flip_magic_byte(&path);
    let old = support::file_id(&path);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeFreeingOldRecord, Some(&path));
    let scanning = tokio::spawn(async move {
        let manager = Manager::new(usize::MAX);
        recover(cache, &manager).await.expect("the scan completes")
    });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the scan must reach its pause point"
    );

    assert!(!path.exists(), "the record is unlinked");
    assert!(
        support::open_files().contains(&old),
        "the record is still open"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        support::store(cache, &key, &support::meta(), b"stored again"),
    )
    .await
    .expect("the key's lock is free while the record waits to be closed");

    pause.release();
    let report = scanning.await.expect("the scan task completes");
    assert_eq!(report.reclaimed_damaged, 1);
    assert!(
        !support::open_files().contains(&old),
        "the record is closed"
    );
}
