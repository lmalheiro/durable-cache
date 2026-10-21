// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.CONCURRENT-01 — concurrent same-key writes never interleave and converge on a single winner.
//!
//! Fully covered directly against `Storage`, same-process (step 4's own
//! scenario), across
//! `durable-cache/tests/storage_publish.rs`'s
//! `tc_storage_publish_01_concurrent_same_key_writers_stay_isolated`
//! (steps 1, 3, 4) and `durable-cache/tests/compat_concurrent.rs`
//! (step 2: lookups while both stores are in flight). This file is the
//! end-to-end HTTP-level version through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPAT.CONCURRENT-01; crate-level coverage already exists in durable-cache/tests/storage_publish.rs and compat_concurrent.rs"]
fn tc_compat_concurrent_01_concurrent_same_key_writes_stay_isolated() {
    unimplemented!()
}
