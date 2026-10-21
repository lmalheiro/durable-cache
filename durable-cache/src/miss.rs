// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `Storage::get_miss_handler` / `HandleMiss` — the write side
//! (FDS-STORAGE.PUBLISH-01).
//!
//! A store writes its own per-writer temporary in the canonical name's own
//! directory: the header, key, and metadata region when the miss handler
//! opens (the `CacheMeta` is available then), then the body chunk by chunk,
//! each folded into a running CRC-32C unless the build leaves out the
//! `body-crc` feature. `finish` patches `body_len` and `body_crc`,
//! synchronises the file, and renames it over the canonical name. A store
//! that never finishes takes its temporary with it, and so does one that an
//! invalidating purge of its key ended while it was in flight.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use pingora_cache::eviction::CacheEntryId;
use pingora_cache::storage::streaming_write::U64WriteId;
use pingora_cache::storage::{HandleMiss, MissFinishType};
use pingora_cache::{CacheKey, CacheMeta};
use pingora_error::{Error, ErrorType, OrErr, Result};
use zerocopy::IntoBytes;

use crate::error::STORE_PURGED;
use crate::format::{self, RecordHeader};
#[cfg(test)]
use crate::hooks::FaultPoint;
use crate::inflight::{InFlightWriter, WriteOutcome};
use crate::{blocking, key_codec, publish, sharding, FsCache};

/// How many counter values a store tries before giving up on finding a free
/// temporary name (FDS-STORAGE.PUBLISH-01, "advance the counter and retry").
/// A name carries this instance's own identity and counter, so no other
/// store of this backend can hold it: only a file something else put there
/// under such a name can, and each retry steps past one. Sixteen in a row
/// mean something is deliberately in the way.
const NAME_ATTEMPTS: u32 = 16;

/// What [`FsMissHandler::prepare`] hands to [`FsMissHandler::open`]: owned,
/// `Send` data, so the store's file work can run on the blocking pool.
pub(crate) struct PreparedStore {
    name: String,
    key_bytes: Vec<u8>,
    internal_bytes: Vec<u8>,
    header_bytes: Vec<u8>,
}

// IMP-STORAGE.PUBLISH-01: satisfy FDS-STORAGE.PUBLISH-01
pub struct FsMissHandler {
    cache: &'static FsCache,
    /// The temporary. Shared so each body write can run on the blocking
    /// pool; only this handler ever writes to it. Taken only as the handler
    /// is dropped, to be closed off the dropping thread (see `Drop`).
    file: Option<Arc<File>>,
    temp_path: PathBuf,
    canonical_path: PathBuf,
    name: String,
    header: RecordHeader,
    /// CRC-32C over the body bytes accepted so far
    /// (ADR-PERSISTENCE.DURABILITY-01); stays zero in a build without the
    /// `body-crc` feature.
    body_crc: u32,
    /// Body bytes written to the temporary so far. Every chunk is written
    /// before its `write_body` returns (FRE-COMPAT.UNSIZED-01).
    body_written: u64,
    writer: Arc<InFlightWriter>,
    /// The tag `pingora-cache` passes back when the request that opened this
    /// store looks it up, so that lookup resolves to this very store.
    write_tag: U64WriteId,
    /// Set once `finish` completes, so `Drop` neither aborts the store nor
    /// removes a temporary that has already been renamed away.
    finished: bool,
    /// Set once `finish` removes the temporary rather than publish it,
    /// because the backend is read-only or an invalidating purge ended the
    /// store.
    discarded: bool,
    /// Set once a write, synchronisation, or rename has failed: the store is
    /// aborted from then on (FRE-COMPAT.WRITEFAIL-01).
    failed: bool,
}

impl FsMissHandler {
    /// The CPU-only half of opening a store for `key`: hashing the key and
    /// serialising the metadata. Split out so the borrowed `CacheKey`/`CacheMeta` stay on
    /// the caller's side and only owned bytes reach the blocking pool
    /// (FDS-STORAGE.EXECUTOR-01).
    pub(crate) fn prepare(
        cache: &'static FsCache,
        key: &CacheKey,
        meta: &CacheMeta,
    ) -> Result<PreparedStore> {
        let (internal_bytes, header_bytes) = meta.serialize()?;
        Ok(PreparedStore {
            name: cache.object_name(key),
            key_bytes: key_codec::encode(&key.to_compact()),
            internal_bytes,
            header_bytes,
        })
    }

