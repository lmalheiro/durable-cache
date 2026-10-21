// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.EVICTION-01 — Exact eviction state after restart.
//!
//! Once the backend reports its restart procedure complete, the eviction
//! manager's view is consistent with the durably published objects: each
//! represented once with its record size and freshness deadline, nothing
//! left for what is no longer there, and the totals equal to the published
//! set's count and aggregate record size.
//!
//! Overlaps mechanically with `eviction_reconstruct.rs` (both populate,
//! "restart", and check totals) but verifies a different requirement: this
//! one is FRE-COMPAT.EVICTION-01 (the functional requirement) rather than
//! FDS-EVICTION.STARTUP-01 (the design).

use super::support;

use durable_cache::recovery::recover;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::storage::Storage;
use std::time::{Duration, SystemTime};

// IMP-TC.COMPAT.EVICTION-01: satisfy TC-COMPAT.EVICTION-01

/// Steps 1 to 5: a populated cache, plus every kind of entry that must not
/// be admitted, is restarted; the manager's per-entry view and totals match
/// the published objects exactly, and a store completed afterwards is
/// admitted with the same size definition.
#[tokio::test]
async fn tc_compat_eviction_01_exact_eviction_state_after_restart() {
    let (cache, tmp) = support::leaked_cache();
    let now = SystemTime::now();

    // Step 1: objects of very different sizes, and Vary variants.
    let mut kept: Vec<pingora_cache::CacheKey> = Vec::new();
    for (i, size) in [1usize, 100, 100_000].iter().enumerate() {
        let key = support::key(&format!("/eviction-keep-{i}"));
        support::store(cache, &key, &support::meta_created(now), &vec![b'k'; *size]).await;
        kept.push(key);
    }
    let variant = support::variant(&kept[0], [0x21; 16]);
    support::store(
        cache,
        &variant,
        &support::meta_created(now),
        b"variant body",
    )
    .await;
    kept.push(variant);
    let orphaned_primary = support::key("/eviction-orphaned-primary");
    support::store(
        cache,
        &orphaned_primary,
        &support::meta_created(now),
        b"primary",
    )
    .await;
    let orphaned_variant = support::variant(&orphaned_primary, [0x22; 16]);
    support::store(
        cache,
        &orphaned_variant,
        &support::meta_created(now),
        b"orphaned variant",
    )
    .await;
    kept.push(orphaned_variant);

    // Step 2: everything that must *not* be admitted.
    // (a) an object purged during the previous run,
    let purged = support::key("/eviction-purged");
    support::store(cache, &purged, &support::meta(), b"gone").await;
    support::purge_active(cache, &purged).await;
    // (b) a record damaged out of band,
    let damaged = support::key("/eviction-damaged");
    support::store(cache, &damaged, &support::meta(), b"damaged").await;
    support::flip_magic_byte(&support::canonical_path(cache, &damaged));
    // (c) a crash-orphaned per-writer temporary,
    let orphan_temp = support::forge_temp(
        cache,
        "eeee0000000000000000000000000000",
        &support::writer_without_lease(),
        1,
    );
    // (d) a variant's primary, removed. The variant stays, and is admitted
    // like any other record.
    std::fs::remove_file(support::canonical_path(cache, &orphaned_primary))
        .expect("the primary is removed");

    // Step 3: restart, and wait for the procedure to report complete.
    let restarted = support::leaked_cache_at(tmp.path());
    let manager = Manager::new(usize::MAX);
    let report = recover(restarted, &manager).await.unwrap();
    assert_eq!(report.admitted, kept.len());

    // Step 4: the per-entry view and the totals.
    let mut expected_size = 0u64;
    for key in &kept {
        let path = support::canonical_path(restarted, key);
        let len = std::fs::metadata(&path).unwrap().len();
        expected_size += len;
        assert!(
            manager.peek(&support::entry(key)),
            "every durably published object is represented"
        );
    }
    assert_eq!(
        manager.total_items(),
        kept.len(),
        "represented exactly once each"
    );
    assert_eq!(
        manager.total_size() as u64,
        expected_size,
        "each entry's size is the length of the file that stores the object"
    );
    for absent in [&purged, &damaged, &orphaned_primary] {
        assert!(
            !manager.peek(&support::entry(absent)),
            "no entry may exist for a purged, damaged, or removed object"
        );
    }
    assert!(
        !orphan_temp.exists(),
        "the crash-orphaned temporary is reclaimed"
    );

    // Step 5: a store completed after the restart reports the same size.
    let fresh = support::key("/eviction-after-restart");
    let reported = support::store(restarted, &fresh, &support::meta(), b"stored after").await;
    let on_disk = std::fs::metadata(support::canonical_path(restarted, &fresh))
        .unwrap()
        .len();
    assert_eq!(
        reported as u64, on_disk,
        "a live store is admitted with the record size the scan also uses"
    );
}

/// The freshness deadline the scan admits is each object's own, not a value
/// invented at restart: the manager evicts the object whose deadline has
/// passed before the one whose has not.
#[tokio::test]
async fn tc_compat_eviction_01_admits_each_objects_own_freshness_deadline() {
    let (cache, tmp) = support::leaked_cache();
    let body = vec![b'e'; 10_000];

    // One object already past its freshness deadline, one comfortably
    // inside it, both the same size and stored in this order.
    let stale = support::key("/eviction-stale");
    let expired = pingora_cache::CacheMeta::new(
        SystemTime::now() - Duration::from_secs(60),
        SystemTime::now() - Duration::from_secs(120),
        0,
        0,
        pingora_http::ResponseHeader::build(200, None).unwrap(),
    );
    support::store(cache, &stale, &expired, &body).await;
    let fresh = support::key("/eviction-fresh");
    support::store(cache, &fresh, &support::meta(), &body).await;

    let restarted = support::leaked_cache_at(tmp.path());
    let record_size = std::fs::metadata(support::canonical_path(restarted, &fresh))
        .unwrap()
        .len() as usize;
    // A budget with room for both, but not for a third object.
    let manager = Manager::new(record_size * 2 + record_size / 2);
    recover(restarted, &manager).await.unwrap();
    assert_eq!(manager.total_items(), 2);

    // Admitting a third forces a choice; `simple_lru` evicts what is
    // already stale first, which it can only know from the deadline the
    // scan admitted with.
    let newcomer = support::key("/eviction-newcomer");
    let victims = manager.admit(
        support::entry(&newcomer),
        record_size,
        support::meta().fresh_until(),
    );
    assert_eq!(victims.len(), 1, "exactly one object makes room");
    assert_eq!(
        victims[0],
        support::entry(&stale),
        "the object whose freshness deadline had passed is the victim, so that \
         deadline survived the restart"
    );
}

/// A hit taken before Eviction Recovery has reached its object reports the
/// record size as its eviction weight, so a manager that does not yet know
/// the object tracks it at its real size rather than at zero.
#[tokio::test]
async fn tc_compat_eviction_01_a_hit_reports_its_record_size_as_its_weight() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/eviction-weight");
    let reported = support::store(cache, &key, &support::meta(), &vec![b'w'; 5000]).await;

    let (_meta, hit) = support::lookup_hit(cache, &key).await;
    assert_eq!(
        hit.get_eviction_weight(),
        reported,
        "the weight a hit reports is the record size a store reports"
    );

    // A partial hit is admitted by its own store's finish, so it carries no
    // weight of its own.
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut storing = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    storing
        .write_body(bytes::Bytes::from_static(b"in flight"), false)
        .await
        .unwrap();
    let (_meta, partial) = support::lookup_hit(cache, &key).await;
    assert_eq!(partial.get_eviction_weight(), 0);
}
