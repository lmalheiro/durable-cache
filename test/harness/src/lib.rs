// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Test-only infrastructure shared by `test/compat`: process control for
//! `fixture-proxy`, an in-process mock origin, an HTTP client, and
//! filesystem tamper helpers. See the plan §3 for the tool choices and
//! why each one was made.

pub mod fixture_proxy;
pub mod hotupgrade;
pub mod http_client;
pub mod mock_origin;
pub mod process;
pub mod tamper;
