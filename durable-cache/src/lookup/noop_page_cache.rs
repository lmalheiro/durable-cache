// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The stand-in for `page_cache` elsewhere than on Linux, which alone has
//! calls that fail rather than wait for the device: nothing is tried on the
//! worker, and every lookup and body read goes to the blocking pool whole.
//! `lookup` declares it as `page_cache`, so that its calls read the same on
//! every platform.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use pingora_cache::CacheMeta;
use pingora_error::Result;

use super::{FsHitHandler, InFlightTemp, OnWorker};
use crate::inflight::InFlightWriter;
use crate::FsCache;

/// The pool opens the published record.
pub(super) fn open_published(
    _cache: &'static FsCache,
    _canonical: &Path,
    _key_bytes: &[u8],
) -> OnWorker<Result<Option<(CacheMeta, FsHitHandler)>>> {
    OnWorker::ToPool(None)
}

/// The pool opens the store's temporary.
pub(super) fn open_partial_hit(
    _cache: &'static FsCache,
    _writer: &Arc<InFlightWriter>,
    _key_bytes: &[u8],
) -> OnWorker<Result<InFlightTemp>> {
    OnWorker::ToPool(None)
}

/// The pool reads the whole body: nothing is read here.
pub(super) fn fill(
    _cache: &FsCache,
    _file: &File,
    _buf: &mut Vec<u8>,
    _end: usize,
    _base: u64,
    _path: &Path,
) -> Result<bool> {
    Ok(false)
}
