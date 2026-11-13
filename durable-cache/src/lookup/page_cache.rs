// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Page-cache reads (`FsCacheConfig::with_page_cache_reads`), Linux only: a
//! lookup, and each read of a hit's body, first tried on the async worker
//! with the calls of `crate::nowait`, which fail rather than wait for the
//! device. What would wait, the caller in `crate::lookup` does on the
//! blocking pool, whole, as FDS-STORAGE.EXECUTOR-01 has it.
//!
//! Each function here does nothing when the configuration leaves these
//! reads off. A call the system does not support after all, although
//! `FsCache::new` checked that it did, is an error naming the file: a
//! page-cache read never turns into a pool read unnoticed.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use pingora_cache::CacheMeta;
use pingora_error::{Error, ErrorType, Result};

use super::{partial_hit, published_hit, FsHitHandler, InFlightTemp};
use crate::inflight::InFlightWriter;
use crate::record::{self, ReadError};
use crate::{blocking, nowait, FsCache};

// IMP-STORAGE.PAGECACHE-01: satisfy FDS-STORAGE.PAGECACHE-01

/// `$call`, a call on the worker, which the test hooks name `$point`, on the
/// file at `$path`: in the crate's test build, the error a test armed for it
/// instead, if it armed one, so that the error takes the same path a real
/// one would (TC-STORAGE.PAGECACHE-01).
macro_rules! on_worker {
    ($cache:expr, $point:ident, $path:expr, $call:expr) => {{
        #[cfg(test)]
        let armed = $cache
            .hooks
            .worker_fault(crate::hooks::WorkerCall::$point, $path);
        #[cfg(not(test))]
        let armed: Option<std::io::Error> = None;
        match armed {
            Some(e) => Err(e.into()),
            None => $call,
        }
    }};
}

/// What an operation tried on the worker came to.
pub(super) enum OnWorker<T> {
    /// Its answer.
    Done(T),
    /// The pool takes over, with the file if the worker opened it: so that
    /// the pool does not open it again, and the worker does not close it. A
    /// close can free a file that a store, a purge or an abort has just
    /// removed, if it was its last descriptor (see `FsHitHandler`'s `Drop`).
    ToPool(Option<File>),
}

/// [`open_published`](super::open_published) on the worker, from the page
/// cache only, when the configuration asks. It answers a hit, a miss on a
/// record that does not exist, and a call the system does not support after
/// all. Anything else goes to the pool, which does it all again: a read that
/// would wait for the device, a checksum mismatch that `open_published`
/// retries under the key's lock, a record it reports as a miss, an error.
pub(super) fn open_published(
    cache: &'static FsCache,
    canonical: &Path,
    key_bytes: &[u8],
) -> OnWorker<Result<Option<(CacheMeta, FsHitHandler)>>> {
    if !cache.config.page_cache_reads() {
        return OnWorker::ToPool(None);
    }
    let file = match on_worker!(
        cache,
        Open,
        canonical,
        nowait::open(canonical, cache.config.noatime())
    ) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OnWorker::Done(Ok(None)),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            return OnWorker::Done(unsupported(canonical, e))
        }
        Err(_) => return OnWorker::ToPool(None),
    };
    let parsed = match on_worker!(
        cache,
        RecordRead,
        canonical,
        record::read_published_nowait(&file, key_bytes)
    ) {
        Ok(parsed) => parsed,
        Err(ReadError::Io(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            close_on_pool(file, cache, canonical);
            return OnWorker::Done(unsupported(canonical, e));
        }
        Err(_) => return OnWorker::ToPool(Some(file)),
    };
    let Ok(meta) = cache.decode_meta(&parsed.internal_bytes, &parsed.header_bytes) else {
        return OnWorker::ToPool(Some(file));
    };
    let hit = published_hit(cache, file, parsed, canonical.to_path_buf());
    OnWorker::Done(Ok(Some((meta, hit))))
}

/// [`open_partial_hit`](super::open_partial_hit) on the worker, from the
/// page cache only, when the configuration asks. It answers a partial hit,
/// a temporary that is gone, and a call the system does not support after
/// all. Anything else goes to the pool, which does it all again.
pub(super) fn open_partial_hit(
    cache: &'static FsCache,
    writer: &Arc<InFlightWriter>,
    key_bytes: &[u8],
) -> OnWorker<Result<InFlightTemp>> {
    if !cache.config.page_cache_reads() {
        return OnWorker::ToPool(None);
    }
    let path = &writer.temp_path;
    let file = match on_worker!(
        cache,
        Open,
        path,
        nowait::open(path, cache.config.noatime())
    ) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return OnWorker::Done(Ok(InFlightTemp::Gone))
        }
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            return OnWorker::Done(unsupported(path, e))
        }
        Err(_) => return OnWorker::ToPool(None),
    };
    let parsed = match on_worker!(
        cache,
        RecordRead,
        path,
        record::read_in_flight_nowait(&file, key_bytes)
    ) {
        Ok(parsed) => parsed,
        Err(ReadError::Io(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            close_on_pool(file, cache, path);
            return OnWorker::Done(unsupported(path, e));
        }
        Err(_) => return OnWorker::ToPool(Some(file)),
    };
    let Ok(meta) = cache.decode_meta(&writer.internal_meta, &writer.header_meta) else {
        return OnWorker::ToPool(Some(file));
    };
    OnWorker::Done(Ok(partial_hit(
        cache,
        Arc::clone(writer),
        file,
        &parsed,
        meta,
    )))
}

/// The read of [`fill`](super::fill) on the worker, from the page cache
/// only, when the configuration asks: `true` once `buf` holds `end` bytes,
/// `false` to carry on on the pool from what it holds, which also reports
/// any error with its context. A call the system does not support after all
/// is an error.
pub(super) fn fill(
    cache: &FsCache,
    file: &File,
    buf: &mut Vec<u8>,
    end: usize,
    base: u64,
    path: &Path,
) -> Result<bool> {
    if !cache.config.page_cache_reads() {
        return Ok(false);
    }
    match on_worker!(
        cache,
        BodyRead,
        path,
        nowait::fill(file, buf, end, base, nowait::Wait::Never)
    ) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => unsupported(path, e),
        Err(_) => Ok(false),
    }
}

/// The error of a page-cache read of `path` that the system does not
/// support after all, although `FsCache::new` checked that it did: a file
/// in the cache lies on another file system than the root, or the system
/// changed since.
fn unsupported<T>(path: &Path, e: std::io::Error) -> Result<T> {
    Error::e_because(
        ErrorType::FileReadError,
        format!(
            "reading {} from the page cache, as FsCacheConfig::with_page_cache_reads asks",
            path.display()
        ),
        e,
    )
}

/// Close `file`, from `path`, on the blocking pool: the worker never closes
/// a record it gives up on, as the close may free a record that a store or
/// a purge has just removed (see `FsHitHandler`'s `Drop`).
fn close_on_pool(file: File, cache: &'static FsCache, path: &Path) {
    #[cfg(not(test))]
    let _ = (cache, path);
    blocking::close_removed_file(
        file,
        #[cfg(test)]
        cache,
        #[cfg(test)]
        path.to_path_buf(),
    );
}
