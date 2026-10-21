// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.TRAILERS-01 — Trailer fields kept distinct.
//!
//! Step 1 of this TC's spec ("no trailer reaches the backend") is a fact
//! about the `Storage`/`HandleMiss` trait signatures themselves, not
//! something a test can exercise: `get_miss_handler`'s `meta: &CacheMeta`
//! carries header fields only, and `HandleMiss::write_body`'s `data: Bytes`
//! carries body bytes only — neither has any parameter a trailer could ever
//! travel through, so the interface itself discharges that half of
//! FRE-COMPLIANCE.TRAILERS-01 by construction, independent of this crate's
//! own implementation.
//!
//! Step 2 is conditional on this backend's own on-disk format defining a
//! trailer section — it doesn't (`format.rs`: header, key, metadata region,
//! then body, nothing after). The test below proves that structurally: a
//! published record's on-disk length is exactly the body's end offset, with
//! no further section a trailer could occupy or be confused with.

use bytes::Bytes;
use durable_cache::sharding::shard_path;
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.COMPLIANCE.TRAILERS-01: satisfy TC-COMPLIANCE.TRAILERS-01

#[tokio::test]
async fn tc_compliance_trailers_01_no_section_exists_beyond_the_body() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/trailers-no-section");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(
            Bytes::from_static(b"body only, no trailer channel exists"),
            true,
        )
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let canonical = shard_path(cache.root(), &key.combined()).unwrap();
    let (header, body) = support::read_and_validate_record(&canonical);
    let actual_len = std::fs::metadata(&canonical).unwrap().len() as usize;

    assert_eq!(
        actual_len,
        header.body_offset() + body.len(),
        "the record's on-disk length must end exactly at the body's last byte -- \
         no trailer section, or anything else, follows it"
    );

    // The returned header set is exactly what was stored -- nothing a
    // trailer could have been folded into appears that wasn't given.
    let (found_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    assert_eq!(
        found_meta.response_header().headers.len(),
        meta.response_header().headers.len()
    );
    let mut read_body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        read_body.extend_from_slice(&chunk);
    }
    assert_eq!(read_body, b"body only, no trailer channel exists");
}
