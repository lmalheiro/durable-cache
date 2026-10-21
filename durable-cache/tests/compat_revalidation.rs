// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.REVALIDATION-01 — Metadata freshened atomically.
//!
//! Step 2 (lookups concurrently with an update never see a mixture) is
//! guaranteed by construction rather than forced here as a literal race:
//! the header write is the sole commit point, and `crate::lookup`'s
//! bounded retry absorbs a reader catching it mid-write (see
//! `metaupdate.rs`'s and `lookup.rs`'s own doc comments).

// IMP-TC.COMPLIANCE.REVALIDATION-01: satisfy TC-COMPLIANCE.REVALIDATION-01
use super::support;

use bytes::Bytes;
use durable_cache::{format, FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;
use std::time::{Duration, SystemTime};

fn meta_with_header(name: &'static str, value: &str) -> CacheMeta {
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.append_header(name, value).unwrap();
    let now = SystemTime::now();
    CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header)
}

async fn store(
    cache: &'static FsCache,
    key: &pingora_cache::CacheKey,
    meta: &CacheMeta,
    body: &'static [u8],
) {
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(key, meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(body), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();
}

async fn lookup_body_and_header(
    cache: &'static FsCache,
    key: &pingora_cache::CacheKey,
    name: &str,
) -> (Vec<u8>, Option<String>) {
    let trace = support::inactive_trace();
    let (meta, mut hit) = cache.lookup(key, &trace).await.unwrap().unwrap();
    let value = meta
        .response_header()
        .headers
        .get(name)
        .map(|v| v.to_str().unwrap().to_string());
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    (body, value)
}

#[tokio::test]
async fn tc_compliance_revalidation_01_in_place_update_preserves_body() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-in-place");

    let stored = meta_with_header("etag", "\"v1\"");
    store(cache, &key, &stored, b"original body").await;
    cache
        .update_meta(
            &key,
            &support::revalidation(&stored, &meta_with_header("etag", "\"v2\"")),
            &support::inactive_trace(),
        )
        .await
        .unwrap();

    let (body, etag) = lookup_body_and_header(cache, &key, "etag").await;
    assert_eq!(
        body, b"original body",
        "the body must be untouched by a metadata-only update"
    );
    assert_eq!(etag.as_deref(), Some("\"v2\""));
}

#[tokio::test]
async fn tc_compliance_revalidation_01_missing_key_reports_distinguishable_error() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-missing");

    let err = cache
        .update_meta(
            &key,
            &meta_with_header("etag", "\"x\""),
            &support::inactive_trace(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.etype, pingora_error::ErrorType::Custom("key_not_found"));
    // Its context says which operation found which file missing.
    assert_eq!(
        err.context.as_ref().map(|c| c.as_str().to_owned()),
        Some(format!(
            "update_meta: no object stored at {}",
            support::canonical_path(cache, &key).display()
        ))
    );

    assert!(
        cache
            .lookup(&key, &support::inactive_trace())
            .await
            .unwrap()
            .is_none(),
        "must not materialise a bodyless object for a key that was never stored"
    );
}

#[tokio::test]
async fn tc_compliance_revalidation_01_update_durable_across_restart() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/revalidation-restart");

    let stored = meta_with_header("etag", "\"before\"");
    store(cache, &key, &stored, b"restart body").await;
    cache
        .update_meta(
            &key,
            &support::revalidation(&stored, &meta_with_header("etag", "\"after\"")),
            &support::inactive_trace(),
        )
        .await
        .unwrap();

    let restarted = FsCache::new(FsCacheConfig::new(tmp.path(), None)).unwrap();
    let restarted: &'static FsCache = Box::leak(Box::new(restarted));
    let (body, etag) = lookup_body_and_header(restarted, &key, "etag").await;
    assert_eq!(body, b"restart body");
    assert_eq!(
        etag.as_deref(),
        Some("\"after\""),
        "a restart must return the updated metadata, never the superseded set"
    );
}

/// The length of a stored `x-filler` header, which tells the header sets of
/// `support::meta_with_header_filler` apart.
fn filler_len(meta: &CacheMeta) -> usize {
    meta.headers()
        .get("x-filler")
        .expect("the stored header set")
        .len()
}

/// Whether the record at `path` is withdrawn.
fn is_withdrawn(path: &std::path::Path) -> bool {
    support::read_header(path).version() == format::WITHDRAWN_VERSION
}

/// The error `update_meta` reports for a key that holds no object.
fn is_key_not_found(err: &pingora_error::Error) -> bool {
    err.etype == pingora_error::ErrorType::Custom("key_not_found")
}

