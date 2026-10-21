// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.FRESHNESS-01 — Freshness and age inputs preserved.
//!
//! The raw HTTP inputs this TC names (`Date`, `Expires`, `Age`,
//! `max-age`/`s-maxage`) are already interpreted into `CacheMeta`'s numeric
//! fields (`fresh_until`, `created`, `updated`, the two stale-serving
//! durations) before `pingora-cache` ever calls this backend --- that
//! interpretation is `pingora_cache::filters::resp_cacheable`'s job, not
//! this backend's (FDS-STORAGE.PUBLISH-01's own boundary: the backend
//! stores whatever `CacheMeta`/`ResponseHeader` it is given, verbatim). So
//! this TC's obligation on the backend is to round-trip those numeric
//! fields exactly, and the raw header bytes they were derived from
//! (`compat_headers.rs`'s concern) alongside them -- both covered here,
//! directly against `Storage`, no HTTP needed.

// IMP-TC.COMPLIANCE.FRESHNESS-01: satisfy TC-COMPLIANCE.FRESHNESS-01
use super::support;

use bytes::Bytes;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;
use std::time::{Duration, SystemTime};

struct FreshnessInputs {
    fresh_until: SystemTime,
    created: SystemTime,
    stale_while_revalidate_sec: u32,
    stale_if_error_sec: u32,
    /// Raw header text carried alongside -- proves the opaque bytes these
    /// numbers were nominally derived from round-trip too, un-normalised.
    raw_headers: &'static [(&'static str, &'static str)],
}

fn build_meta(inputs: &FreshnessInputs) -> CacheMeta {
    let mut header = ResponseHeader::build(200, None).unwrap();
    for (name, value) in inputs.raw_headers {
        header.append_header(*name, *value).unwrap();
    }
    CacheMeta::new(
        inputs.fresh_until,
        inputs.created,
        inputs.stale_while_revalidate_sec,
        inputs.stale_if_error_sec,
        header,
    )
}

/// A fixed reference instant (not "now" at assertion time) so `is_fresh`/
/// `fresh_sec` comparisons are deterministic regardless of how long the
/// test takes to run.
fn reference_time() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
}

fn assert_same_freshness(before: &CacheMeta, after: &CacheMeta) {
    let t = reference_time();
    assert_eq!(before.is_fresh(t), after.is_fresh(t));
    assert_eq!(before.fresh_sec(), after.fresh_sec());
    assert_eq!(before.fresh_until(), after.fresh_until());
    assert_eq!(before.created(), after.created());
    assert_eq!(before.updated(), after.updated());
    assert_eq!(
        before.stale_while_revalidate_sec(),
        after.stale_while_revalidate_sec()
    );
    assert_eq!(before.stale_if_error_sec(), after.stale_if_error_sec());
    assert_eq!(
        before.serve_stale_while_revalidate(t),
        after.serve_stale_while_revalidate(t)
    );
    assert_eq!(
        before.serve_stale_if_error(t),
        after.serve_stale_if_error(t)
    );
    // age() always reads real wall-clock time internally (no override), so
    // compare with a tolerance rather than bit-for-bit.
    let age_before = before.age();
    let age_after = after.age();
    let delta = age_after.max(age_before) - age_after.min(age_before);
    assert!(
        delta < Duration::from_secs(5),
        "age() before store and after retrieval must agree, modulo real \
         elapsed wall-clock time: before={age_before:?} after={age_after:?}"
    );
}

