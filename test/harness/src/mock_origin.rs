// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Thin re-export: `test/compat` tests spawn `durable-cache-mock-origin`
//! directly via `tokio::spawn` (see the plan §3 — it runs in-process, not
//! as a child, since it needs per-chunk timing control the harness's
//! process-level primitives don't provide).

pub use durable_cache_mock_origin::script::{Chunk, Scenario};
pub use durable_cache_mock_origin::MockOrigin;