/// Steps 1 and 3: an update whose header set outgrows the region the store
/// reserved is not applied: it withdraws the record, in place, and reports
/// `false`. A client already reading the record still gets its whole body;
/// from then on, a lookup misses, so the next request fetches the object
/// again. Step 4: a restart finds it withdrawn too.
#[tokio::test]
async fn tc_compliance_revalidation_01_an_outgrown_update_withdraws_the_record() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/revalidation-withdrawn");
    let stored = support::meta_with_header_filler(8);
    let body: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    support::store(cache, &key, &stored, &body).await;
    let canonical = support::canonical_path(cache, &key);
    let length = std::fs::metadata(&canonical).expect("the record").len();
    let (_, mut reading) = support::lookup_hit(cache, &key).await;

    // 4000 bytes of headers outgrow the region's 512 bytes of headroom.
    let trace = support::inactive_trace();
    assert!(
        !cache
            .update_meta(
                &key,
                &support::revalidation(&stored, &support::meta_with_header_filler(4000)),
                &trace,
            )
            .await
            .expect("the update returns"),
        "the update is not applied"
    );
    assert!(is_withdrawn(&canonical), "the record is withdrawn");
    assert_eq!(
        std::fs::metadata(&canonical).expect("the record").len(),
        length,
        "in place: the file keeps its length, and the space the eviction manager counts"
    );
    assert_eq!(
        support::drain_body(&mut reading)
            .await
            .expect("the open hit reads"),
        body,
        "a client already reading the record gets its whole body"
    );
    assert!(
        support::lookup_body(cache, &key).await.is_none(),
        "a lookup misses at once"
    );

    let restarted = support::leaked_cache_at(tmp.path());
    assert!(
        support::lookup_body(restarted, &key).await.is_none(),
        "and after a restart"
    );
}

/// Step 6: with no headroom configured, the region is exactly full, so even
/// a small update that grows the metadata withdraws the record.
#[tokio::test]
async fn tc_compliance_revalidation_01_zero_headroom_withdraws_on_any_growth() {
    let (cache, _tmp) = support::leaked_cache_with_headroom(0);
    let key = support::key("/revalidation-zero-headroom");
    let stored = meta_with_header("etag", "\"v1\"");
    store(cache, &key, &stored, b"tiny").await;
    let canonical = support::canonical_path(cache, &key);

    // Sixteen bytes more than the full region holds: more than the internal
    // segment's timestamps can shrink by, so the update cannot fit it.
    let trace = support::inactive_trace();
    assert!(
        !cache
            .update_meta(
                &key,
                &support::revalidation(
                    &stored,
                    &meta_with_header("etag", "\"v1-0123456789abcdef\""),
                ),
                &trace,
            )
            .await
            .expect("the update returns"),
        "the update is not applied"
    );
    assert!(is_withdrawn(&canonical), "the record is withdrawn");
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 7: the store the next request makes replaces the withdrawn file,
/// with a region sized for its own header set, and lookups hit it.
#[tokio::test]
async fn tc_compliance_revalidation_01_the_next_store_replaces_a_withdrawn_record() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-stored-again");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"old body").await;
    let canonical = support::canonical_path(cache, &key);
    let trace = support::inactive_trace();
    let fetched = support::meta_with_header_filler(4000);
    assert!(!cache
        .update_meta(&key, &support::revalidation(&stored, &fetched), &trace)
        .await
        .expect("the update returns"));
    assert!(is_withdrawn(&canonical));

    support::store(cache, &key, &fetched, b"fetched again").await;
    let (header, _) = support::read_and_validate_record(&canonical);
    assert!(
        header.meta_capacity() > header.internal_len() + header.header_len(),
        "a region with headroom for the new header set"
    );
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the object is stored again");
    assert_eq!(filler_len(&meta), 4000);
    assert_eq!(body, b"fetched again");
}

/// Step 8: sixteen updates that each outgrow the region, made at once. They
/// take the key's lock one after another: the first withdraws the record
/// and reports `false`, and every later one finds no object stored, and
/// reports so, as an update of an absent key does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compliance_revalidation_01_outgrown_updates_at_once_withdraw_the_record_once() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-herd");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"stored body").await;
    let canonical = support::canonical_path(cache, &key);
    let length = std::fs::metadata(&canonical).expect("the record").len();

    let mut herd = Vec::new();
    for extra in 1..=16 {
        let key = key.clone();
        let update =
            support::revalidation(&stored, &support::meta_with_header_filler(4000 + extra));
        herd.push(tokio::spawn(async move {
            let trace = support::inactive_trace();
            cache.update_meta(&key, &update, &trace).await
        }));
    }
    let mut withdrew = 0;
    for update in herd {
        match update.await.expect("the updating task completes") {
            Ok(applied) => {
                assert!(!applied, "no update is applied");
                withdrew += 1;
            }
            Err(e) => assert!(is_key_not_found(&e), "unexpected error: {e}"),
        }
    }
    assert_eq!(withdrew, 1, "one update withdraws the record");
    assert!(is_withdrawn(&canonical));
    assert_eq!(
        std::fs::metadata(&canonical).expect("the record").len(),
        length,
        "and nothing grows"
    );
}

