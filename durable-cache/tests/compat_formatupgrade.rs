// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.FORMATUPGRADE-01 — Foreign records are misses.
//!
//! A record written under another record format revision is reported as a
//! miss and never decoded, while one written under the current revision is
//! returned exactly. The `pingora-cache` release does not decide that: this
//! backend decides when its format changes. The golden-bytes test pins the
//! metadata encoding the current revision relies on, so a release that
//! changes it is flagged instead of silently changing what stored records
//! mean.
//!
//! The header compression dictionary is not part of it either: a
//! deployment that changes the dictionary clears the cache directory or uses
//! a new one (FRE-COMPAT.FORMATUPGRADE-01). A record of another revision is
//! forged by rewriting its version field and resealing its header checksum.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use durable_cache::recovery::recover;
use durable_cache::{format, FsCache};
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;

use super::support;

// IMP-TC.COMPAT.FORMATUPGRADE-01: satisfy TC-COMPAT.FORMATUPGRADE-01

fn cache_at(root: &Path) -> &'static FsCache {
    support::leaked_cache_at(root)
}

fn keys() -> Vec<pingora_cache::CacheKey> {
    (0..4)
        .map(|i| support::key(&format!("/format-{i}")))
        .collect()
}

async fn store_set(cache: &'static FsCache, keys: &[pingora_cache::CacheKey]) {
    for (i, key) in keys.iter().enumerate() {
        support::store(cache, key, &support::meta(), format!("body {i}").as_bytes()).await;
    }
}

/// Steps 1 and 2: the same format revision reads back every object
/// exactly as stored.
#[tokio::test]
async fn tc_compat_formatupgrade_01_same_format_reads_every_object() {
    let tmp = support::tempdir();
    let keys = keys();
    store_set(cache_at(tmp.path()), &keys).await;

    let reopened = cache_at(tmp.path());
    for (i, key) in keys.iter().enumerate() {
        let (_meta, body) = support::lookup_body(reopened, key)
            .await
            .expect("a record written under the same revision is a hit");
        assert_eq!(body, format!("body {i}").as_bytes());
    }
}

/// Step 3: a record of another format revision is a miss, never decoded,
/// and the restart procedure reclaims it. The directory then simply turns
/// over.
#[tokio::test]
async fn tc_compat_formatupgrade_01_another_format_revision_is_a_miss_never_decoded() {
    let tmp = support::tempdir();
    let keys = keys();
    let writer = cache_at(tmp.path());
    store_set(writer, &keys).await;
    for key in &keys {
        support::set_format_version(
            &support::canonical_path(writer, key),
            format::FORMAT_VERSION + 1,
        );
    }

    let reader = cache_at(tmp.path());
    let decodes_before = reader.hooks().meta_decodes();
    for key in &keys {
        assert!(
            support::lookup_body(reader, key).await.is_none(),
            "a record of another format revision must be a miss"
        );
    }
    assert_eq!(
        reader.hooks().meta_decodes(),
        decodes_before,
        "its metadata must never be decoded"
    );

    let manager = Manager::new(usize::MAX);
    let report = recover(reader, &manager).await.unwrap();
    assert_eq!(report.reclaimed_damaged, keys.len() as u64);
    assert_eq!(report.admitted, 0);

    let key = support::key("/format-after-upgrade");
    support::store(reader, &key, &support::meta(), b"written by this revision").await;
    assert_eq!(
        support::lookup_body(reader, &key).await.unwrap().1,
        b"written by this revision"
    );
}

/// Metadata with fixed times and headers, so its encoding is the same on
/// every run (step 4).
fn golden_meta() -> CacheMeta {
    let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
    let mut header = ResponseHeader::build(200, None).unwrap();
    header.append_header("content-type", "text/plain").unwrap();
    header.append_header("etag", "\"golden\"").unwrap();
    CacheMeta::new(at(1_800_000_600), at(1_800_000_000), 30, 60, header)
}

