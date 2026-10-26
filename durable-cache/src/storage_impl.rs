// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! [`FsCache`]: the crate's `Storage` implementation, gluing together
//! `format`/`record`/`sharding`/`dircache`/`inflight`/`publish`/`miss`/
//! `lookup`/`metaupdate`/`purge`/`reclaim`.

use std::any::Any;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::{
    HitHandler, MissHandler, PurgeOutcome, PurgeTarget, PurgeType, Storage,
};
use pingora_cache::trace::SpanHandle;
use pingora_cache::{CacheKey, CacheMeta};
use pingora_error::{ErrorType, OrErr, Result};

use crate::config::FsCacheConfig;
use crate::dircache::ShardDirCache;
#[cfg(test)]
use crate::hooks::Hooks;
use crate::inflight::InFlightRegistry;
use crate::locks::ChangeLocks;
use crate::writerid::WriterLease;
use crate::{metrics, sharding};

pub struct FsCache {
    pub(crate) config: FsCacheConfig,
    pub(crate) dir_cache: ShardDirCache,
    pub(crate) inflight: InFlightRegistry,
    /// The read-only gate and the publication locks: every change to the
    /// cache directory begins here (`locks.begin(name)`).
    pub(crate) locks: ChangeLocks,
    /// This instance's lease, held locked for its whole life; its identity
    /// is in every temporary's name (ADR-STORAGE.WRITERID-01).
    pub(crate) lease: WriterLease,
    /// The test hooks of this instance; only in the crate's own test build.
    #[cfg(test)]
    pub(crate) hooks: Hooks,
}

/// With `FsCacheConfig::with_noatime`, check that the kernel lets this
/// process open the cache's files without updating their access time: it
/// allows `O_NOATIME` only to a file's owner, and the owner of the root
/// stands for the owner of the files under it.
#[cfg(target_os = "linux")]
fn check_noatime(root: &Path) -> Result<()> {
    crate::nowait::open_blocking(root, true)
        .map(drop)
        .or_err_with(ErrorType::FileOpenError, || {
            format!(
                "opening {} without updating its access time, as FsCacheConfig::with_noatime asks",
                root.display()
            )
        })
}

/// `FsCacheConfig::with_noatime` needs Linux's `O_NOATIME`.
#[cfg(not(target_os = "linux"))]
fn check_noatime(_root: &Path) -> Result<()> {
    pingora_error::Error::e_explain(
        ErrorType::InternalError,
        "FsCacheConfig::with_noatime needs O_NOATIME, which only Linux has",
    )
}

impl FsCache {
    /// Build a cache rooted at `config.root()`. Runs FDS-LAYOUT.STARTUP-01's
    /// construction-time shard scan; does *not* run Eviction Recovery
    /// (see [`crate::recover`]) — that's a separate call the
    /// integrator makes explicitly, since it needs the `EvictionManager`
    /// instance too, which this constructor has no opinion about.
    ///
    /// With the `metrics` feature, it also registers the backend's counters,
    /// at zero (ADR-OBSERVABILITY.METRICS-01).
    ///
    /// Fails with a `FileCreateError` if the shard tree cannot be created or
    /// listed under the root, or this instance's lease cannot be created and
    /// locked (ADR-STORAGE.WRITERID-01). With
    /// [`FsCacheConfig::with_noatime`], it also fails with a `FileOpenError`
    /// if the kernel refuses to open the root without updating its access
    /// time, as it does when this process does not own it, and with an
    /// `InternalError` on a platform without `O_NOATIME`.
    pub fn new(config: FsCacheConfig) -> Result<Self> {
        let dir_cache = ShardDirCache::build(config.root())
            .or_err_with(ErrorType::FileCreateError, || {
                format!("creating the shard tree under {}", config.root().display())
            })?;
        if config.noatime() {
            check_noatime(config.root())?;
        }
        let lease = WriterLease::acquire(config.root())
            .or_err_with(ErrorType::FileCreateError, || {
                format!("creating a writer lease under {}", config.root().display())
            })?;
        // With the `metrics` feature, the counters are served from here on,
        // at zero, rather than from their first event.
        metrics::register();
        Ok(FsCache {
            config,
            dir_cache,
            inflight: InFlightRegistry::new(),
            locks: ChangeLocks::default(),
            lease,
            #[cfg(test)]
            hooks: Hooks::default(),
        })
    }

    /// [`Self::new`] with test hooks (see [`crate::hooks`]).
    #[cfg(test)]
    pub fn with_test_hooks(config: FsCacheConfig, hooks: crate::hooks::HookConfig) -> Result<Self> {
        let mut cache = Self::new(config)?;
        cache.hooks = Hooks::new(hooks);
        Ok(cache)
    }

    pub fn root(&self) -> &Path {
        self.config.root()
    }

    /// The runtime test hooks of this instance.
    #[cfg(test)]
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Make the backend read-only: it keeps reading the shared cache but
    /// makes no further durable change to it (FDS-COMPAT.READONLY-01). The
    /// integrator calls this when its server announces
    /// `ExecutionPhase::GracefulUpgradeTransferringFds`, while the process
    /// drains its sessions. Callable from any thread and idempotent; when it
    /// returns, no durable change that began before it is still running,
    /// and none can begin after it. A restart scan ([`crate::recover`])
    /// still running, or run later, then admits nothing: the eviction
    /// manager it would fill is discarded when the process exits.
    pub fn set_read_only(&self) {
        self.locks.set_read_only();
        log::info!("read-only; no further durable changes will be made");
    }