/// Step 8, continued: an update that outgrows the record while a store of
/// the key from the origin is in flight withdraws the record. The store then
/// lands and replaces it, as any newer store does.
#[tokio::test]
async fn tc_compliance_revalidation_01_an_outgrown_update_during_a_store_from_the_origin_withdraws()
{
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-during-a-store");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"old body").await;
    let canonical = support::canonical_path(cache, &key);
    let trace = support::inactive_trace();
    let mut store = cache
        .get_miss_handler(&key, &support::meta_with_header_filler(8), &trace)
        .await
        .expect("a store opens");
    store
        .write_body(Bytes::from_static(b"new body"), true)
        .await
        .expect("its body is written");

    assert!(
        !cache
            .update_meta(
                &key,
                &support::revalidation(&stored, &support::meta_with_header_filler(4000)),
                &trace,
            )
            .await
            .expect("the update returns"),
        "the update is not applied"
    );
    assert!(is_withdrawn(&canonical), "the record is withdrawn");

    store.finish().await.expect("the store lands");
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the object is stored");
    assert_eq!(body, b"new body", "the store from the origin stands");
    assert_eq!(filler_len(&meta), 8);
}

/// Step 10: a withdrawal whose header write fails reports the error and
/// leaves the record as it was, still served with its old metadata. The
/// next attempt withdraws it.
#[tokio::test]
async fn tc_compliance_revalidation_01_a_failed_withdrawal_leaves_the_record_as_it_was() {
    use durable_cache::hooks::FaultPoint;
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-failed-withdrawal");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"stored body").await;
    let canonical = support::canonical_path(cache, &key);
    let before = std::fs::read(&canonical).expect("the record");
    let update = support::revalidation(&stored, &support::meta_with_header_filler(4000));

    cache.hooks().inject_fault(FaultPoint::WithdrawRecord, 1);
    let trace = support::inactive_trace();
    cache
        .update_meta(&key, &update, &trace)
        .await
        .expect_err("the withdrawal fails");
    assert_eq!(
        std::fs::read(&canonical).expect("the record"),
        before,
        "the record is as it was"
    );
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the record still reads");
    assert_eq!(filler_len(&meta), 8, "with its old metadata");
    assert_eq!(body, b"stored body");

    assert!(
        !cache
            .update_meta(&key, &update, &trace)
            .await
            .expect("the update returns"),
        "the next attempt is not applied either"
    );
    assert!(is_withdrawn(&canonical), "it withdraws the record");
}

