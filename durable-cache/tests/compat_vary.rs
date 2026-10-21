// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.VARY-01 — Vary variants stored and selected.
//!
//! `pingora-cache` folds the request's `Vary`-named fields into the cache
//! key's variance *before* ever calling this backend (FRE-COMPLIANCE.VARY-01:
//! "the backend never sees the request itself") -- so the backend's own
//! obligation is entirely about the key it is actually given: store and
//! return exactly the object at that key, primary or variant, never
//! conflating one with another. That's fully exercisable directly against
//! `Storage` with pre-computed variance-augmented keys, the same technique
//! `storage_reclaim.rs`'s variant cases already established -- no HTTP
//! needed.

// IMP-TC.COMPLIANCE.VARY-01: satisfy TC-COMPLIANCE.VARY-01
use super::support;

use bytes::Bytes;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::{CacheKey, CacheMeta};

fn variant_key(base: &CacheKey, variance: [u8; 16]) -> CacheKey {
    let mut k = base.clone();
    k.set_variance_key(variance);
    k
}

/// Store `body` under `key`, and return the metadata stored with it.
async fn store(cache: &'static FsCache, key: &CacheKey, body: &'static [u8]) -> CacheMeta {
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(body), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();
    meta
}

async fn read_body(cache: &'static FsCache, key: &CacheKey) -> Option<Vec<u8>> {
    let trace = support::inactive_trace();
    let (_meta, mut hit) = cache.lookup(key, &trace).await.unwrap()?;
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

#[tokio::test]
async fn tc_compliance_vary_01_each_variant_selected_independently() {
    let (cache, _tmp) = support::leaked_cache();
    let base = support::key("/vary-basic");
    let variant_a = variant_key(&base, [0xAA; 16]);
    let variant_b = variant_key(&base, [0xBB; 16]);
    let variant_unstored = variant_key(&base, [0xCC; 16]);

    store(cache, &variant_a, b"variant-a-body").await;
    store(cache, &variant_b, b"variant-b-body").await;

    assert_eq!(
        read_body(cache, &variant_a).await,
        Some(b"variant-a-body".to_vec())
    );
    assert_eq!(
        read_body(cache, &variant_b).await,
        Some(b"variant-b-body".to_vec())
    );
    assert_eq!(
        read_body(cache, &variant_unstored).await,
        None,
        "a variance value that was never stored must report a miss, not \
         conflate with a stored variant"
    );
}

#[tokio::test]
async fn tc_compliance_vary_01_primary_and_variant_slots_never_conflated() {
    let (cache, _tmp) = support::leaked_cache();
    let base = support::key("/vary-primary-vs-variant");
    let variant = variant_key(&base, [0xDD; 16]);

    store(cache, &base, b"primary-body").await;
    store(cache, &variant, b"variant-body").await;

    assert_eq!(
        read_body(cache, &base).await,
        Some(b"primary-body".to_vec())
    );
    assert_eq!(
        read_body(cache, &variant).await,
        Some(b"variant-body".to_vec()),
        "the primary and its variant occupy distinct slots and must not shadow each other"
    );
}

#[tokio::test]
async fn tc_compliance_vary_01_mutating_one_variant_leaves_the_other_untouched() {
    let (cache, _tmp) = support::leaked_cache();
    let base = support::key("/vary-mutate-isolated");
    let variant_a = variant_key(&base, [0x11; 16]);
    let variant_b = variant_key(&base, [0x22; 16]);

    store(cache, &variant_a, b"a-original").await;
    let stored_b = store(cache, &variant_b, b"b-original").await;

    // Replace variant A with a newer response...
    store(cache, &variant_a, b"a-replaced").await;
    // ...and apply a metadata update to variant B.
    let trace = support::inactive_trace();
    cache
        .update_meta(
            &variant_b,
            &support::revalidation(&stored_b, &support::meta()),
            &trace,
        )
        .await
        .unwrap();

    assert_eq!(
        read_body(cache, &variant_a).await,
        Some(b"a-replaced".to_vec())
    );
    assert_eq!(
        read_body(cache, &variant_b).await,
        Some(b"b-original".to_vec()),
        "a metadata update on B must never touch A's body, and vice versa"
    );
}

#[tokio::test]
async fn tc_compliance_vary_01_full_variant_set_survives_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let base = support::key("/vary-restart");
    let variant_a = variant_key(&base, [0x33; 16]);
    let variant_b = variant_key(&base, [0x44; 16]);

    store(cache, &base, b"primary-body").await;
    store(cache, &variant_a, b"a-body").await;
    store(cache, &variant_b, b"b-body").await;

    let restarted = FsCache::new(FsCacheConfig::new(tmp.path(), None)).unwrap();
    let restarted: &'static FsCache = Box::leak(Box::new(restarted));

    assert_eq!(
        read_body(restarted, &base).await,
        Some(b"primary-body".to_vec())
    );
    assert_eq!(
        read_body(restarted, &variant_a).await,
        Some(b"a-body".to_vec())
    );
    assert_eq!(
        read_body(restarted, &variant_b).await,
        Some(b"b-body".to_vec())
    );
}

/// Step 3, continued: purging one variant through its own
/// variance-augmented key leaves the other variants and the primary alone
/// (FRE-COMPLIANCE.VARYISOLATION-01).
#[tokio::test]
async fn tc_compliance_vary_01_purging_one_variants_own_key_leaves_the_others() {
    let (cache, _tmp) = support::leaked_cache();
    let primary = support::key("/vary-purge-one");
    let first = support::variant(&primary, [0x61; 16]);
    let second = support::variant(&primary, [0x62; 16]);
    support::store(cache, &primary, &support::meta(), b"primary body").await;
    support::store(cache, &first, &support::meta(), b"first variant").await;
    support::store(cache, &second, &support::meta(), b"second variant").await;

    assert!(matches!(
        support::purge_active(cache, &first).await,
        pingora_cache::storage::PurgeOutcome::Purged(None)
    ));

    assert!(support::lookup_body(cache, &first).await.is_none());
    assert_eq!(
        support::lookup_body(cache, &second).await.unwrap().1,
        b"second variant",
        "the other variant is untouched"
    );
    assert_eq!(
        support::lookup_body(cache, &primary).await.unwrap().1,
        b"primary body",
        "and so is the primary"
    );
}
