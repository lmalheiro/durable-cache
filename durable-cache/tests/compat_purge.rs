// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.PURGE-01 — Purge makes all variants unreachable.
//!
//! `pingora-cache` addresses a purge to the *primary* slot, and its own
//! `Vary` resolution (`cache_vary_lookup`) consults the primary first
//! (FDS-STORAGE.PURGE-01) — so once the primary is gone, no request can
//! reach a variant again, even while the variant's own file is still
//! physically present. "Never served again" is therefore a joint guarantee
//! of this backend (the primary is durably gone) and that calling
//! convention. The orphaned variant records stay, admitted at a restart like
//! any other record, until eviction removes them (FRE-COMPLIANCE.PURGE-01).
//! This file proves the backend's half, driving `Storage` directly.

use super::support;

use durable_cache::recovery::recover;
use durable_cache::FsCache;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::storage::{PurgeOutcome, Storage};

// IMP-TC.COMPLIANCE.PURGE-01: satisfy TC-COMPLIANCE.PURGE-01

async fn purge(cache: &'static FsCache, key: &pingora_cache::CacheKey) -> bool {
    match support::purge_active(cache, key).await {
        PurgeOutcome::Purged(None) => true,
        PurgeOutcome::NotFound => false,
        other => panic!("this backend assigns no entry identifier, got {other:?}"),
    }
}

/// Steps 1 to 3: the primary is gone after the purge, repeated and
/// never-stored purges are harmless no-ops, and nothing comes back after a
/// restart — clean or after a crash, which for a durable cache is the same
/// path, since nothing is written at shutdown.
#[tokio::test]
async fn tc_compliance_purge_01_primary_gone_and_nothing_returns_after_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let primary = support::key("/purge-all-variants");
    let variants: Vec<_> = (0u8..3)
        .map(|i| support::variant(&primary, [i + 1; 16]))
        .collect();
    support::store(cache, &primary, &support::meta(), b"primary body").await;
    for (i, variant) in variants.iter().enumerate() {
        support::store(
            cache,
            variant,
            &support::meta(),
            format!("variant {i}").as_bytes(),
        )
        .await;
    }

    assert!(purge(cache, &primary).await);
    assert!(
        support::lookup_body(cache, &primary).await.is_none(),
        "the primary slot is empty, so no variant can be reached through it"
    );

    // Steps 3: idempotent, and defined on a key that never existed.
    assert!(!purge(cache, &primary).await);
    assert!(!purge(cache, &support::key("/never-stored")).await);

    // Step 4: a restart brings nothing back.
    let restarted = support::leaked_cache_at(tmp.path());
    assert!(support::lookup_body(restarted, &primary).await.is_none());
    let manager = Manager::new(usize::MAX);
    recover(restarted, &manager).await.unwrap();
    assert!(support::lookup_body(restarted, &primary).await.is_none());
}

/// Step 5: a purge while a reader is mid-stream on a variant, and while a
/// same-key store is in flight: the reader completes, and the store
/// publishes nothing, since the purge ended it (FRE-COMPLIANCE.PURGE-01).
#[tokio::test]
async fn tc_compliance_purge_01_mid_stream_reader_and_racing_store() {
    let (cache, _tmp) = support::leaked_cache();
    let primary = support::key("/purge-midstream");
    let variant = support::variant(&primary, [0x44; 16]);
    let body: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
    support::store(cache, &variant, &support::meta(), &body).await;
    support::store(cache, &primary, &support::meta(), b"primary").await;

    let (_meta, mut hit) = support::lookup_hit(cache, &variant).await;
    let first = hit.read_body().await.unwrap().expect("a first chunk");

    // A store of the primary is in flight when the purge arrives.
    let trace = support::inactive_trace();
    let meta = support::meta();
    let mut storing = cache
        .get_miss_handler(&primary, &meta, &trace)
        .await
        .unwrap();
    storing
        .write_body(bytes::Bytes::from_static(b"republished"), true)
        .await
        .unwrap();

    assert!(purge(cache, &primary).await);
    assert!(purge(cache, &variant).await);

    let mut read = first.to_vec();
    read.extend_from_slice(
        &support::drain_body(&mut hit)
            .await
            .expect("a clean end of body"),
    );
    assert_eq!(
        read, body,
        "the reader receives its own object's bytes, whole"
    );

    storing
        .finish()
        .await
        .err()
        .expect("a store in flight when the purge arrives is ended by it");
    assert!(
        support::lookup_body(cache, &primary).await.is_none(),
        "so nothing is served under the purged key"
    );
}

/// Step 6: after a restart, the variant records the purge orphaned are
/// still there, each admitted with its record size, so that eviction
/// removes them like any other record (FRE-COMPLIANCE.PURGE-01).
#[tokio::test]
async fn tc_compliance_purge_01_orphaned_variants_are_left_to_eviction() {
    let (cache, tmp) = support::leaked_cache();
    let primary = support::key("/purge-orphans");
    let variants: Vec<_> = (0u8..3)
        .map(|i| support::variant(&primary, [i + 10; 16]))
        .collect();
    support::store(cache, &primary, &support::meta(), b"primary").await;
    for variant in &variants {
        support::store(cache, variant, &support::meta(), b"variant body").await;
    }
    assert!(purge(cache, &primary).await);
    for variant in &variants {
        assert!(
            support::canonical_path(cache, variant).exists(),
            "the purge itself cannot reach a variant's file"
        );
    }

    let restarted = support::leaked_cache_at(tmp.path());
    let manager = Manager::new(usize::MAX);
    let report = recover(restarted, &manager)
        .await
        .expect("the scan completes");

    assert_eq!(
        report.admitted,
        variants.len(),
        "the variants, and nothing else"
    );
    for variant in &variants {
        assert!(
            support::canonical_path(restarted, variant).exists(),
            "every orphaned variant record stays"
        );
        assert!(manager.peek(&support::entry(variant)), "and is admitted");
    }
    assert_eq!(
        manager.total_size() as u64,
        variants
            .iter()
            .map(|v| std::fs::metadata(support::canonical_path(restarted, v))
                .expect("a variant record")
                .len())
            .sum::<u64>(),
        "with its record size"
    );
}
