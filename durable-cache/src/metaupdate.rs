// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `Storage::update_meta` (FDS-STORAGE.METAUPDATE-01): a metadata update
//! writes the new metadata into the record's metadata region, in place,
//! when it fits the region's headroom. When it does not, the update
//! withdraws the record instead: every lookup then misses, so the next
//! request fetches the object again, and its store, sized for the new
//! metadata, replaces the file. Either way a header write, made last,
//! commits the change, and the body is never read or moved.
//!
//! The update is visible to lookups when `update_meta` returns, which is
//! when `HttpCache` releases the key's write lock.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::SystemTime;

use pingora_cache::{CacheKey, CacheMeta};
use pingora_error::{Error, ErrorType, OrErr, Result};
use zerocopy::IntoBytes;

use crate::error::{refuse_read_only, Op, KEY_NOT_FOUND};
#[cfg(test)]
use crate::hooks::{FaultPoint, PausePoint};
use crate::record::{self, ParsedRecord, ReadError, RecordKind};
use crate::{format, key_codec, metrics, FsCache};

// IMP-STORAGE.METAUPDATE-01: satisfy FDS-STORAGE.METAUPDATE-01
// IMP-OBSERVABILITY.HEADROOM-01: satisfy FDS-OBSERVABILITY.HEADROOM-01

/// What [`prepare`] hands to [`update_meta`]: owned, `Send` data, so the
/// update's file work can run on the blocking pool
/// (FDS-STORAGE.EXECUTOR-01).
pub(crate) struct PreparedUpdate {
    name: String,
    key_bytes: Vec<u8>,
    /// When the response the update freshens was created, which names it:
    /// `pingora-cache` gives the new metadata the creation time of the
    /// response its `304` validated (`HttpCache::revalidate_cache_meta`).
    created: SystemTime,
    new_internal: Vec<u8>,
    new_header: Vec<u8>,
}

/// The CPU-only half: hash the key and serialise the new metadata.
pub(crate) fn prepare(
    cache: &'static FsCache,
    key: &CacheKey,
    meta: &CacheMeta,
) -> Result<PreparedUpdate> {
    let (new_internal, new_header) = meta.serialize()?;
    Ok(PreparedUpdate {
        name: cache.object_name(key),
        key_bytes: key_codec::encode(&key.to_compact()),
        created: meta.created(),
        new_internal,
        new_header,
    })
}