    /// The blocking half of opening a store: create a fresh per-writer
    /// temporary, and write the record's fixed part up front, the header,
    /// key, and metadata region. The body is appended later, one
    /// `write_body` chunk at a time. Runs on the blocking pool.
    pub(crate) fn open(cache: &'static FsCache, prepared: PreparedStore) -> Result<Self> {
        let PreparedStore {
            name,
            key_bytes,
            internal_bytes,
            header_bytes,
        } = prepared;
        let root = cache.root();
        cache.dir_cache.ensure_shard_dir(root, &name)?;
        let canonical_path = cache.canonical_path(&name)?;

        let segments_len = format::segments_len(internal_bytes.len(), header_bytes.len())
            .or_err(ErrorType::InternalError, "sizing the record's metadata")?;

        // `body_len` and `body_crc` stay zero until `finish` patches them.
        let mut header = RecordHeader::new();
        header
            .edit()
            .meta_capacity(cache.config.meta_capacity_for(segments_len))
            .key_and_metadata(&key_bytes, &internal_bytes, &header_bytes)
            .or_err(ErrorType::InternalError, "encoding the record header")?;

        let (mut file, temp_path) = create_temp(cache, &name)?;
        // From here on the temporary exists on disk, but no `FsMissHandler`
        // owns it yet: any failure below must remove it, or it would sit
        // there for this process's whole life — the reclaim scan leaves a
        // live writer's temporaries alone (FRE-COMPAT.ABORTSPACE-01). The
        // write is made in a closure so that any `?` in it, the test hook's
        // included, ends in the cleanup below rather than returning from
        // `open` with the temporary left behind.
        let mut write_fixed_part = || -> Result<()> {
            // One write for the whole fixed part of the record: header, key,
            // both metadata segments, and the headroom that completes the
            // region. The body then starts exactly at `header.body_offset()`,
            // which is what a reader computes.
            let mut prefix = Vec::with_capacity(header.body_offset());
            prefix.extend_from_slice(header.as_bytes());
            prefix.extend_from_slice(&key_bytes);
            prefix.extend_from_slice(&internal_bytes);
            prefix.extend_from_slice(&header_bytes);
            prefix.resize(header.body_offset(), 0);
            test_hook! { cache.hooks.fault(FaultPoint::WriteTemp)?; }
            file.write_all(&prefix)
                .or_err_with(ErrorType::FileWriteError, || {
                    format!("writing {}", temp_path.display())
                })
        };
        if let Err(e) = write_fixed_part() {
            publish::discard_temp(&temp_path);
            return Err(e);
        }
        let writer = cache
            .inflight
            .begin(&name, temp_path.clone(), internal_bytes, header_bytes);
        let write_tag = U64WriteId::from(writer.id);

        Ok(FsMissHandler {
            cache,
            file: Some(Arc::new(file)),
            temp_path,
            canonical_path,
            name,
            header,
            body_crc: 0,
            body_written: 0,
            writer,
            write_tag,
            finished: false,
            discarded: false,
            failed: false,
        })
    }

    /// The temporary's handle.
    fn file(&self) -> &Arc<File> {
        self.file
            .as_ref()
            .expect("the temporary is taken only as the handler is dropped")
    }

    /// Rename the temporary over the canonical name. The caller holds the
    /// key's publication lock and the read-only gate.
    fn rename_into_place(&self) -> Result<()> {
        test_hook! { self.cache.hooks.fault(FaultPoint::Rename)?; }
        publish::rename(&self.temp_path, &self.canonical_path).or_err_with(
            ErrorType::FileWriteError,
            || {
                format!(
                    "renaming {} to {}",
                    self.temp_path.display(),
                    self.canonical_path.display()
                )
            },
        )
    }

    /// The error a store that has already failed reports for any further
    /// call.
    fn already_failed<T>(&self) -> Result<T> {
        Error::e_explain(
            ErrorType::InternalError,
            format!("the store of {} has already failed", self.name),
        )
    }

