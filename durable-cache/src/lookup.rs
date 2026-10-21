// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `Storage::lookup` / `HandleHit` — the read side (SDS-LAYOUT.OBJECTFILE-01,
//! FDS-STORAGE.PARTIAL-01).
//!
//! A store in flight takes precedence over the published record: a lookup
//! is answered with a partial hit of the most recently opened store of the
//! key that no invalidating purge has ended. A lookup carrying
//! the `pingora-cache` streaming write tag is answered with the store the tag
//! names, even an ended one, or with nothing. A damaged, foreign, or
//! wrong-key record is a miss, never a corrupt read, and the file is left
//! where it is: a later store replaces it and the restart-time scan
//! reclaims it. The exception is a damaged temporary that a tagged lookup
//! names, which is an error (see `lookup`).

// IMP-STORAGE.LOOKUP-01: satisfy SDS-LAYOUT.OBJECTFILE-01 -- the read
// protocol (validation order, retry, optional body check, seek); miss.rs's
// IMP-STORAGE.PUBLISH-01 covers the writer half.
// IMP-STORAGE.PARTIAL-01: satisfy FDS-STORAGE.PARTIAL-01
use crate::blocking;
use crate::inflight::{InFlightWriter, WriteOutcome};
use crate::key_codec;
use crate::record::{self, ReadError, RecordError, RecordKind};
use crate::FsCache;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_cache::eviction::CacheEntryId;
use pingora_cache::storage::streaming_write::U64WriteId;
use pingora_cache::storage::{HandleHit, Storage};
use pingora_cache::trace::SpanHandle;
use pingora_cache::{CacheKey, CacheMeta};
use pingora_error::{Error, ErrorType, OrErr, Result};
use std::any::Any;
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

const CHUNK_SIZE: u64 = 64 * 1024;

/// Where a hit's body comes from, and how far it has been read.
///
/// Positions here count from the body's first byte, not from the start of
/// the file: body position `p` is at file offset `body_offset + p`.
enum ReadSource {
    /// A published record, whose body no longer changes. It can be sought.
    Complete {
        /// File offset of the body's first byte.
        body_offset: u64,
        /// The body's length.
        body_len: u64,
        /// The next body position to read.
        pos: u64,
        /// The body position to stop at, excluded: `body_len`, unless a seek
        /// set it.
        end: u64,
    },
    /// The temporary of a store still in flight: a partial hit, whose body
    /// grows while it is read (FDS-STORAGE.PARTIAL-01).
    Partial {
        /// The store, whose valid length says how much body can be read.
        writer: Arc<InFlightWriter>,
        /// File offset of the body's first byte.
        body_offset: u64,
        /// Body bytes delivered so far, which is also the next body position
        /// to read.
        read_so_far: u64,
    },
}

pub struct FsHitHandler {
    /// Shared so a blocking read can own a handle without reopening the
    /// file (FDS-STORAGE.EXECUTOR-01). `read_exact_at` takes `&self`, so a
    /// shared descriptor is all a reader ever needs. Taken only as the
    /// handler is dropped, to close it where its close cannot stall an async
    /// worker (see `Drop`).
    file: Option<Arc<File>>,
    /// Where the body comes from, and how far it has been read.
    source: ReadSource,
    /// The file this hit reads, which names it in the context of an error
    /// this handler returns.
    path: PathBuf,
    /// The read-time body check, while one runs: switched on in the
    /// configuration, for a published record that carries a checksum, until
    /// the read seeks (ADR-PERSISTENCE.DURABILITY-01).
    #[cfg(feature = "body-crc-verify")]
    check: Option<BodyCheck>,
    /// The backend, for the test hook in `Drop`.
    #[cfg(test)]
    cache: &'static FsCache,
}

/// A body check in progress (ADR-PERSISTENCE.DURABILITY-01).
#[cfg(feature = "body-crc-verify")]
struct BodyCheck {
    /// CRC-32C over the bytes delivered so far.
    running: u32,
    /// The record's `body_crc`.
    stored: u32,
}

#[cfg(feature = "body-crc-verify")]
impl BodyCheck {
    /// A check of a record whose header stores `stored`, if `cache` has
    /// checking switched on. A stored zero is not checked: it is what a
    /// build without the `body-crc` feature records, and the checksum of an
    /// empty body.
    fn start(cache: &FsCache, stored: u32) -> Option<BodyCheck> {
        (cache.config.body_crc_verification() && stored != 0)
            .then_some(BodyCheck { running: 0, stored })
    }

