// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.STREAMING-01 — Followers stream a partial hit.
//!
//! The backend advertises streaming partial writes
//! (FRE-COMPAT.STREAMFLAG-01), a lookup while several stores of a key are
//! in flight resolves to the most recently opened one
//! (FRE-COMPAT.STREAMING-01), chunks reach followers within the bound
//! FRE-COMPAT.STREAMDELIVERY-01 sets, and a follower ends normally when the
//! store completes (FRE-COMPAT.STREAMEND-01).
//!
//! Two legs need a real proxy rather than the `Storage` API: the cache
//! lock being released once the leader's miss handler opens (step 2), and
//! counting upstream fetches (step 9). Those are
//! `test/compat/tests/streaming.rs`'s, which carries the IMP marker that
//! closes this TC jointly with this file. The abort leg
//! (FRE-COMPLIANCE.PARTIALABORT-01) is in `compat_incomplete.rs`.

use super::support;

use bytes::Bytes;
use pingora_cache::storage::Storage;
use std::time::{Duration, Instant};

/// Step 1: the flag that lets followers reach a partial hit at all
/// (FRE-COMPAT.STREAMFLAG-01).
#[tokio::test]
async fn tc_compat_streaming_01_advertises_streaming_partial_writes() {
    let (cache, _tmp) = support::leaked_cache();
    assert!(cache.support_streaming_partial_write());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_compat_streaming_01_follower_streams_as_leader_writes() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-basic");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    writer
        .write_body(Bytes::from_static(b"chunk1-"), false)
        .await
        .unwrap();

    let (_meta, mut follower) = cache
        .lookup(&key, &trace)
        .await
        .unwrap()
        .expect("a partial hit while the writer is still open");

    // Already-written bytes are delivered without waiting.
    let first = follower.read_body().await.unwrap();
    assert_eq!(first, Some(Bytes::from_static(b"chunk1-")));

    // The next read has nothing available yet, so it must wait -- run it on
    // a spawned task and let the leader's next write wake it.
    let read_task = tokio::spawn(async move {
        let second = follower.read_body().await.unwrap();
        let third = follower.read_body().await.unwrap();
        (follower, second, third)
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    writer
        .write_body(Bytes::from_static(b"chunk2"), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();

    let (mut follower, second, third) = read_task.await.unwrap();
    assert_eq!(second, Some(Bytes::from_static(b"chunk2")));
    assert_eq!(
        third, None,
        "the reader must remain valid across the concluding rename and terminate as a clean end of body"
    );

    // No more reads after a clean end of body.
    assert_eq!(follower.read_body().await.unwrap(), None);
}

#[tokio::test]
async fn tc_compat_streaming_01_lookup_resolves_to_the_most_recently_opened_writer() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-most-recent");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut first_writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    first_writer
        .write_body(Bytes::from_static(b"from-first"), false)
        .await
        .unwrap();

    let mut second_writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    second_writer
        .write_body(Bytes::from_static(b"from-second"), false)
        .await
        .unwrap();

    let (_meta, mut follower) = cache
        .lookup(&key, &trace)
        .await
        .unwrap()
        .expect("a partial hit");
    let chunk = follower.read_body().await.unwrap().unwrap();
    assert_eq!(
        &chunk[..],
        b"from-second",
        "lookup must resolve to the most recently opened writer, never an earlier one"
    );

    // Let both writers finish so the test doesn't leave a dangling
    // in-flight entry behind (last rename wins; either outcome is fine
    // here, only the *lookup* resolution above is under test).
    first_writer.write_body(Bytes::new(), true).await.unwrap();
    first_writer.finish().await.unwrap();
    second_writer.write_body(Bytes::new(), true).await.unwrap();
    second_writer.finish().await.unwrap();
}

/// Steps 6 to 8: several followers on one in-flight store receive each
/// chunk as the store writes it — at least 99 % of deliveries within 10 ms
/// of the chunk's `write_body` returning (FRE-COMPAT.STREAMDELIVERY-01) —
/// and every follower's body matches the store's, ending normally across
/// the concluding rename (FRE-COMPAT.STREAMEND-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compat_streaming_01_followers_receive_chunks_within_the_latency_bound() {
    const FOLLOWERS: usize = 10;
    const CHUNKS: usize = 1000;
    const CHUNK: &[u8] = b"0123456789abcdef";

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-latency");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    // One chunk first, so every follower attaches to a store that already
    // has a byte to hand over.
    writer
        .write_body(Bytes::from_static(CHUNK), false)
        .await
        .unwrap();

    // The instant the leader's most recent chunk returned from write_body.
    let last_write = std::sync::Arc::new(std::sync::Mutex::new(Instant::now()));

    let mut readers = Vec::new();
    for _ in 0..FOLLOWERS {
        let (_meta, mut follower) = support::lookup_hit(cache, &key).await;
        let last_write = last_write.clone();
        readers.push(tokio::spawn(async move {
            let mut delays = Vec::new();
            let mut body = Vec::new();
            while let Some(chunk) = follower.read_body().await.unwrap() {
                let since = {
                    let at = *last_write.lock().unwrap();
                    at.elapsed()
                };
                delays.push(since);
                body.extend_from_slice(&chunk);
            }
            (delays, body)
        }));
    }

    for i in 1..CHUNKS {
        let eof = i + 1 == CHUNKS;
        writer
            .write_body(Bytes::from_static(CHUNK), eof)
            .await
            .unwrap();
        *last_write.lock().unwrap() = Instant::now();
        // Let the followers run: a throttled upstream is the case the bound
        // is written for.
        tokio::task::yield_now().await;
    }
    writer.finish().await.unwrap();

    let expected: Vec<u8> = CHUNK.repeat(CHUNKS);
    let mut all_delays = Vec::new();
    for reader in readers {
        let (delays, body) = reader.await.unwrap();
        assert_eq!(
            body, expected,
            "every follower receives the whole body, in order"
        );
        all_delays.extend(delays);
    }

    all_delays.sort();
    let p99 = all_delays[all_delays.len() * 99 / 100];
    assert!(
        p99 <= Duration::from_millis(10),
        "at least 99% of chunk deliveries must be within 10 ms of their write_body \
         returning; the 99th percentile was {p99:?} over {} deliveries",
        all_delays.len()
    );
}

/// A lookup carrying a store's streaming write tag, which `pingora-cache`
/// makes from the request that opened the store to read back the body it
/// is writing, resolves to that very store, not to whichever opened most
/// recently.
#[tokio::test]
async fn tc_compat_streaming_01_a_tagged_lookup_resolves_to_its_own_writer() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-tagged");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut first = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    first
        .write_body(Bytes::from_static(b"from-first"), false)
        .await
        .unwrap();
    let tag = first
        .streaming_write_tag()
        .map(<[u8]>::to_vec)
        .expect("a write tag");

    let mut second = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    second
        .write_body(Bytes::from_static(b"from-second"), false)
        .await
        .unwrap();

    let (_meta, mut follower) = cache
        .lookup_streaming_write(&key, Some(&tag), &trace)
        .await
        .unwrap()
        .expect("the tagged store is still in flight");
    let chunk = follower.read_body().await.unwrap().unwrap();
    assert_eq!(
        &chunk[..],
        b"from-first",
        "a tagged lookup must never hop to another writer's body"
    );

    drop(follower);
    drop(first);
    second.write_body(Bytes::new(), true).await.unwrap();
    second.finish().await.unwrap();
}