    /// Patch the header, synchronise, and publish. Returns the record size:
    /// the published file's full length (FRE-COMPAT.EVICTION-01).
    fn finish_inner(&mut self) -> Result<u64> {
        if self.failed {
            return self.already_failed();
        }
        self.header.edit().body(self.body_written, self.body_crc);
        let patch = |h: &mut Self| -> Result<()> {
            h.file()
                .write_all_at(h.header.as_bytes(), 0)
                .or_err_with(ErrorType::FileWriteError, || {
                    format!("writing the header of {}", h.temp_path.display())
                })?;
            test_hook! { h.cache.hooks.fault(FaultPoint::SyncTemp)?; }
            publish::sync_temp(h.file()).or_err_with(ErrorType::FileWriteError, || {
                format!("synchronising {}", h.temp_path.display())
            })
        };
        patch(self).inspect_err(|_| self.failed = true)?;

        let record_len = self.header.record_len();
        // A writable backend publishes inside a durable change, so a
        // `set_read_only` in progress cannot land between the check and the
        // rename (FDS-COMPAT.READONLY-01). The record this store replaces is
        // held open across the rename, so the kernel frees it when
        // `replaced` is dropped, at the end, rather than inside the rename,
        // under the lock.
        let mut ended_by_purge = false;
        let replaced = match self.cache.locks.begin(&self.name) {
            // An invalidating purge of the key ended this store while it was
            // in flight (FRE-COMPLIANCE.PURGE-01): publish nothing. The purge
            // marks the store under this same lock, so a purge either came
            // first and is seen here, or comes after the rename and removes
            // the record.
            Some(_change) if self.writer.purged() => {
                publish::discard_temp(&self.temp_path);
                self.discarded = true;
                ended_by_purge = true;
                None
            }
            Some(_change) => {
                let replaced = publish::hold_open(&self.canonical_path);
                self.rename_into_place()
                    .inspect_err(|_| self.failed = true)?;
                replaced
            }
            // Read-only: publish nothing, but report the store created, so
            // the draining process's request completes normally.
            None => {
                publish::discard_temp(&self.temp_path);
                self.discarded = true;
                None
            }
        };

        // Readers attached to this store deliver the rest and end normally,
        // whether the record was published or discarded
        // (FRE-COMPAT.STREAMEND-01).
        self.writer.set_outcome(WriteOutcome::Completed {
            body_len: self.header.body_len(),
        });
        self.cache.inflight.end(&self.name, self.writer.id);
        self.finished = true;
        // Last, so that freeing the replaced record delays none of the above.
        test_hook! {
            self.cache.hooks.pause(
                crate::hooks::PausePoint::BeforeFreeingOldRecord,
                &self.canonical_path,
            );
        }
        drop(replaced);
        if ended_by_purge {
            // An error, so that `HttpCache` admits nothing to its eviction
            // manager, and lets the requests waiting on its cache lock
            // retry rather than read a record that does not exist.
            return Error::e_explain(
                STORE_PURGED,
                format!(
                    "the store of {} was ended by a purge while in flight",
                    self.name
                ),
            );
        }
        Ok(record_len)
    }
}

/// Create the store's temporary exclusively. On a name collision the
/// counter advances; if the shard directory has been removed behind the
/// backend's back, it is recreated once (FDS-LAYOUT.STARTUP-01).
pub(crate) fn create_temp(cache: &FsCache, name: &str) -> Result<(File, PathBuf)> {
    let mut recreated_dir = false;
    for _ in 0..NAME_ATTEMPTS {
        let path = sharding::temp_path(
            cache.root(),
            name,
            &cache.lease.id(),
            cache.lease.next_counter(),
        )
        .or_err(ErrorType::InternalError, "deriving a temporary's path")?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !recreated_dir => {
                recreated_dir = true;
                let dir = cache.shard_dir(name)?;
                std::fs::create_dir_all(&dir).or_err_with(ErrorType::FileCreateError, || {
                    format!("recreating {}", dir.display())
                })?;
            }
            Err(e) => {
                return Error::e_because(
                    ErrorType::FileCreateError,
                    format!("creating {}", path.display()),
                    e,
                )
            }
        }
    }
    Error::e_explain(
        ErrorType::FileCreateError,
        format!("no free temporary name for {name} in {NAME_ATTEMPTS} attempts"),
    )
}