    /// Log a mismatch once the whole body has been delivered. The response
    /// still ends normally: its bytes are already on their way to the
    /// client, and an error now could only cut the connection.
    fn finish(self, path: &Path) {
        if self.running != self.stored {
            log::error!(
                "the body of {} does not match its stored checksum \
                 (stored {:#010x}, computed {:#010x}); it was served as read",
                path.display(),
                self.stored,
                self.running
            );
        }
    }
}

/// Answer a lookup. A `streaming_write_tag` names a store in flight:
/// `pingora-cache` passes one only from the request that has just opened
/// that store, to read back the body it is writing
/// (`HttpCache::set_miss_handler`).
pub(crate) async fn lookup(
    cache: &'static FsCache,
    key: &CacheKey,
    streaming_write_tag: Option<&[u8]>,
) -> Result<Option<(CacheMeta, FsHitHandler)>> {
    let name = cache.object_name(key);
    let key_bytes = key_codec::encode(&key.to_compact());

    // A tag this backend did not issue names no store of its own, and is
    // kept distinct from carrying no tag at all: falling back to the
    // most-recently-opened store would answer a request that named one write
    // with a different writer's body, which is exactly what
    // `Storage::lookup_streaming_write`'s own doc rules out. Unreachable
    // through the supported `pingora-cache`, whose only caller passes back
    // the eight-byte tag this crate's own miss handler just returned; stated
    // so that another caller cannot make it a wrong answer.
    let tag = match streaming_write_tag {
        Some(bytes) => match U64WriteId::try_from(bytes) {
            Ok(tag) => Some(u64::from(tag)),
            Err(_) => {
                log::warn!(
                    "lookup carried a {}-byte streaming write tag, which \
                     this backend never issues; reporting a miss rather than another writer's body",
                    bytes.len()
                );
                return Ok(None);
            }
        },
        None => None,
    };
    let in_flight = match tag {
        Some(tag) => cache.inflight.by_tag(&name, tag),
        None => cache.inflight.most_recent(&name),
    };
    if let Some(writer) = in_flight {
        let key_bytes = key_bytes.clone();
        let opening = Arc::clone(&writer);
        match blocking::run(move || open_partial_hit(cache, opening, &key_bytes)).await? {
            InFlightTemp::Hit(meta, hit) => return Ok(Some((meta, hit))),
            // The store wrote this fixed part in full before registering,
            // and a read torn by its header update is retried, so this is
            // damage or a bug. With a tag it is an error rather than a
            // miss: `pingora-cache` panics when this lookup finds nothing,
            // and an error fails only this request.
            InFlightTemp::Unreadable(e) if tag.is_some() => {
                return Error::e_because(
                    ErrorType::FileReadError,
                    format!("reading {}", writer.temp_path.display()),
                    e,
                )
            }
            // The store published or removed its temporary between finding
            // it and opening it. An untagged lookup also passes over a
            // damaged temporary, to the published record.
            InFlightTemp::Gone | InFlightTemp::Unreadable(_) => {}
        }
    }
    // A tagged lookup is answered with the store its tag names or with
    // nothing, as `Storage::lookup_streaming_write`'s doc asks. Never
    // from the published record, even once the named store has published:
    // the record there may be a newer store's.
    if tag.is_some() {
        return Ok(None);
    }

    let canonical = cache.canonical_path(&name)?;
    // Opening, validating, and decoding the record is all blocking syscall
    // work, so it goes to the blocking pool in one handoff rather than
    // stalling the worker this lookup is running on
    // (FDS-STORAGE.EXECUTOR-01).
    blocking::run(move || open_published(cache, &name, canonical, key_bytes)).await
}

