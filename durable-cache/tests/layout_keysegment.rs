// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-LAYOUT.KEYSEGMENT-01 — Key segment round trip.
//!
//! Steps 1 and 2 exercise the encoder and decoder directly, as unit tests
//! in `src/key_codec.rs`. This file covers step 3: a record whose stored
//! key does not decode is reclaimed by the restart-time scan as damaged,
//! and never admitted.

use durable_cache::recovery::recover;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;

use super::support;

// IMP-TC.LAYOUT.KEYSEGMENT-01: satisfy TC-LAYOUT.KEYSEGMENT-01

/// Step 3: the record still passes every record check, its checksums
/// resealed over the changed key, so only decoding the key can reject it.
#[tokio::test]
async fn tc_layout_keysegment_01_an_undecodable_key_is_reclaimed_as_damaged() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/undecodable-key");
    support::store(cache, &key, &support::meta(), b"body").await;
    let path = support::canonical_path(cache, &key);
    support::set_key_variance_flag(&path, 2);

    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.unwrap();

    assert_eq!(report.reclaimed_damaged, 1, "the record with the bad key");
    assert!(!path.exists(), "reclaimed from disk");
    assert_eq!(report.admitted, 0);
    assert!(!manager.peek(&support::entry(&key)), "never admitted");
}