/// Synchronous throughout: the read-only gate and publication lock are held
/// across the durable part, and neither may be held across an `.await`.
/// Runs on the blocking pool. Returns `true` once the update is visible to
/// lookups, and `false` when it is not applied: the key holds another
/// response than the one the update freshens, or the new metadata outgrew
/// the record, which is then withdrawn.
pub(crate) fn update_meta(cache: &'static FsCache, prepared: PreparedUpdate) -> Result<bool> {
    let PreparedUpdate {
        name,
        key_bytes,
        created,
        new_internal,
        new_header,
    } = prepared;
    if cache.is_read_only() {
        // An update this process will not make durable fails rather than
        // being reported as applied (FRE-COMPAT.HOTUPGRADE-01).
        return refuse_read_only(Op::UpdateMeta);
    }
    let canonical = cache.canonical_path(&name)?;

    // Under the key's publication lock, this opens and reads the record, then
    // writes the new metadata into it in place, or, when the new metadata no
    // longer fits, withdraws the record.
    //
    // The open needs the lock because every change that replaces or removes
    // the key's record (a store's rename, a purge's or the reclaim's unlink)
    // holds this lock while it does. Opened before the lock, the record could
    // lose its name in between, and this update would change a file that no
    // lookup reads any more, yet report success. Opened under the lock, it is
    // the record the key holds now.
    //
    // The read needs the lock because of how an update or expiry writes a
    // record's metadata: first the metadata, then the header, which holds its
    // lengths and checksum. A read between the two writes finds a checksum
    // that does not match, just as in a damaged record. Under the lock, no
    // such write is half done, so a mismatch means real damage. Without the
    // lock, a healthy record caught between the two writes would be taken
    // for a damaged one, and this update would report that the key holds no
    // object.
    //
    // The write needs the lock too: the header it writes is built from the
    // one just read, and two updates of this key must not interleave their
    // writes (see `write_in_place`).
    //
    // A lookup does not take the lock to read. It re-reads instead, and takes
    // the lock only for a last read when the checksums keep failing
    // (SDS-LAYOUT.OBJECTFILE-01).
    test_hook! { cache.hooks.pause(PausePoint::UpdateBeforeLock, &canonical); }
    let Some(_change) = cache.locks.begin(&name) else {
        return refuse_read_only(Op::UpdateMeta);
    };

    let file = match OpenOptions::new().read(true).write(true).open(&canonical) {
        Ok(file) => file,
        // Nothing is stored under this key: it was never stored, or it
        // has been purged or evicted. FRE-COMPLIANCE.METAABSENT-01 asks
        // for two things here. First, an error, so that the caller knows
        // nothing was applied; `KEY_NOT_FOUND` also says why. Second, the
        // key must still hold no object afterwards. An update brings
        // metadata but no body, and a record made from it alone would let
        // a later lookup serve a response without its body. The file is
        // opened without `create`, so this open cannot make one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Error::e_explain(
                KEY_NOT_FOUND,
                format!(
                    "{}: no object stored at {}",
                    Op::UpdateMeta,
                    canonical.display()
                ),
            )
        }
        Err(e) => {
            return Error::e_because(
                ErrorType::FileOpenError,
                format!("opening {}", canonical.display()),
                e,
            )
        }
    };

    let parsed = match record::read_record_blocking(&file, RecordKind::Published, Some(&key_bytes))
    {
        Ok(parsed) => parsed,
        Err(ReadError::Record(e)) => {
            // Nothing readable is stored under this key: a lookup would
            // report a miss, so an update reports the absent-key error
            // rather than writing into a damaged or foreign record.
            log::debug!(
                "{} found {} unreadable: {e}",
                Op::UpdateMeta,
                canonical.display()
            );
            return Error::e_because(
                KEY_NOT_FOUND,
                format!("{}: {} is unreadable", Op::UpdateMeta, canonical.display()),
                e,
            );
        }
        Err(ReadError::Io(e)) => {
            return Error::e_because(
                ErrorType::FileReadError,
                format!("reading {}", canonical.display()),
                e,
            )
        }
    };

    // A `304` updates only the response it validated (RFC 9111 §4.3.4),
    // which the new metadata names by its creation time. A stored
    // response created at another time has replaced that one since the
    // lookup that began the revalidation, so the update is not applied,
    // and the newer response stands whole (FDS-STORAGE.METAUPDATE-01).
    let stored = cache.decode_meta(&parsed.internal_bytes, &parsed.header_bytes)?;
    if stored.created() != created {
        log::debug!(
            "{} for key {name}: the key holds a response stored since the one \
             revalidated, so the update is not applied",
            Op::UpdateMeta
        );
        return Ok(false);
    }

    if let Some(shortfall) = shortfall(&parsed, &new_internal, &new_header)? {
        withdraw(
            &file,
            &parsed,
            #[cfg(test)]
            cache,
        )?;
        report_outgrown(&canonical, shortfall);
        return Ok(false);
    }
    write_in_place(
        &file,
        &parsed,
        &new_internal,
        &new_header,
        #[cfg(test)]
        cache,
        #[cfg(test)]
        &canonical,
    )?;
    Ok(true)
}

/// By how many bytes `new_internal` and `new_header` outgrow the record's
/// metadata region, or `None` when they fit it and can be written in place.
fn shortfall(parsed: &ParsedRecord, new_internal: &[u8], new_header: &[u8]) -> Result<Option<u32>> {
    let segments = format::segments_len(new_internal.len(), new_header.len())
        .or_err(ErrorType::InternalError, "sizing the record's metadata")?;
    let capacity = parsed.header.meta_capacity();
    Ok((segments > capacity).then(|| segments - capacity))
}

