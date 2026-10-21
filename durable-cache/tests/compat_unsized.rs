// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.UNSIZED-01 — Unsized body accepted and persisted.
//!
//! Every clause of this TC's expected result — no size declared, reserved,
//! or bounded ahead of the bytes; persisted length exactly equal to the
//! total streamed, including the empty-body and multi-chunk-boundary cases;
//! and every chunk in the file before its `write_body` returns, so the store
//! holds no body bytes beyond the chunk in hand — is a property of
//! `Storage::get_miss_handler`/`write_body` itself, not of how the bytes
//! were sourced, so this crate-level exercise (no HTTP, no upstream) closes
//! it fully. `test/compat`'s `unsized_body.rs` is a redundant end-to-end
//! HTTP-level check of the same property.

use bytes::Bytes;
use durable_cache::sharding::shard_path;
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.COMPAT.UNSIZED-01: satisfy TC-COMPAT.UNSIZED-01

async fn store_and_verify(name: &str, chunks: &[&[u8]]) {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key(name);
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let mut expected = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let eof = i == chunks.len() - 1;
        // No Content-Length is ever passed to get_miss_handler or write_body
        // -- the total length is learned only once the last chunk lands,
        // exactly as FRE-COMPAT.UNSIZED-01 requires.
        handler
            .write_body(Bytes::copy_from_slice(chunk), eof)
            .await
            .unwrap();
        expected.extend_from_slice(chunk);
    }
    handler.finish().await.unwrap();

    let canonical = shard_path(cache.root(), &key.combined()).unwrap();
    let (header, body) = support::read_and_validate_record(&canonical);
    assert_eq!(header.body_len(), expected.len() as u64);
    assert_eq!(body, expected);
}

#[tokio::test]
async fn tc_compat_unsized_01_irregular_chunk_sizes() {
    store_and_verify(
        "/unsized-irregular",
        &[b"a", b"", b"bcdefg", b"h", b"ijklmnopqrstuvwxyz"],
    )
    .await;
}

#[tokio::test]
async fn tc_compat_unsized_01_empty_body() {
    // A single empty final chunk -- pingora-cache always calls write_body
    // at least once, with eof set on the last (here, only) call, even for
    // a zero-length body.
    store_and_verify("/unsized-empty", &[b""]).await;
}

/// Steps 2 and 4: whatever the body's size and however it is chunked,
/// every chunk is in the store's temporary before its `write_body` returns:
/// after each call the file holds the header, key, and metadata region plus
/// exactly the body bytes accepted so far, so nothing is held back in memory.
#[tokio::test]
async fn tc_compat_unsized_01_every_chunk_is_written_before_write_body_returns() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/unsized-write-through");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let temps = support::temp_entries(cache, &key);
    assert_eq!(temps.len(), 1, "one temporary for the store: {temps:?}");
    let temp = support::shard_dir_of(cache, &key).join(&temps[0]);
    let header = support::read_header(&temp);
    let body_offset = header.body_offset();

    let chunk = vec![0x7Eu8; 33_333];
    let mut expected = Vec::new();
    for i in 0..60 {
        handler
            .write_body(Bytes::copy_from_slice(&chunk), i == 59)
            .await
            .unwrap();
        expected.extend_from_slice(&chunk);
        assert_eq!(
            std::fs::metadata(&temp).unwrap().len(),
            body_offset as u64 + expected.len() as u64,
            "after write_body {i} the temporary holds every byte accepted so far"
        );
    }
    handler.finish().await.unwrap();
    let (_meta, body) = support::lookup_body(cache, &key).await.unwrap();
    assert_eq!(
        body, expected,
        "the persisted body is exactly what was streamed"
    );
}

#[tokio::test]
async fn tc_compat_unsized_01_multi_block_body() {
    // Comfortably spans typical 4 KiB filesystem block boundaries.
    let big_chunk = vec![0xABu8; 9000];
    let chunks: Vec<&[u8]> = vec![
        &big_chunk[..3000],
        &big_chunk[3000..6000],
        &big_chunk[6000..],
    ];
    store_and_verify("/unsized-multiblock", &chunks).await;
}