/// Open and validate the published record at `canonical`, the record of the
/// key whose files are named `name`, returning the hit it answers with, or
/// `None` for a miss. Blocking throughout; called only from the blocking
/// pool.
fn open_published(
    cache: &'static FsCache,
    name: &str,
    canonical: PathBuf,
    key_bytes: Vec<u8>,
) -> Result<Option<(CacheMeta, FsHitHandler)>> {
    let file = match File::open(&canonical) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Error::e_because(
                ErrorType::FileOpenError,
                format!("opening {}", canonical.display()),
                e,
            )
        }
    };
    let mut read = record::read_record_blocking(&file, RecordKind::Published, Some(&key_bytes));
    // Checksums that kept failing may be an in-place metadata update caught
    // between its two writes, by an updater the scheduler stopped there.
    // Every in-place update holds the key's publication lock from its first
    // write to its last, so one more read under that lock finds the record
    // whole, before or after the update, and a mismatch then is damage
    // (SDS-LAYOUT.OBJECTFILE-01). A lookup changes nothing, so it takes the
    // lock alone, without entering the read-only gate, and it holds no
    // change meanwhile, as `ChangeLocks::lock_key` requires.
    if matches!(&read, Err(ReadError::Record(e)) if e.is_retryable()) {
        let _key = cache.locks.lock_key(name);
        read = record::read_record_once(&file, RecordKind::Published, Some(&key_bytes));
    }
    let parsed = match read {
        Ok(parsed) => parsed,
        Err(ReadError::Record(e)) => {
            // A miss, and the file is left in place: a later store of the
            // same key replaces it and FDS-STORAGE.RECLAIM-01's scan
            // reclaims it, so a lookup never unlinks a path whose content
            // may have changed since it opened it.
            log::debug!("{} reported as a miss: {e}", canonical.display());
            return Ok(None);
        }
        Err(ReadError::Io(e)) => {
            return Error::e_because(
                ErrorType::FileReadError,
                format!("reading {}", canonical.display()),
                e,
            )
        }
    };
    let meta = cache.decode_meta(&parsed.internal_bytes, &parsed.header_bytes)?;
    Ok(Some((
        meta,
        FsHitHandler {
            file: Some(Arc::new(file)),
            source: ReadSource::Complete {
                body_offset: parsed.header.body_offset() as u64,
                body_len: parsed.header.body_len(),
                pos: 0,
                end: parsed.header.body_len(),
            },
            path: canonical,
            #[cfg(feature = "body-crc-verify")]
            check: BodyCheck::start(cache, parsed.header.body_crc()),
            #[cfg(test)]
            cache,
        },
    )))
}

/// What opening an in-flight store's temporary found.
enum InFlightTemp {
    /// A partial hit on the store.
    Hit(CacheMeta, FsHitHandler),
    /// The temporary is gone: the store published it or removed it.
    Gone,
    /// The temporary is there, but its fixed part does not read as this
    /// key's record.
    Unreadable(RecordError),
}

/// Open a partial hit on `writer`'s temporary.
fn open_partial_hit(
    cache: &'static FsCache,
    writer: Arc<InFlightWriter>,
    key_bytes: &[u8],
) -> Result<InFlightTemp> {
    let file = match File::open(&writer.temp_path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(InFlightTemp::Gone),
        Err(e) => {
            return Error::e_because(
                ErrorType::FileOpenError,
                format!("opening {}", writer.temp_path.display()),
                e,
            )
        }
    };
    // Not a published record: `body_len` is patched in only at `finish`, so
    // the in-flight registry's valid length, not this header, says how much
    // body is safe to read.
    let parsed = match record::read_record_blocking(&file, RecordKind::InFlight, Some(key_bytes)) {
        Ok(parsed) => parsed,
        Err(ReadError::Record(e)) => {
            log::debug!("in-flight record unreadable: {e}");
            return Ok(InFlightTemp::Unreadable(e));
        }
        Err(ReadError::Io(e)) => {
            return Error::e_because(
                ErrorType::FileReadError,
                format!("reading {}", writer.temp_path.display()),
                e,
            )
        }
    };
    let meta = cache.decode_meta(&writer.internal_meta, &writer.header_meta)?;
    let path = writer.temp_path.clone();
    Ok(InFlightTemp::Hit(
        meta,
        FsHitHandler {
            file: Some(Arc::new(file)),
            source: ReadSource::Partial {
                writer,
                body_offset: parsed.header.body_offset() as u64,
                read_so_far: 0,
            },
            path,
            #[cfg(feature = "body-crc-verify")]
            check: None,
            #[cfg(test)]
            cache,
        },
    ))
}

