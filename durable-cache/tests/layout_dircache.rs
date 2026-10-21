// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-LAYOUT.DIRCACHE-01 — Shard cache matches disk at start.
//!
//! Full scenario coverage (pre-population, concurrent first-time stores,
//! restart) lives as unit tests in `src/dircache.rs`. This file is the
//! public-API-level, TC-ID-traceable entry point.

use durable_cache::dircache::ShardDirCache;

// IMP-TC.LAYOUT.DIRCACHE-01: satisfy TC-LAYOUT.DIRCACHE-01
#[test]
fn tc_layout_dircache_01_construction_creates_256_first_level_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    ShardDirCache::build(tmp.path()).unwrap();
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 256);
}

#[test]
fn tc_layout_dircache_01_ensure_shard_dir_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = ShardDirCache::build(tmp.path()).unwrap();
    let hash = "ab12xxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    cache.ensure_shard_dir(tmp.path(), hash).unwrap();
    cache.ensure_shard_dir(tmp.path(), hash).unwrap(); // must not error the second time
    assert!(tmp.path().join("ab").join("12").is_dir());
}