async fn store_and_lookup(cache: &'static FsCache, name: &str, meta: &CacheMeta) -> CacheMeta {
    let key = support::key(name);
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(&key, meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"freshness body"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();
    cache.lookup(&key, &trace).await.unwrap().unwrap().0
}

fn scenarios() -> Vec<(&'static str, FreshnessInputs)> {
    let now = SystemTime::now();
    vec![
        (
            "normal",
            FreshnessInputs {
                fresh_until: now + Duration::from_secs(60),
                created: now,
                stale_while_revalidate_sec: 30,
                stale_if_error_sec: 120,
                raw_headers: &[("cache-control", "max-age=60, stale-while-revalidate=30")],
            },
        ),
        (
            "age-zero",
            FreshnessInputs {
                fresh_until: now + Duration::from_secs(10),
                created: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                raw_headers: &[("age", "0")],
            },
        ),
        (
            "max-age-zero",
            FreshnessInputs {
                // Matches how pingora-cache itself treats a zero freshness
                // duration: fresh_until placed just before `created`, so
                // the object is immediately stale.
                fresh_until: now - Duration::from_secs(1),
                created: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                raw_headers: &[("cache-control", "max-age=0")],
            },
        ),
        (
            "stale-overflow-cap",
            FreshnessInputs {
                fresh_until: now + Duration::from_secs(10),
                created: now,
                // Delta-seconds overflow cap per RFC 7234's parsing rules,
                // still a valid u32.
                stale_while_revalidate_sec: 2_147_483_648,
                stale_if_error_sec: 2_147_483_648,
                raw_headers: &[("cache-control", "stale-while-revalidate=2147483648")],
            },
        ),
        (
            "already-past-expires",
            FreshnessInputs {
                fresh_until: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
                created: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                raw_headers: &[("expires", "Fri, 15 May 2015 15:34:21 GMT")],
            },
        ),
        (
            "invalid-expires-opaque-header",
            FreshnessInputs {
                // The backend never parses Expires itself; an invalid value
                // is just opaque header bytes to it (compat_headers.rs's
                // concern) -- freshness here comes from whatever numeric
                // fields the caller supplied regardless.
                fresh_until: now + Duration::from_secs(5),
                created: now,
                stale_while_revalidate_sec: 0,
                stale_if_error_sec: 0,
                raw_headers: &[("expires", "not-a-valid-http-date")],
            },
        ),
    ]
}

#[tokio::test]
async fn tc_compliance_freshness_01_all_scenarios_round_trip() {
    let (cache, _tmp) = support::leaked_cache();
    for (name, inputs) in scenarios() {
        let before = build_meta(&inputs);
        let raw_before: Vec<(String, String)> = inputs
            .raw_headers
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();

        let after = store_and_lookup(cache, &format!("/freshness-{name}"), &before).await;
        assert_same_freshness(&before, &after);

        let raw_after: Vec<(String, String)> = inputs
            .raw_headers
            .iter()
            .map(|(n, _)| {
                (
                    n.to_string(),
                    after
                        .response_header()
                        .headers
                        .get(*n)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            raw_after, raw_before,
            "scenario {name}: the raw header bytes freshness was nominally \
             derived from must round-trip un-normalised alongside the numbers"
        );
    }
}

#[tokio::test]
async fn tc_compliance_freshness_01_survives_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let inputs = &scenarios()[0].1;
    let before = build_meta(inputs);
    let key = support::key("/freshness-restart");
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &before, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"x"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let restarted = FsCache::new(FsCacheConfig::new(tmp.path(), None)).unwrap();
    let restarted: &'static FsCache = Box::leak(Box::new(restarted));
    let (after, _hit) = restarted.lookup(&key, &trace).await.unwrap().unwrap();

    assert_same_freshness(&before, &after);
}

#[tokio::test]
async fn tc_compliance_freshness_01_metadata_update_changes_freshness_for_every_lookup() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/freshness-update");
    let trace = support::inactive_trace();
    let now = SystemTime::now();

    let original = build_meta(&FreshnessInputs {
        fresh_until: now + Duration::from_secs(10),
        created: now,
        stale_while_revalidate_sec: 0,
        stale_if_error_sec: 0,
        raw_headers: &[("cache-control", "max-age=10")],
    });
    let mut handler = cache
        .get_miss_handler(&key, &original, &trace)
        .await
        .unwrap();
    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    // A metadata update carrying new freshness fields, as a revalidation
    // response would (FRE-COMPLIANCE.REVALIDATION-01): a longer freshness
    // window, and a newly granted stale-while-revalidate allowance, for the
    // stored response, whose `created` `pingora-cache` keeps.
    let updated = support::revalidation(
        &original,
        &build_meta(&FreshnessInputs {
            fresh_until: now + Duration::from_secs(125),
            created: now + Duration::from_secs(5),
            stale_while_revalidate_sec: 30,
            stale_if_error_sec: 0,
            raw_headers: &[("cache-control", "max-age=120, stale-while-revalidate=30")],
        }),
    );
    assert!(
        cache
            .update_meta(&key, &updated, &trace)
            .await
            .expect("the update returns"),
        "the update is applied"
    );

    let (found, _hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    assert_eq!(found.fresh_until(), updated.fresh_until());
    assert_eq!(
        found.created(),
        now,
        "created() still names the stored response"
    );
    assert_eq!(
        found.updated(),
        updated.updated(),
        "updated() is the update's"
    );
    assert_eq!(found.stale_while_revalidate_sec(), 30);
    assert_ne!(
        found.fresh_until(),
        original.fresh_until(),
        "must reflect the update, never the superseded freshness window"
    );
}
