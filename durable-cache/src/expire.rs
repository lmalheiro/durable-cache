// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `Storage::expire` (FDS-STORAGE.EXPIRE-01): mark a stored object stale
//! without removing it, so the next request revalidates against the origin
//! and the origin's `304` is answered from the body this cache already
//! holds.
//!
//! The mechanism is the one FDS-STORAGE.METAUPDATE-01 already specifies
//! (ADR-STORAGE.EXPIRE-01): `CacheMeta::expire_at` changes one freshness
//! field in the internal-metadata segment, and the header segment and the
//! body are kept as stored, byte for byte. The expired metadata is never
//! longer than the stored metadata, so the expiry is always written in
//! place, whatever headroom the record has left. No body byte is read or
//! copied.
//!
//! The expired metadata is derived from what is on disk rather than from
//! anything the caller holds, because the caller of an expiry holds no
//! `CacheMeta` at all: it arrives with a key and nothing else.

// IMP-STORAGE.EXPIRE-01: satisfy FDS-STORAGE.EXPIRE-01
use crate::error::{refuse_read_only, Op};
use crate::key_codec;
use crate::metaupdate;
use crate::record::{self, ParsedRecord, ReadError, RecordKind};
use crate::FsCache;
use pingora_cache::key::CompactCacheKey;
use pingora_cache::storage::PurgeOutcome;
use pingora_cache::CacheMeta;
use pingora_error::{Error, ErrorType, OrErr, Result};
use std::fs::{File, OpenOptions};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Expire the record at `key`'s canonical path. The caller resolves the
/// `PurgeTarget` (an `Identified` one names a generation this backend never
/// issued, ADR-COMPAT.ENTRYID-01) and hands an owned key in, so this can run
/// on the blocking pool (FDS-STORAGE.EXECUTOR-01).
///
/// Synchronous throughout, as `update_meta` is: the read-only gate and the
/// publication lock are held across the durable part, and neither may be
/// held across an `.await`. Reports `Expired` once the expired metadata is
/// written, and so visible to lookups, and `NotFound` when the key holds no
/// record, or one that does not read.
pub(crate) fn expire_key(cache: &'static FsCache, key: &CompactCacheKey) -> Result<PurgeOutcome> {
    if cache.is_read_only() {
        // An expiry this process will not make durable fails rather than
        // being reported as applied (FRE-COMPAT.HOTUPGRADE-01).
        return refuse_read_only(Op::Expire);
    }
    let name = cache.object_name(key);
    let canonical = cache.canonical_path(&name)?;
    let key_bytes = key_codec::encode(key);

    // Under the read-only gate and the key's publication lock: open the
    // record, read its stored metadata, derive the expired metadata from it,
    // and write it back, as a metadata update does
    // (FDS-STORAGE.METAUPDATE-01). Opening the record under the lock means
    // the expiry works on the record the key holds now: every change that
    // replaces or removes it holds the lock while it does. Holding the lock
    // from the read to the write is what keeps a concurrent expiry, or a
    // revalidation's `update_meta`, from reading metadata this call is
    // between reading and rewriting.
    test_hook! { cache.hooks.pause(crate::hooks::PausePoint::ExpireBeforeLock, &canonical); }
    let Some(_change) = cache.locks.begin(&name) else {
        return refuse_read_only(Op::Expire);
    };

    let file = match OpenOptions::new().read(true).write(true).open(&canonical) {
        Ok(file) => file,
        // The absent-key case, which is not an error
        // (FRE-COMPLIANCE.PURGEABSENT-01).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PurgeOutcome::NotFound),
        Err(e) => {
            return Error::e_because(
                ErrorType::FileOpenError,
                format!("opening {}", canonical.display()),
                e,
            )
        }
    };
    let Some(parsed) = read(&file, &canonical, &key_bytes)? else {
        return Ok(PurgeOutcome::NotFound);
    };
    let new_internal = expired(cache, &parsed.internal_bytes, &parsed.header_bytes)?;
    test_hook! { cache.hooks.pause(crate::hooks::PausePoint::ExpireBeforeWrite, &canonical); }
    // With the header segment written back as stored, the segments then fit
    // the region the stored ones were read from.
    debug_assert!(
        new_internal.len() <= parsed.internal_bytes.len(),
        "an expiry lengthened the internal-metadata segment of {}",
        canonical.display()
    );
    metaupdate::write_in_place(
        &file,
        &parsed,
        &new_internal,
        &parsed.header_bytes,
        #[cfg(test)]
        cache,
        #[cfg(test)]
        &canonical,
    )?;
    Ok(PurgeOutcome::Expired)
}

