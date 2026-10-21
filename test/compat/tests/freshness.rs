// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.FRESHNESS-01 — freshness/age computation inputs survive a store+restart round trip.
//!
//! Fully closed at the crate level in
//! `durable-cache/tests/compat_freshness.rs`: the raw HTTP inputs
//! this TC names are already interpreted into `CacheMeta`'s numeric fields
//! before `pingora-cache` ever calls this backend, so the backend's own
//! obligation -- round-tripping those numeric fields and the raw header
//! bytes exactly -- needs no HTTP to exercise. This file is a redundant
//! end-to-end check through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.FRESHNESS-01; crate-level coverage already exists in durable-cache/tests/compat_freshness.rs"]
fn tc_compliance_freshness_01_freshness_and_age_inputs_preserved() {
    unimplemented!()
}
