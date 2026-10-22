// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! A file-system-backed, durable storage backend for Pingora®: an
//! implementation of the `pingora-cache` `Storage` trait, for the Pingora
//! release its manifest requires.
//!
// The specification of this very version: the link names its release tag.
#![doc = concat!(
    "The [specification](https://github.com/lmalheiro/durable-cache/blob/v",
    env!("CARGO_PKG_VERSION"),
    "/docs/durable-cache.pdf) this crate implements is in its repository;"
)]
//! module-level docs cite the specific requirement IDs (`ADR-*`, `FDS-*`,
//! `SDS-*`) each one realises.
//!
//! Construction: [`FsCache::new`] builds the cache itself. Separately, call
//! [`recover`] once at process start to reconstruct the
//! `EvictionManager`'s view from durable state (ADR-COMPAT.EVICTION-01) —
//! it is not run automatically by `new`, since it needs the
//! `EvictionManager` instance, which this crate has no opinion about. On a
//! Pingora server, [`hotupgrade::install`] does that and makes the backend
//! read-only on a hot upgrade, and is required for hot upgrades
//! (FDS-COMPAT.READONLY-01).
//!
//! **Deployment constraint.** Every process sharing one cache directory
//! must run on the same host, with the directory on a local file system:
//! each backend instance holds a lease under the root, locked with `flock`, and the restart-time scan decides whether a
//! temporary's writer is still running by trying that lock
//! (ADR-STORAGE.WRITERID-01). Processes in different PID namespaces, such as
//! containers on one host, may share a directory.
//!
//! **Logging.** The crate logs through the `log` crate. Each record's target
//! is the module that wrote it, under `durable_cache` (for example
//! `durable_cache::recovery`), and its messages carry no prefix of their
//! own: keep the target in the log format to tell them apart, and filter on
//! it, as in `RUST_LOG=durable_cache=warn`.
//!
//! Pingora is a trademark or registered trademark of Cloudflare, Inc. This
//! crate is an independent project, not affiliated with, endorsed by, or
//! sponsored by Cloudflare, Inc.

/// Run a test hook (see `src/hooks.rs`): the body is compiled only into this
/// crate's own test build. Everything in it, the arguments a hook call
/// computes included, is absent from any other build, so a hook can never
/// cost a production build anything or block it. The body is a block: a
/// `?` inside it returns from the enclosing function, and a `let` inside it
/// is not visible after it.
macro_rules! test_hook {
    ($($body:tt)*) => {
        #[cfg(test)]
        {
            $($body)*
        }
    };
}

mod blocking;
mod config;
pub(crate) mod dircache;
mod error;
pub(crate) mod expire;
pub(crate) mod format;
#[cfg(test)]
pub(crate) mod hooks;
pub mod hotupgrade;
pub(crate) mod inflight;
pub(crate) mod key_codec;
pub(crate) mod locks;
pub(crate) mod lookup;
pub(crate) mod metaupdate;
pub(crate) mod metrics;
pub(crate) mod miss;
#[cfg(target_os = "linux")]
pub(crate) mod nowait;
pub(crate) mod publish;
pub(crate) mod purge;
pub(crate) mod reclaim;
pub(crate) mod record;
mod recovery;
pub(crate) mod sharding;
mod storage_impl;
pub(crate) mod writerid;

pub use config::FsCacheConfig;
pub use recovery::{recover, RecoveryReport};
pub use storage_impl::FsCache;

/// Internals the workspace's test crates (`test/harness`, `test/compat`,
/// `test/fixture-proxy`) use to tamper with records, inspect temporaries,
/// and run Eviction Recovery their own way. Not API: it exists only with
/// the `test-support` feature, which those crates enable, and is exempt
/// from semver.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    pub use crate::format::HEADER_LEN;
    pub use crate::writerid::{LEASE_DIR, TEMP_SUFFIX};

    /// The hot-upgrade helper's upgrade watcher alone, for a server that
    /// runs Eviction Recovery itself.
    pub fn read_only_on_upgrade(
        server: &pingora_core::server::Server,
        cache: &'static crate::FsCache,
    ) -> impl pingora_core::services::ServiceWithDependents + 'static {
        crate::hotupgrade::read_only_on_upgrade(server, cache)
    }
}

// The test suite in `tests/`. It is compiled into this crate's own unit-test
// binary rather than as separate test crates (`autotests = false` in the
// manifest), so the tests can use the `cfg(test)` hooks in [`hooks`] and no
// build other than `cargo test` of this crate ever contains them. The files
// reach the crate through its own name, as an external crate would; being
// inside it, they can also use its crate-private modules.
#[cfg(test)]
extern crate self as durable_cache;

#[cfg(test)]
#[path = "../tests"]
mod integration_tests {
    mod support;

    mod compat_abortspace;
    mod compat_body;
    mod compat_concurrent;
    mod compat_eviction;
    mod compat_formatupgrade;
    mod compat_freshness;
    mod compat_headers;
    mod compat_hotupgrade;
    mod compat_incomplete;
    mod compat_purge;
    mod compat_replace;
    mod compat_revalidation;
    mod compat_revalidation_observability;
    mod compat_streaming;
    mod compat_trailers;
    mod compat_unsized;
    mod compat_validators;
    mod compat_vary;
    mod compat_writefail;
    mod eviction_reconstruct;
    mod executor_nonblocking;
    mod layout_dircache;
    mod layout_keysegment;
    mod layout_objectfile;
    mod layout_sharding;
    mod measurements;
    mod persistence_crashsafe;
    mod storage_expire;
    mod storage_publish;
    mod storage_purge;
    mod storage_reclaim;
    mod storage_reclaim_reporting;
}
