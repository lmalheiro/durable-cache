// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.EVICTION-01 — after a restart, the eviction manager's view matches the persisted object set exactly.
//!
//! Fully covered directly against `Storage`/`recovery::recover` in
//! `durable-cache/tests/compat_eviction.rs` (no HTTP needed,
//! including the restart and the removed-object-doesn't-reappear cases).
//! This file is the end-to-end HTTP-level version through `fixture-proxy`,
//! left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPAT.EVICTION-01; crate-level coverage already exists in durable-cache/tests/compat_eviction.rs"]
fn tc_compat_eviction_01_exact_eviction_state_after_restart() {
    unimplemented!()
}
