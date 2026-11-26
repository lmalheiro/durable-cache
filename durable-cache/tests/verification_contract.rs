// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-VERIFICATION.CONTRACT-01 — The storage contract admits the backends.
//!
//! The backend, and `MemCache`, the in-memory storage of `pingora-cache`,
//! driven through the workloads of the storage-contract library
//! (`verification/libs/HttpCacheStorage`), each run's trace written for the
//! library's checker. `cargo xtask verify --checker trace` runs the two
//! ignored tests below, then `check-trace` on what they wrote: a call whose
//! result the contract does not allow fails the check.
//!
//! `MemCache` is driven without the calls it cannot take
//! (`Allow::MEMCACHE`); the backend with every call the contract allows.

// IMP-TC.VERIFICATION.CONTRACT-01: satisfy TC-VERIFICATION.CONTRACT-01

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use httpcachestorage_driver::{run, Allow, Options};
use pingora_cache::MemCache;

use super::support;

/// The directory the traces go to.
pub(crate) const TRACE_DIR: &str = "DURABLE_CACHE_TRACE_DIR";
/// How many workloads each storage is driven through, one for each seed.
pub(crate) const TRACE_SEEDS: &str = "DURABLE_CACHE_TRACE_SEEDS";

fn trace_dir() -> PathBuf {
    let dir = PathBuf::from(
        std::env::var_os(TRACE_DIR).expect("DURABLE_CACHE_TRACE_DIR names a directory"),
    );
    std::fs::create_dir_all(&dir).expect("the trace directory is created");
    dir
}

fn seeds() -> u64 {
    std::env::var(TRACE_SEEDS).map_or(8, |s| {
        s.parse().expect("DURABLE_CACHE_TRACE_SEEDS is a number")
    })
}

fn trace_file(name: &str, seed: u64) -> BufWriter<File> {
    let path = trace_dir().join(format!("{name}-{seed}.jsonl"));
    BufWriter::new(File::create(&path).expect("the trace file is created"))
}

/// Step 2: `MemCache`, through the workloads it can take.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "cargo xtask verify --checker trace runs it, then checks the traces"]
async fn tc_verification_contract_01_memcache_traces() {
    for seed in 0..seeds() {
        let storage: &'static MemCache = Box::leak(Box::new(MemCache::new()));
        let summary = run(
            storage,
            &Options::new(seed, Allow::MEMCACHE),
            trace_file("memcache", seed),
        )
        .await
        .expect("the trace is written");
        assert!(
            summary.hits > 0 && summary.stored > 0,
            "seed {seed}: {summary:?}"
        );
    }
}

/// Step 3: the backend, through every call the contract allows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "cargo xtask verify --checker trace runs it, then checks the traces"]
async fn tc_verification_contract_01_backend_traces() {
    for seed in 0..seeds() {
        let (cache, _dir) = support::leaked_cache();
        let summary = run(
            cache,
            &Options::new(seed, Allow::ALL),
            trace_file("backend", seed),
        )
        .await
        .expect("the trace is written");
        assert!(
            summary.hits > 0 && summary.stored > 0,
            "seed {seed}: {summary:?}"
        );
    }
}
