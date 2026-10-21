// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.VALIDATORS-01 — ETag/Last-Modified validators survive a store+restart round trip.
//!
//! Fully closed at the crate level in
//! `durable-cache/tests/compat_validators.rs` (no HTTP needed for a
//! `CacheMeta`/`ResponseHeader` round-trip property). This file is a
//! redundant end-to-end HTTP-level check through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.VALIDATORS-01; crate-level coverage already exists in durable-cache/tests/compat_validators.rs"]
fn tc_compliance_validators_01_validator_fields_preserved() {
    unimplemented!()
}
