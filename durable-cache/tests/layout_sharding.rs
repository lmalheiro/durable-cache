// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-LAYOUT.SHARDING-01 — Shard layout and directory bound.
//!
//! The path derived from an object's full `CacheKey` (used on lookup and
//! store) is identical to the one derived from its `CompactCacheKey` (used
//! on purge), for keys with and without a variance; the two-level split
//! always falls on hex characters one-two and three-four of the name; the
//! layout keeps every directory within the bound REQ-EFFICIENCY.DIRSCALE-01
//! sets; and keys that differ only in their user tag are separate objects.
//!
//! The exhaustive boundary-splitting checks on the path helpers themselves
//! live as unit tests in `src/sharding.rs`.

use super::support;

use durable_cache::hooks::HookConfig;
use durable_cache::recovery::recover;
use durable_cache::sharding::{object_name, shard_path, SECOND_LEVEL_COUNT, SHARD_COUNT};
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::key::{CacheHashKey, CacheKey};
use pingora_cache::storage::PurgeOutcome;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

// IMP-TC.LAYOUT.SHARDING-01: satisfy TC-LAYOUT.SHARDING-01

/// Steps 1 to 3: a store and a purge of the same object resolve to the same
/// file, for a primary key and for a variant.
#[tokio::test]
async fn tc_layout_sharding_01_store_and_purge_resolve_to_the_same_file() {
    let (cache, _tmp) = support::leaked_cache();

    for (name, key) in [
        ("primary", support::key("/sharding-roundtrip")),
        (
            "variant",
            support::variant(&support::key("/sharding-roundtrip"), [0x5C; 16]),
        ),
    ] {
        support::store(cache, &key, &support::meta(), b"body").await;
        let stored_at = support::canonical_path(cache, &key);
        assert!(
            stored_at.exists(),
            "the {name} store resolved to {stored_at:?}"
        );

        // The purge sees only the compact key, wrapped in the Active
        // target `HttpCache` issues an invalidation with.
        assert_eq!(
            support::purge_active(cache, &key).await,
            PurgeOutcome::Purged(None)
        );
        assert!(
            !stored_at.exists(),
            "the {name} purge must resolve to the very file the store wrote"
        );
    }
}

/// The same agreement at the path level, for a key form with a variance and
/// one without.
#[test]
fn tc_layout_sharding_01_cachekey_and_compactcachekey_agree() {
    let root = Path::new("/cache");
    // `CacheKey::new` takes no namespace: a caller combining a namespace
    // with a path frames them itself, as this key does.
    let plain = CacheKey::new("ns/some/path", "tenant");
    let mut varianced = plain.clone();
    varianced.set_variance_key([0x9E; 16]);

    for key in [plain, varianced] {
        let compact = key.to_compact();
        assert_eq!(
            shard_path(root, &object_name(key.combined_bin(), key.user_tag())).unwrap(),
            shard_path(
                root,
                &object_name(compact.combined_bin(), compact.user_tag())
            )
            .unwrap(),
            "lookup/store (CacheKey) and purge (CompactCacheKey) must resolve to one path"
        );
    }
}

/// Step 7: keys for one URL that differ only in their user tag are separate
/// objects. Each has a file of its own, each lookup is answered with its own
/// body, a restart admits one eviction entry per tag, and purging one leaves
/// the other.
#[tokio::test]
async fn tc_layout_sharding_01_keys_differing_only_in_their_tag_are_separate_objects() {
    let (cache, tmp) = support::leaked_cache();
    let a = support::tagged_key("/one-url", "tenant-a");
    let b = support::tagged_key("/one-url", "tenant-b");
    support::store(cache, &a, &support::meta(), b"body for a").await;
    support::store(cache, &b, &support::meta(), b"body for b").await;

    let (path_a, path_b) = (
        support::canonical_path(cache, &a),
        support::canonical_path(cache, &b),
    );
    assert_ne!(path_a, path_b, "a file of its own per tag");
    assert!(path_a.exists() && path_b.exists());
    assert_eq!(
        support::lookup_body(cache, &a).await.unwrap().1,
        b"body for a"
    );
    assert_eq!(
        support::lookup_body(cache, &b).await.unwrap().1,
        b"body for b"
    );

    let manager = Manager::new(usize::MAX);
    let report = recover(support::leaked_cache_at(tmp.path()), &manager)
        .await
        .unwrap();
    assert_eq!(report.admitted, 2, "one eviction entry per tag");
    assert!(manager.peek(&support::entry(&a)));
    assert!(manager.peek(&support::entry(&b)));

    assert_eq!(
        support::purge_active(cache, &a).await,
        PurgeOutcome::Purged(None)
    );
    assert!(support::lookup_body(cache, &a).await.is_none());
    assert_eq!(
        support::lookup_body(cache, &b).await.unwrap().1,
        b"body for b",
        "purging one tag's object leaves the other's"
    );
}