/// The `pingora-cache` encoding of [`golden_meta`] that the current record
/// format relies on: its internal and header segments. Step 4 checks the
/// release in use still produces it.
const GOLDEN_INTERNAL: &[u8] = &[
    0x99, 0x02, 0x92, 0xce, 0x6b, 0x49, 0xd4, 0x58, 0x00, 0x92, 0xce, 0x6b, 0x49, 0xd2, 0x00, 0x00,
    0x92, 0xce, 0x6b, 0x49, 0xd2, 0x00, 0x00, 0x1e, 0x3c, 0xc0, 0xc0, 0x92, 0xce, 0x6b, 0x49, 0xd2,
    0x00, 0x00,
];
const GOLDEN_HEADER: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x3d, 0xe9, 0x01, 0x00, 0x48, 0x54, 0x54, 0x50, 0x2f, 0x31, 0x2e,
    0x31, 0x20, 0x32, 0x30, 0x30, 0x20, 0x4f, 0x4b, 0x0d, 0x0a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e,
    0x74, 0x2d, 0x74, 0x79, 0x70, 0x65, 0x3a, 0x20, 0x74, 0x65, 0x78, 0x74, 0x2f, 0x70, 0x6c, 0x61,
    0x69, 0x6e, 0x0d, 0x0a, 0x65, 0x74, 0x61, 0x67, 0x3a, 0x20, 0x22, 0x67, 0x6f, 0x6c, 0x64, 0x65,
    0x6e, 0x22, 0x0d, 0x0a, 0x0d, 0x0a,
];

const ENCODING_CHANGED: &str = "the pingora-cache in use encodes CacheMeta differently \
    from the bytes the current record format relies on. Decide whether records written \
    before still read back exactly under this release: if not, bump \
    format::FORMAT_VERSION (the cache then starts cold). Either way, regenerate \
    GOLDEN_INTERNAL and GOLDEN_HEADER from this release. The expiry relies on the \
    encoding too: expire.rs's unit tests check what it relies on";

/// Step 4: the metadata encoding the current record format relies on is the
/// one this `pingora-cache` release reads and writes, in both directions. A
/// release that changes it fails here, so an update never changes what
/// stored records mean unnoticed (FRE-COMPAT.FORMATUPGRADE-01).
#[test]
fn tc_compat_formatupgrade_01_pingora_cache_keeps_the_formats_metadata_encoding() {
    let expected = golden_meta();
    let (internal, header) = expected.serialize().unwrap();
    assert_eq!(
        (internal.as_slice(), header.as_slice()),
        (GOLDEN_INTERNAL, GOLDEN_HEADER),
        "{ENCODING_CHANGED}"
    );

    let decoded = CacheMeta::deserialize(GOLDEN_INTERNAL, GOLDEN_HEADER).expect(ENCODING_CHANGED);
    assert_eq!(decoded.created(), expected.created(), "{ENCODING_CHANGED}");
    assert_eq!(decoded.updated(), expected.updated(), "{ENCODING_CHANGED}");
    assert_eq!(
        decoded.provenance(),
        expected.provenance(),
        "{ENCODING_CHANGED}"
    );
    assert_eq!(
        decoded.fresh_until(),
        expected.fresh_until(),
        "{ENCODING_CHANGED}"
    );
    assert_eq!(
        decoded.stale_while_revalidate_sec(),
        expected.stale_while_revalidate_sec(),
        "{ENCODING_CHANGED}"
    );
    assert_eq!(
        decoded.stale_if_error_sec(),
        expected.stale_if_error_sec(),
        "{ENCODING_CHANGED}"
    );
    assert_eq!(
        decoded.response_header_copy().status,
        expected.response_header_copy().status,
        "{ENCODING_CHANGED}"
    );
    assert_eq!(decoded.headers(), expected.headers(), "{ENCODING_CHANGED}");
}