#[async_trait]
impl HandleMiss for FsMissHandler {
    async fn write_body(&mut self, data: Bytes, _eof: bool) -> Result<()> {
        if self.failed {
            return self.already_failed();
        }
        // Folding the chunk into the running checksum is pure CPU, so it
        // stays here; the write behind it is a syscall and does not
        // (FDS-STORAGE.EXECUTOR-01).
        #[cfg(feature = "body-crc")]
        {
            self.body_crc = crc32c::crc32c_append(self.body_crc, &data);
        }
        // Every chunk is written before this call returns, and only then
        // made readable: the store holds no body bytes beyond the chunk in
        // hand (FRE-COMPAT.UNSIZED-01), and a partial-hit reader sees each
        // one as soon as it is in the file (FDS-STORAGE.PARTIAL-01).
        let file = Arc::clone(self.file());
        let offset = self.header.body_offset() as u64 + self.body_written;
        let len = data.len() as u64;
        #[cfg(test)]
        let cache = self.cache;
        let result = blocking::run(move || {
            test_hook! { cache.hooks.fault(FaultPoint::WriteTemp)?; }
            Ok(file.write_all_at(&data, offset))
        })
        .await
        .and_then(|written| {
            written.or_err_with(ErrorType::FileWriteError, || {
                format!("writing a body chunk to {}", self.temp_path.display())
            })
        });
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.body_written += len;
        self.writer.publish_valid_len(self.body_written);
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        // `finish` patches the header, `fdatasync`s, and renames: three
        // syscalls, one of them a flush that can take milliseconds on a real
        // device. So the whole handler moves to the blocking pool
        // (FDS-STORAGE.EXECUTOR-01), and is dropped there as the closure
        // ends, whether the store published or failed. A failed store's
        // `Drop` aborts it and removes its temporary
        // (FRE-COMPAT.ABORTSPACE-01). A published store's closes its handle
        // on the record, which frees the record if another store has
        // replaced it, or a purge removed it, in the meantime: a close that
        // must not stall an async worker.
        let record_len = blocking::run(move || {
            let mut handler = self;
            handler.finish_inner()
        })
        .await?;
        // This backend always writes a brand-new per-writer temporary, even
        // when replacing an existing object (FRE-COMPLIANCE.REPLACE-01) --
        // never incrementally reopens an old file across handler calls --
        // so every completed write is a fresh `Created`, never `Appended`.
        Ok(MissFinishType::Created(record_len as usize))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.write_tag.as_bytes())
    }

    // IMP-COMPAT.ENTRYID-01: satisfy ADR-COMPAT.ENTRYID-01
    /// No identity beyond the cache key; see `HandleHit::entry_id`, which
    /// this must agree with. The per-writer temporary this handler owns is
    /// write state, not a stored generation, and the trait requires an
    /// identifier to stay valid after `finish`, so the temporary's identity
    /// could not serve as one even if there were a use for it.
    fn entry_id(&self) -> Option<CacheEntryId> {
        None
    }
}

impl Drop for FsMissHandler {
    fn drop(&mut self) {
        test_hook! {
            self.cache
                .hooks
                .pause(crate::hooks::PausePoint::StoreDropping, &self.canonical_path);
        }
        // "When self is dropped without calling [finish], the storage
        // should consider this write failed" (HandleMiss::finish's own
        // doc) -- an orderly abort, an upstream failure, a cancellation, a
        // write failure, or a same-process panic. The temporary goes with
        // it (FRE-COMPAT.ABORTSPACE-01); a real process crash leaves one
        // behind for FDS-STORAGE.RECLAIM-01's restart-time scan instead.
        if !self.finished {
            // One of the two syscalls this backend still makes on the
            // caller's thread (FDS-STORAGE.EXECUTOR-01). `Drop` cannot
            // await, and the temporary's name must be gone when the drop
            // returns (FRE-COMPAT.ABORTSPACE-01). The unlink is cheap: this
            // handler still has the file open, so it removes only the name.
            publish::discard_temp(&self.temp_path);
            self.writer.set_outcome(WriteOutcome::Aborted);
            self.cache.inflight.end(&self.name, self.writer.id);
        }
        // Unless the store published, its temporary has lost its name:
        // removed above, or by `finish` once the backend was read-only.
        // Closing this handler's handle on it frees its storage, so
        // `blocking::close_removed_file` closes it off the async worker.
        // Nothing is left behind if the process exits first: the kernel
        // closes the file then. A reader still streaming the store keeps its
        // own handle, and the storage, until it finishes.
        if !self.finished || self.discarded {
            // A write still running on the blocking pool, its request having
            // stopped waiting for it, holds a reference too, and closes the
            // temporary there, last: `try_unwrap` leaves the close to it.
            if let Some(file) = self.file.take().and_then(|f| Arc::try_unwrap(f).ok()) {
                blocking::close_removed_file(
                    file,
                    #[cfg(test)]
                    self.cache,
                    #[cfg(test)]
                    self.temp_path.clone(),
                );
            }
        }
    }
}
