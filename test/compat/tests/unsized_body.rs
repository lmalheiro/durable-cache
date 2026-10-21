// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.UNSIZED-01 — a body of unknown length (chunked, no Content-Length) is accepted and persisted exactly.
//!
//! The underlying write path is implemented and covered directly against
//! `Storage::get_miss_handler` in `durable-cache/tests/compat_unsized.rs`
//! (no HTTP needed for that). This file is the end-to-end HTTP-level
//! version through `fixture-proxy`, left unimplemented.

#[test]
#[ignore = "redundant end-to-end duplicate, not implemented: fixture-proxy's cache wiring does now exist (exercised by headers.rs, incomplete.rs and streaming.rs) and this TC is already fully closed at the crate level — see docs/durable-cache-requirements.json TC-COMPAT.UNSIZED-01; crate-level coverage already exists in durable-cache/tests/compat_unsized.rs"]
fn tc_compat_unsized_01_unsized_body_accepted_and_persisted() {
    unimplemented!()
}