/// One positional read, on the blocking pool: a body read is a `pread` that
/// may reach the device, which must not run on the worker driving other
/// connections (FDS-STORAGE.EXECUTOR-01).
async fn read_at(file: Arc<File>, offset: u64, len: usize, path: &Path) -> Result<Vec<u8>> {
    blocking::run(move || {
        let mut buf = vec![0u8; len];
        Ok(file.read_exact_at(&mut buf, offset).map(|()| buf))
    })
    .await?
    .or_err_with(ErrorType::FileReadError, || {
        format!("reading {}", path.display())
    })
}

impl FsHitHandler {
    /// The file this hit reads.
    fn file(&self) -> &Arc<File> {
        self.file
            .as_ref()
            .expect("the file is taken only as the handler is dropped")
    }

    async fn read_body_inner(&mut self) -> Result<Option<Bytes>> {
        let file = Arc::clone(self.file());
        match &mut self.source {
            ReadSource::Complete {
                body_offset,
                pos,
                end,
                ..
            } => {
                if *pos >= *end {
                    #[cfg(feature = "body-crc-verify")]
                    if let Some(check) = self.check.take() {
                        check.finish(&self.path);
                    }
                    return Ok(None);
                }
                let len = (*end - *pos).min(CHUNK_SIZE) as usize;
                let offset = *body_offset + *pos;
                let buf = read_at(Arc::clone(&file), offset, len, &self.path).await?;
                #[cfg(feature = "body-crc-verify")]
                if let Some(check) = &mut self.check {
                    check.running = crc32c::crc32c_append(check.running, &buf);
                }
                *pos += len as u64;
                Ok(Some(Bytes::from(buf)))
            }
            ReadSource::Partial {
                writer,
                body_offset,
                read_so_far,
            } => loop {
                // Register for the next notification *before* checking
                // state, so an append/finish landing in between is never
                // missed (tokio::sync::Notify's documented pattern:
                // notified() snapshots the notify_waiters() count at
                // creation, not at first poll).
                let notified = writer.notify.notified();

                let valid = writer.valid_len.load(Ordering::Acquire);
                if *read_so_far < valid {
                    let len = (valid - *read_so_far).min(CHUNK_SIZE) as usize;
                    let offset = *body_offset + *read_so_far;
                    let buf = read_at(Arc::clone(&file), offset, len, &self.path).await?;
                    *read_so_far += len as u64;
                    return Ok(Some(Bytes::from(buf)));
                }

                match writer.outcome() {
                    Some(WriteOutcome::Completed { body_len }) => {
                        if *read_so_far >= body_len {
                            // The store completed, published or discarded
                            // alike: end normally
                            // (FRE-COMPAT.STREAMEND-01). This descriptor
                            // stays valid either way.
                            return Ok(None);
                        }
                        // valid_len has not caught up with body_len in this
                        // load yet (a benign race right at completion);
                        // loop rather than trust a stale value.
                    }
                    // FRE-COMPLIANCE.PARTIALABORT-01: never a clean end.
                    Some(WriteOutcome::Aborted) => {
                        return Error::e_explain(
                            ErrorType::InternalError,
                            format!(
                                "the store writing {} aborted before this reader caught up",
                                self.path.display()
                            ),
                        )
                    }
                    None => notified.await,
                }
            },
        }
    }
}

