// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.REPLACE-01 — a new store under an occupied key replaces the old object atomically, never torn.
//!
//! Fully covered directly against `Storage::lookup`/`get_miss_handler` in
//! `durable-cache/tests/compat_replace.rs` (no HTTP needed for any
//! of it, including the restart-survival case, since a fresh `FsCache`
//! over the same root is a restart). This file is the end-to-end
//! HTTP-level version through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.REPLACE-01; crate-level coverage already exists in durable-cache/tests/compat_replace.rs"]
fn tc_compliance_replace_01_stores_replace_atomically() {
    unimplemented!()
}