/// An update still waiting for the key's lock when a newer store replaces
/// the record is not applied, and writes nothing. It opens the record only
/// once it holds the lock, so it finds the newer response, which is not the
/// one it freshens (FDS-STORAGE.METAUPDATE-01). Had it opened the record
/// first, it would have written into the replaced file, and reported
/// success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_paused_before_its_lock_leaves_a_record_replaced_meanwhile() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-replaced-before-the-lock");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache.hooks().arm_pause(
        durable_cache::hooks::PausePoint::UpdateBeforeLock,
        Some(&canonical),
    );
    let updating = {
        let key = key.clone();
        let update = support::revalidation(&stored, &support::meta_with_header_filler(100));
        tokio::spawn(async move {
            let trace = support::inactive_trace();
            cache.update_meta(&key, &update, &trace).await
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the update must reach its pause point"
    );

    support::store(
        cache,
        &key,
        &support::meta_with_header_filler(16),
        b"newer body",
    )
    .await;
    pause.release();

    assert!(
        !updating
            .await
            .expect("the updating task completes")
            .expect("the update returns"),
        "the update is not applied"
    );
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the newer object");
    assert_eq!(body, b"newer body");
    assert_eq!(filler_len(&meta), 16, "the newer response stands whole");
}

/// An update still waiting for the key's lock when a purge removes the
/// record reports the key absent, as an update of any key with no stored
/// object does (FRE-COMPLIANCE.METAABSENT-01), and creates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_paused_before_its_lock_reports_a_purged_key_absent() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-purged-before-the-lock");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache.hooks().arm_pause(
        durable_cache::hooks::PausePoint::UpdateBeforeLock,
        Some(&canonical),
    );
    let updating = {
        let key = key.clone();
        let update = support::revalidation(&stored, &support::meta_with_header_filler(100));
        tokio::spawn(async move {
            let trace = support::inactive_trace();
            cache.update_meta(&key, &update, &trace).await
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the update must reach its pause point"
    );

    assert_eq!(
        support::purge_active(cache, &key).await,
        pingora_cache::storage::PurgeOutcome::Purged(None),
        "the purge removes the record"
    );
    pause.release();

    let err = updating
        .await
        .expect("the updating task completes")
        .expect_err("an update of a purged key fails");
    assert_eq!(err.etype, pingora_error::ErrorType::Custom("key_not_found"));
    assert!(
        support::lookup_body(cache, &key).await.is_none(),
        "the key still holds no object"
    );
}

/// Step 9: once the backend is read-only, an update fails with an error,
/// whether it fits the region or would withdraw the record, and changes
/// nothing (FRE-COMPAT.HOTUPGRADE-01).
#[tokio::test]
async fn tc_compliance_revalidation_01_once_read_only_fails_on_either_path() {
    // 100 bytes of headers fit 512 bytes of headroom; 4000 fit none.
    for (headroom, filler) in [(512u32, 100), (0, 4000)] {
        let (cache, _tmp) = support::leaked_cache_with_headroom(headroom);
        let key = support::key("/revalidation-read-only");
        let stored = support::meta_with_header_filler(8);
        support::store(cache, &key, &stored, b"body").await;
        let before = std::fs::read(support::canonical_path(cache, &key)).unwrap();

        cache.set_read_only();

        let trace = support::inactive_trace();
        let err = cache
            .update_meta(
                &key,
                &support::revalidation(&stored, &support::meta_with_header_filler(filler)),
                &trace,
            )
            .await
            .expect_err("a read-only backend refuses metadata updates");
        assert_eq!(
            err.etype,
            pingora_error::ErrorType::Custom("cache_read_only"),
            "unexpected error: {err}"
        );
        assert_eq!(
            std::fs::read(support::canonical_path(cache, &key)).unwrap(),
            before,
            "nothing on disk may change with {headroom} bytes of headroom"
        );
    }
}

/// Concurrent in-place updates of one key never leave a record whose header
/// and metadata region disagree: each takes the key's publication lock, so
/// the object stays readable and carries one of the sets written
/// (FRE-COMPLIANCE.METAATOMIC-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compliance_revalidation_01_concurrent_in_place_updates_stay_readable() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-concurrent");
    let stored = meta_with_header("etag", "\"v0\"");
    support::store(cache, &key, &stored, b"body").await;

    let mut updates = Vec::new();
    for i in 0..8 {
        let key = key.clone();
        let rounds: Vec<CacheMeta> = (0..25)
            .map(|round| {
                let etag = format!("\"w{i}-{round}\"");
                support::revalidation(&stored, &meta_with_header("etag", &etag))
            })
            .collect();
        updates.push(tokio::spawn(async move {
            let trace = support::inactive_trace();
            for update in &rounds {
                assert!(
                    cache
                        .update_meta(&key, update, &trace)
                        .await
                        .expect("the update returns"),
                    "each update is applied"
                );
            }
        }));
    }
    for update in updates {
        update.await.unwrap();
    }

    let (body, etag) = lookup_body_and_header(cache, &key, "etag").await;
    assert_eq!(body, b"body", "the body is untouched throughout");
    let etag = etag.expect("the record must still be readable after the race");
    assert!(
        etag.starts_with("\"w") && etag.ends_with('"'),
        "the metadata must be one writer's set, whole: {etag}"
    );
}

/// Step 13: a `304` updates only the response it validated (RFC 9111
/// §4.3.4). When a newer response has replaced that one since the lookup
/// that began the revalidation, which `pingora-cache` allows once a writer
/// outlives its cache lock, the update is not applied, and the newer response
/// stands whole.
#[tokio::test]
async fn tc_compliance_revalidation_01_a_304_for_a_replaced_response_is_not_applied() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/revalidation-of-a-replaced-response");
    let first = meta_with_header("etag", "\"v1\"");
    store(cache, &key, &first, b"one").await;
    let newer = {
        let mut header = ResponseHeader::build(200, None).expect("a header");
        header.append_header("etag", "\"v2\"").expect("an etag");
        let created = first.created() + Duration::from_secs(1);
        CacheMeta::new(created + Duration::from_secs(60), created, 0, 0, header)
    };
    store(cache, &key, &newer, b"second").await;

    let trace = support::inactive_trace();
    assert!(
        !cache
            .update_meta(
                &key,
                &support::revalidation(&first, &meta_with_header("etag", "\"v1\"")),
                &trace,
            )
            .await
            .expect("the update returns"),
        "the update for the replaced response is not applied"
    );
    let (body, etag) = lookup_body_and_header(cache, &key, "etag").await;
    assert_eq!(body, b"second");
    assert_eq!(
        etag.as_deref(),
        Some("\"v2\""),
        "the newer response stands whole"
    );
}