#[async_trait]
impl HandleHit for FsHitHandler {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        self.read_body_inner().await
    }

    /// Nothing to do here: the handler is dropped as this returns, and its
    /// `Drop` closes the file.
    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _trace: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    /// A published record supports seeking; a partial hit does not
    /// (FDS-STORAGE.PARTIAL-01).
    fn can_seek(&self) -> bool {
        matches!(self.source, ReadSource::Complete { .. })
    }

    /// Read `start..end` of the body next. As in `MemCache`, the in-memory
    /// backend of `pingora-cache`, `start` must fall inside the
    /// body, so an empty body cannot be sought at all, while an `end` past
    /// the last byte reads to the end.
    fn seek(&mut self, start: usize, end: Option<usize>) -> Result<()> {
        match &mut self.source {
            ReadSource::Complete {
                body_len,
                pos,
                end: read_end,
                ..
            } => {
                let start = start as u64;
                if start >= *body_len {
                    return Error::e_explain(
                        ErrorType::InternalError,
                        format!(
                            "seek start {start} is outside the {body_len}-byte body of {}",
                            self.path.display()
                        ),
                    );
                }
                *pos = start;
                // An end past the last byte is allowed: deliver what is
                // there.
                *read_end = end.map_or(*body_len, |e| (e as u64).min(*body_len));
                // A sought read is not checked: the stored checksum covers
                // a whole body only.
                #[cfg(feature = "body-crc-verify")]
                {
                    self.check = None;
                }
                Ok(())
            }
            ReadSource::Partial { .. } => Error::e_explain(
                ErrorType::InternalError,
                format!(
                    "cannot seek in {}: a store still in flight",
                    self.path.display()
                ),
            ),
        }
    }

    /// A partial hit counts no eviction access, as in `MemCache`. It reads a
    /// store still in flight, whose size is not known yet and which may
    /// still abort; Pingora admits the object, with its size, when the store
    /// finishes. Counting an access now would make an eviction manager that
    /// does not track the object yet start tracking it, at the zero weight
    /// `get_eviction_weight` reports for a partial hit.
    fn should_count_access(&self) -> bool {
        matches!(self.source, ReadSource::Complete { .. })
    }

    /// The record's size: its file's length, which is also what `finish`
    /// reported for it and what Eviction Recovery admits it with
    /// (FRE-COMPAT.EVICTION-01). Pingora passes it to the eviction manager
    /// with each counted access, so for complete hits only (see
    /// `should_count_access`). A manager that does not track the object yet
    /// starts tracking it at this size, rather than at the default of zero.
    /// That happens, for example, when a hit comes after a restart, before
    /// Eviction Recovery has admitted the record (FDS-EVICTION.STARTUP-01).
    fn get_eviction_weight(&self) -> usize {
        match &self.source {
            ReadSource::Complete {
                body_offset,
                body_len,
                ..
            } => (*body_offset + *body_len) as usize,
            // Never asked for: a partial hit counts no access. Its store is
            // admitted, with its size, when it finishes.
            ReadSource::Partial { .. } => 0,
        }
    }

    // IMP-COMPAT.ENTRYID-01: satisfy ADR-COMPAT.ENTRYID-01
    /// No identity beyond the cache key. One key names one canonical path
    /// and a store replaces whatever is there by renaming over it, so there
    /// are no stored generations to tell apart, and eviction stays keyed on
    /// the `CompactCacheKey` alone. Stated explicitly rather than inherited
    /// from the trait's default so that this and `HandleMiss::entry_id`
    /// visibly agree, as the trait requires them to.
    fn entry_id(&self) -> Option<CacheEntryId> {
        None
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

impl Drop for FsHitHandler {
    /// Close the file this hit read, on the blocking pool when the close
    /// would free it (FDS-STORAGE.EXECUTOR-01).
    ///
    /// A file's storage is freed when its last name and its last descriptor
    /// are both gone, and freeing can take a long time for a large file. A
    /// hit can hold that last descriptor: when a store replaced its record,
    /// or a purge or eviction removed it, while the hit was reading it, or
    /// when a partial hit's temporary was removed because its store aborted
    /// or was discarded. The handler is dropped where Pingora releases it,
    /// on an async worker: at the end of `finish`, or wherever a request
    /// that stopped early lets it go. Closed there, the freeing would stall
    /// every connection that worker drives.
    ///
    /// Handing every hit's close to the pool would cost the worker more than
    /// it saves, as most files keep their name, so one `fstat` decides. It
    /// reads the open file's inode, which is in memory: the cache directory
    /// is on a local file system, so it reaches neither a device nor a
    /// network. A file that still has a name frees nothing as it closes, and
    /// is closed here. One that has none, or whose link count cannot be
    /// read, is given to `blocking::close_removed_file`, which closes it on
    /// the pool, without waiting. Another hit may still hold the same file,
    /// which makes that handoff unnecessary, but harmless. A file that loses
    /// its name between the check and the close is freed here, as before;
    /// that window is a few instructions long.
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        // A read still running on the blocking pool, its request having
        // stopped waiting for it, holds a reference too. It then closes the
        // file there, last, and nothing is left to do here.
        let Ok(file) = Arc::try_unwrap(file) else {
            return;
        };
        if file.metadata().is_ok_and(|meta| meta.nlink() > 0) {
            drop(file);
            return;
        }
        blocking::close_removed_file(
            file,
            #[cfg(test)]
            self.cache,
            #[cfg(test)]
            self.path.clone(),
        );
    }
}
