// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.CONCURRENT-01 — Concurrent same-key writes stay isolated.
//!
//! Isolation-without-interleaving among the writers themselves is also
//! covered, from the publish side, in `storage_publish.rs`'s
//! `tc_storage_publish_01_concurrent_same_key_writers_stay_isolated`; this
//! file adds the mid-flight-lookup leg (step 2) that TC-COMPAT.CONCURRENT-01
//! specifically calls for.

// IMP-TC.COMPAT.CONCURRENT-01: satisfy TC-COMPAT.CONCURRENT-01
use super::support;

use bytes::Bytes;
use pingora_cache::storage::Storage;

#[tokio::test]
async fn tc_compat_concurrent_01_lookups_during_concurrent_writes_never_see_a_torn_result() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/concurrent-lookup");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut a = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let mut b = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();

    a.write_body(Bytes::from_static(b"AAAA"), false)
        .await
        .unwrap();
    b.write_body(Bytes::from_static(b"1111"), false)
        .await
        .unwrap();

    // While both stores are still in flight, issue several lookups: each
    // must resolve to a whole, uncorrupted read of exactly one writer's
    // bytes -- necessarily the most recently opened one, `b` -- never a
    // miss (an in-flight writer always exists here) and never a mixture.
    for _ in 0..5 {
        let (_meta, mut hit) = cache
            .lookup(&key, &trace)
            .await
            .unwrap()
            .expect("an in-flight writer exists, so this must be a partial hit, never a miss");
        let chunk = hit.read_body().await.unwrap().unwrap();
        assert_eq!(
            &chunk[..],
            b"1111",
            "must read exactly one writer's bytes whole, never interleaved with the other's"
        );
    }

    a.write_body(Bytes::from_static(b"AAAA"), true)
        .await
        .unwrap();
    b.write_body(Bytes::from_static(b"1111"), true)
        .await
        .unwrap();
    a.finish().await.unwrap();
    b.finish().await.unwrap();

    let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    assert!(
        body == b"AAAAAAAA" || body == b"11111111",
        "once both complete, the backend must have converged on exactly one whole, uncorrupted object: {body:?}"
    );
}