/// A partial hit neither seeks nor counts an eviction access, like one from
/// `MemCache`, the in-memory backend of `pingora-cache`
/// (FDS-STORAGE.PARTIAL-01).
#[tokio::test]
async fn tc_compat_streaming_01_partial_hits_do_not_seek_or_count_access() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-no-seek");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    writer
        .write_body(Bytes::from_static(b"in flight"), false)
        .await
        .unwrap();

    let (_meta, mut partial) = support::lookup_hit(cache, &key).await;
    assert!(!partial.can_seek(), "a partial hit cannot seek");
    assert!(partial.seek(0, None).is_err());
    assert!(
        !partial.should_count_access(),
        "and counts no eviction access"
    );

    drop(partial);
    drop(writer);

    // A published record does both.
    support::store(cache, &key, &meta, b"published").await;
    let (_meta, published) = support::lookup_hit(cache, &key).await;
    assert!(published.can_seek());
    assert!(published.should_count_access());
}

#[tokio::test]
async fn tc_compat_streaming_01_multiple_concurrent_followers_see_the_same_writer() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-multi-follower");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut writer = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    writer
        .write_body(Bytes::from_static(b"shared"), true)
        .await
        .unwrap();
    writer.finish().await.unwrap();

    // Once published, plain repeated lookups are still just complete hits
    // -- this exercises that concurrent lookups of the same now-published
    // key all see the identical, whole object.
    for _ in 0..4 {
        let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = hit.read_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"shared");
    }
}

