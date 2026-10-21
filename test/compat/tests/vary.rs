// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.VARY-01 — multiple Vary variants of one primary key are stored and correctly selected.
//!
//! Fully closed at the crate level in
//! `durable-cache/tests/compat_vary.rs`: `pingora-cache` folds
//! `Vary`-derived variance into the key before ever calling this backend,
//! so the backend's own obligation -- store and return exactly the object
//! at the presented key, primary or variant, never conflated -- needs no
//! HTTP to exercise. This file is a redundant end-to-end check through
//! `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.VARY-01; crate-level coverage already exists in durable-cache/tests/compat_vary.rs"]
fn tc_compliance_vary_01_vary_variants_stored_and_selected() {
    unimplemented!()
}
