// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.BODY-01 — stored body bytes are returned unchanged, including partial-store/partial-reuse variants.
//!
//! Fully closed at the crate level in
//! `durable-cache/tests/compat_body.rs` (boundary-size round trips,
//! a restart, and the seeked-read step: a published record does advertise
//! `can_seek`, and each seeked read is compared against the stored slice).
//! This file is a redundant end-to-end HTTP-level check through
//! `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.BODY-01; crate-level coverage already exists in durable-cache/tests/compat_body.rs"]
fn tc_compliance_body_01_body_round_trips_byte_for_byte() {
    unimplemented!()
}