/// Rewrite the segments within the reserved region, then commit with the
/// header block, with the read-only gate and the key's publication lock already
/// held by the caller. Two updates of one key interleaving their region and
/// header writes could otherwise leave a header whose checksum describes the
/// other's region, which no reader could accept
/// (FRE-COMPLIANCE.METAATOMIC-01). A lookup that finds the record between
/// the two writes re-reads, and at last waits for the lock
/// (SDS-LAYOUT.OBJECTFILE-01). The segments must fit the region
/// ([`shortfall`]).
///
/// Nothing is synchronised: after a host crash the region holds the old
/// metadata, the new, or a torn mixture, and the metadata checksum rejects
/// the mixture (ADR-PERSISTENCE.DURABILITY-01).
pub(crate) fn write_in_place(
    file: &File,
    parsed: &ParsedRecord,
    new_internal: &[u8],
    new_header: &[u8],
    #[cfg(test)] cache: &FsCache,
    #[cfg(test)] canonical: &Path,
) -> Result<()> {
    let meta_start = parsed.header.meta_region_offset() as u64;
    let mut region = Vec::with_capacity(new_internal.len() + new_header.len());
    region.extend_from_slice(new_internal);
    region.extend_from_slice(new_header);
    file.write_all_at(&region, meta_start).or_err(
        ErrorType::FileWriteError,
        "writing the metadata region in place",
    )?;
    // Bytes between the new content's end and `meta_capacity` are inert: a
    // reader only ever reads `internal_len`/`header_len` bytes, both
    // updated below.
    test_hook! { cache.hooks.pause(PausePoint::InPlaceBetweenWrites, canonical); }

    let mut header = parsed.header;
    header
        .edit()
        .key_and_metadata(&parsed.key_bytes, new_internal, new_header)
        .or_err(ErrorType::InternalError, "encoding the record header")?;

    // The header block is the commit point. A reader catching it mid-write
    // sees a CRC mismatch and re-reads (SDS-LAYOUT.OBJECTFILE-01), so it
    // observes the pre- or post-update set, never a mixture.
    file.write_all_at(header.as_bytes(), 0).or_err(
        ErrorType::FileWriteError,
        "writing the record header in place",
    )
}

/// Withdraw the record (SDS-LAYOUT.OBJECTFILE-01): rewrite only its header,
/// marked withdrawn, with the key's publication lock held by the caller.
/// Every lookup then misses, so the next request fetches the object again,
/// and its store, sized for the new metadata, replaces the file. Until then
/// the file keeps the space the eviction manager counts for the key, so the
/// manager stays right without being told. One header write: a reader
/// catching it mid-write re-reads, and then misses.
///
/// Nothing is synchronised, as for an update in place: a withdrawal that a
/// host crash undoes leaves the record as it was, stale, and the next
/// revalidation withdraws it again (ADR-PERSISTENCE.DURABILITY-01).
fn withdraw(file: &File, parsed: &ParsedRecord, #[cfg(test)] cache: &FsCache) -> Result<()> {
    let mut header = parsed.header;
    header.edit().withdraw();
    test_hook! { cache.hooks.fault(FaultPoint::WithdrawRecord)?; }
    file.write_all_at(header.as_bytes(), 0)
        .or_err(ErrorType::FileWriteError, "withdrawing the record")
}

/// Log and count an update whose new metadata outgrew its record's
/// headroom by `shortfall` bytes, so that the record at `canonical` was
/// withdrawn (FDS-OBSERVABILITY.HEADROOM-01). The path's file name is the
/// key's hex-encoded hash.
fn report_outgrown(canonical: &Path, shortfall: u32) {
    log::warn!(
        "{} of {} exceeded its reserved headroom by {shortfall} byte(s), so the record was \
         withdrawn and the next request fetches it again; consider increasing the configured \
         metadata headroom",
        Op::UpdateMeta,
        canonical.display()
    );
    metrics::record_headroom_exceeded();
}
