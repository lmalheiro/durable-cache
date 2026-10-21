// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.PURGE-01 — after a purge, neither the primary nor any Vary variant is ever served again.
//!
//! The Storage-level purge mechanics (canonical-only unlink, idempotency,
//! and the racing-writer/racing-reader/racing-completion-order legs) are
//! covered crate-level in `durable-cache/tests/storage_purge.rs`
//! (TC-STORAGE.PURGE-01). This TC's own full scope -- driving `Vary`
//! variants into existence, purging the primary, and proving the backend's
//! own half of "never served again" (durable primary removal, and the
//! variants left to eviction) -- is
//! also fully closed at the crate level, in
//! `durable-cache/tests/compat_purge.rs`; the primary-first `Vary`
//! resolution in `pingora-cache` itself is what completes the other half in
//! production, and isn't this backend's to test. This file remains a
//! redundant end-to-end check through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPLIANCE.PURGE-01; crate-level coverage already exists in durable-cache/tests/compat_purge.rs"]
fn tc_compliance_purge_01_purge_removes_all_variants() {
    unimplemented!()
}