    /// Whether [`Self::set_read_only`] has been called.
    pub fn is_read_only(&self) -> bool {
        self.locks.is_read_only()
    }

    /// The name of a key's files ([`sharding::object_name`]): its combined
    /// hash in hex, or, for a key with a user tag, a hash of that hash and
    /// the tag. Every path in the backend is derived through here, so the
    /// test hash hook (TC-LAYOUT.SHARDING-01) applies uniformly to lookups,
    /// stores, metadata updates, expiries and purges.
    pub(crate) fn object_name<K: CacheHashKey>(&self, key: &K) -> String {
        self.map_name(sharding::object_name(key.combined_bin(), key.user_tag()))
    }

    #[cfg(test)]
    fn map_name(&self, name: String) -> String {
        match &self.hooks.config.object_name {
            Some(f) => f(&name),
            None => name,
        }
    }

    #[cfg(not(test))]
    fn map_name(&self, name: String) -> String {
        name
    }

    pub(crate) fn canonical_path(&self, name: &str) -> Result<PathBuf> {
        sharding::shard_path(self.root(), name)
            .or_err(ErrorType::InternalError, "deriving a shard path")
    }

    pub(crate) fn shard_dir(&self, name: &str) -> Result<PathBuf> {
        sharding::shard_dir(self.root(), name)
            .or_err(ErrorType::InternalError, "deriving a shard path")
    }

    /// Deserialise a record's metadata, counting the decode so
    /// TC-COMPAT.FORMATUPGRADE-01 can show a foreign record is never decoded.
    pub(crate) fn decode_meta(&self, internal: &[u8], header: &[u8]) -> Result<CacheMeta> {
        test_hook! { self.hooks.count_meta_decode(); }
        CacheMeta::deserialize(internal, header)
    }

    /// `fsync` a shard directory, so an `unlink` in it survives a host crash
    /// (ADR-PERSISTENCE.DURABILITY-01). On Unix a directory opens like a file,
    /// read-only, and `fsync` on its descriptor flushes its entries.
    pub(crate) fn sync_shard_dir(&self, dir: &Path) -> Result<()> {
        test_hook! { self.hooks.fault(crate::hooks::FaultPoint::SyncDir)?; }
        std::fs::File::open(dir)
            .and_then(|dir_fd| dir_fd.sync_all())
            .or_err_with(ErrorType::FileWriteError, || {
                format!("synchronising {}", dir.display())
            })
    }
}

#[async_trait]
impl Storage for FsCache {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        crate::lookup::lookup(self, key, None)
            .await
            .map(|opt| opt.map(|(meta, handler)| (meta, Box::new(handler) as HitHandler)))
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        _trace: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        crate::lookup::lookup(self, key, streaming_write_tag)
            .await
            .map(|opt| opt.map(|(meta, handler)| (meta, Box::new(handler) as HitHandler)))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<MissHandler> {
        // Hash the key and serialise the metadata here (pure CPU), then let
        // the blocking pool create the temporary and write the record's
        // fixed part (FDS-STORAGE.EXECUTOR-01).
        let prepared = crate::miss::FsMissHandler::prepare(self, key, meta)?;
        crate::blocking::run(move || crate::miss::FsMissHandler::open(self, prepared))
            .await
            .map(|h| Box::new(h) as MissHandler)
    }

    async fn purge(
        &'static self,
        target: PurgeTarget<'_>,
        purge_type: PurgeType,
        _trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        // An eviction and an invalidation unlink the same canonical name,
        // but only an invalidation ends the key's stores in flight and
        // synchronises the shard directory (FDS-STORAGE.PURGE-01). The two
        // target kinds do not differ here:
        // this backend publishes one object per key and assigns no entry
        // identifier, so an Active target and a key-only Exact target name
        // the same file (ADR-COMPAT.ENTRYID-01).
        //
        // The target is resolved here because it borrows the caller's key;
        // the unlink and any `fsync` behind it go to the blocking pool
        // (FDS-STORAGE.EXECUTOR-01).
        let Some(key) = crate::purge::resolve(target) else {
            return Ok(PurgeOutcome::NotFound);
        };
        let key = key.clone();
        crate::blocking::run(move || crate::purge::purge_key(self, &key, purge_type)).await
    }

    async fn expire(
        &'static self,
        target: PurgeTarget<'_>,
        _trace: &SpanHandle,
    ) -> Result<PurgeOutcome> {
        // Implemented rather than inherited: the trait's default deletes the
        // object, which would discard a body this cache is still holding and
        // is exactly the refetch an expiring purge exists to avoid
        // (ADR-STORAGE.EXPIRE-01).
        let Some(key) = crate::purge::resolve(target) else {
            return Ok(PurgeOutcome::NotFound);
        };
        let key = key.clone();
        crate::blocking::run(move || crate::expire::expire_key(self, &key)).await
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _trace: &SpanHandle,
    ) -> Result<bool> {
        // Serialising the new metadata is CPU work and stays here; reading
        // the record and writing it back does not
        // (FDS-STORAGE.EXECUTOR-01).
        let prepared = crate::metaupdate::prepare(self, key, meta)?;
        crate::blocking::run(move || crate::metaupdate::update_meta(self, prepared)).await
    }

    fn support_streaming_partial_write(&self) -> bool {
        // FRE-COMPAT.STREAMFLAG-01.
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}