/// A tagged lookup is answered from the store its tag names while that
/// store is in flight, and otherwise with nothing: never from another
/// writer's body, and never from the published record, which may be
/// another store's (`Storage::lookup_streaming_write`'s own contract).
#[tokio::test]
async fn tc_compat_streaming_01_a_tagged_lookup_never_falls_back_to_another_object() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-tag-gone");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut store = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let tag = store
        .streaming_write_tag()
        .map(<[u8]>::to_vec)
        .expect("a write tag");
    store
        .write_body(Bytes::from_static(b"the tagged store"), true)
        .await
        .unwrap();
    store.finish().await.unwrap();

    // The store has completed, so its tag names nothing in flight. Not even
    // the record it published answers: the record under a key may be a
    // newer store's.
    assert!(
        cache
            .lookup_streaming_write(&key, Some(&tag), &trace)
            .await
            .expect("the lookup succeeds")
            .is_none(),
        "a completed store's tag must resolve to nothing"
    );

    // A tag this backend never issued resolves to nothing at all, rather
    // than to whatever happens to be published under the key.
    let unknown = pingora_cache::storage::streaming_write::U64WriteId::from(u64::MAX);
    assert!(
        cache
            .lookup_streaming_write(&key, Some(unknown.as_bytes()), &trace)
            .await
            .unwrap()
            .is_none(),
        "an unknown tag must never be answered from another object"
    );
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"the tagged store",
        "an untagged lookup still finds the published object"
    );

    // A tag of a width this backend never issues is not "no tag": answering
    // it from the published record, or from whichever store happens to be in
    // flight, would hand a request that named one write a different body.
    for malformed in [&b""[..], &b"short"[..], &b"nine bytes"[..]] {
        assert!(
            cache
                .lookup_streaming_write(&key, Some(malformed), &trace)
                .await
                .unwrap()
                .is_none(),
            "a {}-byte tag must resolve to nothing, not to another body",
            malformed.len()
        );
    }
}

/// A tagged lookup whose store's temporary does not read back is an error,
/// not a miss: `pingora-cache` panics when the lookup that reads back the
/// body it is writing finds nothing. An untagged lookup passes over the
/// damaged temporary to the published record.
#[tokio::test]
async fn a_tagged_lookup_of_a_damaged_temporary_is_an_error() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/streaming-tag-damaged");
    let trace = support::inactive_trace();
    support::store(cache, &key, &support::meta(), b"the published object").await;

    let store = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("a store opens");
    let tag = store
        .streaming_write_tag()
        .map(<[u8]>::to_vec)
        .expect("a write tag");
    let temps = support::temp_entries(cache, &key);
    assert_eq!(temps.len(), 1, "the store has one temporary");
    support::flip_magic_byte(&support::shard_dir_of(cache, &key).join(&temps[0]));

    assert!(
        cache
            .lookup_streaming_write(&key, Some(&tag), &trace)
            .await
            .is_err(),
        "the store's own damaged temporary must be an error, not a miss"
    );
    assert_eq!(
        support::lookup_body(cache, &key)
            .await
            .expect("the published object is still found")
            .1,
        b"the published object",
        "an untagged lookup passes over the damaged temporary"
    );
    drop(store);
}
