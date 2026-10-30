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

use std::path::{Path, PathBuf};

/// A fresh temporary directory for a cache. It lies under the build's target
/// directory, on disk, rather than in the system's temporary directory:
/// page-cache reads, on by default on Linux, need a file system with
/// non-blocking reads, which tmpfs lacks (`FsCacheConfig::with_page_cache_reads`).
pub fn cache_dir() -> tempfile::TempDir {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let base = target.join("test-tmp");
    std::fs::create_dir_all(&base).expect("a directory for temporary caches");
    tempfile::tempdir_in(&base).expect("a temporary directory for a cache")
}