/// Read the record, or `None` when nothing readable is stored under the
/// key.
///
/// A record that fails the read-time validation SDS-LAYOUT.OBJECTFILE-01
/// specifies — damaged, truncated, written under another format version, or
/// withdrawn — is reported absent, which is exactly what a lookup of the
/// same record reports, and is left for the restart scan
/// (FDS-STORAGE.RECLAIM-01) rather than written into.
fn read(
    file: &File,
    canonical: &std::path::Path,
    key_bytes: &[u8],
) -> Result<Option<ParsedRecord>> {
    match record::read_record_blocking(file, RecordKind::Published, Some(key_bytes)) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(ReadError::Record(e)) => {
            log::debug!(
                "{} found {} unreadable: {e}",
                Op::Expire,
                canonical.display()
            );
            Ok(None)
        }
        Err(ReadError::Io(e)) => Error::e_because(
            ErrorType::FileReadError,
            format!("reading {}", canonical.display()),
            e,
        ),
    }
}

/// The expired form of the internal-metadata segment in `internal`,
/// serialised, and never longer than `internal`. `header` is decoded only
/// because `CacheMeta` decodes both segments together: the header segment
/// itself is kept as stored.
fn expired(cache: &FsCache, internal: &[u8], header: &[u8]) -> Result<Vec<u8>> {
    let mut meta: CacheMeta = cache.decode_meta(internal, header)?;
    // The framework owns what "expired" means, including how the serve-stale
    // windows the response set are measured from here; this backend only
    // stores the result. It expires at the start of the current second, not
    // at the current instant, so that the expired metadata is never longer
    // than the stored metadata. MessagePack writes the deadline's seconds and
    // nanoseconds each in as few bytes as its value allows; `expire_at` only
    // ever lowers the deadline, so its seconds take no more bytes than
    // before, and zero nanoseconds take the fewest. The serve-stale windows
    // close up to a second earlier, below HTTP's one-second resolution. The
    // unit tests below check this for every width the encoding gives a
    // deadline, so a `pingora-cache` that encodes it otherwise fails them.
    let second = start_of_second(SystemTime::now())?;
    debug_assert!(
        matches!(second.duration_since(UNIX_EPOCH), Ok(since) if since.subsec_nanos() == 0),
        "an expiry's instant is not on a whole second"
    );
    meta.expire_at(second);
    // `expire_at` changes the internal segment only, but `CacheMeta`
    // serialises both segments together; the header's encoding is dropped.
    let (expired_internal, _) = meta.serialize()?;
    Ok(expired_internal)
}

/// `now` without its fraction of a second. Fails for a clock set before
/// 1970.
fn start_of_second(now: SystemTime) -> Result<SystemTime> {
    let since_epoch = now.duration_since(UNIX_EPOCH).or_err(
        ErrorType::InternalError,
        "the system clock is set before 1970",
    )?;
    Ok(UNIX_EPOCH + Duration::from_secs(since_epoch.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_http::ResponseHeader;

    /// Seconds on each side of the boundary between MessagePack's five-byte
    /// integers and its nine-byte ones (2^32, in 2106), up to the last
    /// second of the year 9999, the latest an `Expires` header can name.
    const SECS: [u64; 4] = [
        1_800_000_000,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        253_402_300_799,
    ];

    /// Nanoseconds on each side of every boundary between MessagePack's
    /// integer widths: one byte up to 127, two up to 255, three up to
    /// 65,535, five above.
    const NANOS: [u32; 8] = [0, 127, 128, 255, 256, 65_535, 65_536, 999_999_999];

    /// What the expiry relies on: expired at the start of any second, a
    /// deadline of any width leaves the internal-metadata segment no longer
    /// than it was. Every stored deadline is tried against every instant,
    /// earlier and later, so both a lowered deadline and one left as it is
    /// are covered.
    #[test]
    fn an_expiry_at_the_start_of_a_second_never_lengthens_the_internal_segment() {
        let instants = || {
            SECS.iter().flat_map(|&secs| {
                NANOS
                    .iter()
                    .map(move |&nanos| UNIX_EPOCH + Duration::new(secs, nanos))
            })
        };
        let created = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        for deadline in instants() {
            let header = ResponseHeader::build(200, None).expect("a response header");
            let stored = CacheMeta::new(deadline, created, 60, 60, header);
            let (internal, header) = stored.serialize().expect("the metadata serialises");
            for at in instants() {
                let mut meta =
                    CacheMeta::deserialize(&internal, &header).expect("the metadata decodes");
                meta.expire_at(start_of_second(at).expect("an instant after 1970"));
                let (expired, _) = meta.serialize().expect("the expired metadata serialises");
                assert!(
                    expired.len() <= internal.len(),
                    "expiring a deadline of {deadline:?} at {at:?} lengthened the \
                     internal-metadata segment from {} to {} bytes",
                    internal.len(),
                    expired.len()
                );
            }
        }
    }

    /// The start of a second keeps the second and drops the rest.
    #[test]
    fn the_start_of_a_second_drops_only_the_fraction() {
        let at = UNIX_EPOCH + Duration::new(1_800_000_000, 999_999_999);
        assert_eq!(
            start_of_second(at).expect("an instant after 1970"),
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        );
        let second = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(
            start_of_second(second).expect("an instant after 1970"),
            second,
            "a whole second is its own start"
        );
    }
}