/// Step 4: with the hash hook choosing combined hashes that probe the split
/// boundary, the first-level directory is always exactly characters one and
/// two, and the second-level exactly three and four — for both key forms.
#[tokio::test]
async fn tc_layout_sharding_01_split_falls_on_the_first_four_hex_characters() {
    // A hash whose first four characters repeat, and one whose second and
    // third differ sharply, are where an off-by-one in the split would show.
    for hash in [
        "aaaa1111111111111111111111111111",
        "0aa0ffffffffffffffffffffffffffff",
        "ff00000000000000000000000000000f",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let chosen: Arc<str> = Arc::from(hash);
        let cache: &'static FsCache = support::leaked_cache_configured(
            FsCacheConfig::new(tmp.path(), None),
            HookConfig {
                object_name: Some(Arc::new(move |_: &str| chosen.to_string())),
            },
        );

        let key = support::key("/sharding-hook");
        support::store(cache, &key, &support::meta(), b"body").await;

        let expected = tmp.path().join(&hash[0..2]).join(&hash[2..4]).join(hash);
        assert!(
            expected.exists(),
            "an object whose combined hash is {hash} belongs at {expected:?}"
        );

        // And the purge, from the compact key, finds that same file.
        assert_eq!(
            support::purge_active(cache, &key).await,
            PurgeOutcome::Purged(None)
        );
        assert!(!expected.exists());
    }
}

/// Step 5 (analysis): with the layout's fixed fan-out, 10^8 uniformly
/// hashed objects leave every directory inside REQ-EFFICIENCY.DIRSCALE-01's
/// 2,000-entry bound, even allowing for the temporaries a leaf holds at peak
/// concurrency.
#[test]
fn tc_layout_sharding_01_analysis_meets_the_directory_bound() {
    const OBJECTS: f64 = 1e8;
    let mean = OBJECTS / SHARD_COUNT as f64;
    // Binomial spread: sigma = sqrt(n * p * (1 - p)).
    let p = 1.0 / SHARD_COUNT as f64;
    let sigma = (OBJECTS * p * (1.0 - p)).sqrt();
    // The spec's own figures: about 1,526 entries per leaf, sigma about 39.
    assert!((mean - 1526.0).abs() < 1.0, "mean per leaf: {mean}");
    assert!((sigma - 39.0).abs() < 1.0, "sigma per leaf: {sigma}");

    // The bound sits more than twelve standard deviations above the mean,
    // which is the margin SDS-LAYOUT.SHARDING-01 claims; what is left over
    // is the room for the transient per-writer temporaries that share a
    // leaf, which are spread over all 65,536 leaves like the objects.
    let margin_in_sigmas = (2000.0 - mean) / sigma;
    assert!(
        margin_in_sigmas > 12.0,
        "2,000 entries must sit more than twelve standard deviations above the \
         mean of {mean:.0} (sigma {sigma:.0}); it sits at {margin_in_sigmas:.2}"
    );
    // First-level directories hold one entry per second-level shard.
    assert_eq!(SECOND_LEVEL_COUNT, 256);
}

/// Step 6: a sample of real hashes shows at most 256 entries in every
/// first-level directory and a per-leaf distribution consistent with the
/// analysis.
///
/// The test case's own sample is 10^6 objects; that many files takes minutes
/// and a million inodes, so the default run uses a smaller sample with the
/// same distribution, and `DURABLE_CACHE_SHARDING_SAMPLE` raises it for a
/// full run.
#[tokio::test]
async fn tc_layout_sharding_01_real_hashes_spread_as_the_analysis_predicts() {
    let sample: usize = std::env::var("DURABLE_CACHE_SHARDING_SAMPLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);

    let (cache, _tmp) = support::leaked_cache();
    let mut per_leaf: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..sample {
        let key = support::key(&format!("/sharding-sample-{i}"));
        let hash = key.combined();
        *per_leaf.entry(hash[0..4].to_owned()).or_default() += 1;
        support::store(cache, &key, &support::meta(), b"x").await;
    }

    // First-level directories: one entry per second-level shard present.
    for first in std::fs::read_dir(cache.root()).unwrap().flatten() {
        if first.file_name() == durable_cache::writerid::LEASE_DIR {
            continue;
        }
        let entries = std::fs::read_dir(first.path()).unwrap().count();
        assert!(
            entries <= SECOND_LEVEL_COUNT,
            "a first-level directory holds at most 256 entries, found {entries}"
        );
    }

    // Per-leaf counts on disk match the hashes the keys actually produced,
    // and stay within the spread the analysis predicts for this sample.
    let mean = sample as f64 / SHARD_COUNT as f64;
    let sigma = (sample as f64 * (1.0 / SHARD_COUNT as f64)).sqrt();
    let observed_max = *per_leaf.values().max().unwrap_or(&0);
    assert!(
        observed_max as f64 <= mean + 12.0 * sigma.max(1.0),
        "the busiest leaf held {observed_max} of {sample} objects (mean {mean:.2})"
    );
    for (leaf, count) in &per_leaf {
        let dir = cache.root().join(&leaf[0..2]).join(&leaf[2..4]);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            *count,
            "leaf {leaf} on disk must hold exactly the objects hashed into it"
        );
    }
}
