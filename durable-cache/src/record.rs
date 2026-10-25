// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The read-time validation of a record (SDS-LAYOUT.OBJECTFILE-01), which
//! makes a damaged or foreign record a miss rather than a corrupt read. The
//! format it validates, the header included, is defined in `format.rs`.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};

use bytes::Bytes;
use zerocopy::{FromZeros, IntoBytes};

use crate::format::{self, HeaderError, RecordHeader};

/// How many times a reader reads the header and metadata region on a CRC
/// mismatch: the first read plus three more (SDS-LAYOUT.OBJECTFILE-01,
/// "Metadata updates in place"). A lookup then reads once more holding the
/// key's publication lock (see `crate::lookup`); a caller that already holds
/// it takes a mismatch that persists as damage.
pub const META_READ_ATTEMPTS: u32 = 4;

/// How many of a published record's first bytes a lookup reads at once:
/// the header, key and metadata region of an ordinary record, and the start
/// of its body, which the hit then serves without reading it again. A
/// record whose metadata region reaches further has the rest read part by
/// part.
pub const LOOKUP_READ_LEN: u64 = 16 * 1024;

/// Why a record read was rejected. Every variant is reported as a miss; the
/// distinction exists for logging, the reclaim scan's classification, and
/// tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error("the file ended before the record did")]
    Truncated,
    #[error("metadata CRC mismatch")]
    MetaCrcMismatch,
    #[error("stored key differs from the looked-up key")]
    KeyMismatch,
    #[error("file length {actual} does not match computed extent {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
}

impl RecordError {
    /// A CRC mismatch may be a reader catching an in-place metadata update
    /// mid-write, so it is retried; every other failure is final.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            RecordError::Header(HeaderError::CrcMismatch) | RecordError::MetaCrcMismatch
        )
    }
}

/// Whether the record being read is published (its length must be exact)
/// or a store's still-growing temporary (its `body_len` is not patched in
/// until `finish`, so only the header, key, and metadata are checked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    Published,
    InFlight,
}

/// A record's header, key, and metadata segments, validated.
#[derive(Debug, Clone)]
pub struct ParsedRecord {
    pub header: RecordHeader,
    pub key_bytes: Vec<u8>,
    pub internal_bytes: Vec<u8>,
    pub header_bytes: Vec<u8>,
    /// The body's first bytes, when a lookup's first read reached them
    /// ([`read_published_once`]); empty otherwise.
    pub body_prefix: Bytes,
}

/// A file's identity: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    pub fn of(meta: &std::fs::Metadata) -> Self {
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

/// How a record's bytes are read: waiting for the device if need be, or
/// from the page cache only (`crate::nowait`).
#[derive(Clone, Copy)]
struct Reads {
    /// Fill the buffer, or fail.
    exact: fn(&File, &mut [u8], u64) -> io::Result<()>,
    /// Fill as much of the buffer as can be read without waiting, if
    /// waiting is not allowed: how many bytes, at least one.
    prefix: fn(&File, &mut [u8], u64) -> io::Result<usize>,
}

/// Reads that wait for the device when they must.
const BLOCKING: Reads = Reads {
    exact: <File as FileExt>::read_exact_at,
    prefix: |file, buf, offset| file.read_exact_at(buf, offset).map(|()| buf.len()),
};

/// Reads from the page cache only (`crate::nowait`).
#[cfg(target_os = "linux")]
const NOWAIT: Reads = Reads {
    exact: crate::nowait::read_exact_at,
    prefix: crate::nowait::read_at_most,
};

/// A failed read: either the record is unacceptable, or the file could not
/// be read at all.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error(transparent)]
    Record(#[from] RecordError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read and validate one record, once, in the order
/// SDS-LAYOUT.OBJECTFILE-01 specifies: magic and version, `header_crc`,
/// `meta_crc`, the stored key (when `expected_key` is given), and, for a
/// published record, the exact length.
///
/// The header is checked before any length it carries is trusted, so a
/// damaged header can never make this allocate or read an arbitrary size.
pub fn read_record_once(
    file: &File,
    kind: RecordKind,
    expected_key: Option<&[u8]>,
) -> Result<ParsedRecord, ReadError> {
    read_record_with(file, kind, expected_key, BLOCKING, 0)
}

/// [`read_record_once`] for a lookup of the published record of
/// `expected_key`. One read of the file's first [`LOOKUP_READ_LEN`] bytes
/// yields the header, key and metadata region of an ordinary record, and
/// the start of its body, returned in [`ParsedRecord::body_prefix`].
pub fn read_published_once(file: &File, expected_key: &[u8]) -> Result<ParsedRecord, ReadError> {
    read_record_with(
        file,
        RecordKind::Published,
        Some(expected_key),
        BLOCKING,
        LOOKUP_READ_LEN,
    )
}

/// [`read_published_once`] from the page cache only, for the worker's fast
/// path (`crate::nowait`): a read that would wait for the device fails with
/// an I/O error of kind `WouldBlock`. The first read keeps what the page
/// cache holds of the file's first [`LOOKUP_READ_LEN`] bytes, so that the
/// lookup succeeds when that covers the metadata, with a shorter
/// [`ParsedRecord::body_prefix`].
#[cfg(target_os = "linux")]
pub fn read_published_nowait(file: &File, expected_key: &[u8]) -> Result<ParsedRecord, ReadError> {
    read_record_with(
        file,
        RecordKind::Published,
        Some(expected_key),
        NOWAIT,
        LOOKUP_READ_LEN,
    )
}

/// [`read_record_once`] for the temporary of a store in flight, from the
/// page cache only, for the worker's fast path (`crate::nowait`): a read
/// that would wait for the device fails with an I/O error of kind
/// `WouldBlock`.
#[cfg(target_os = "linux")]
pub fn read_in_flight_nowait(file: &File, expected_key: &[u8]) -> Result<ParsedRecord, ReadError> {
    read_record_with(
        file,
        RecordKind::InFlight,
        Some(expected_key),
        NOWAIT,
        LOOKUP_READ_LEN,
    )
}

/// [`read_record_once`], reading the record with `reads`. When `first_read`
/// is above zero, one read of up to the file's first `first_read` bytes
/// comes first, and each part it covers is taken from it instead of being
/// read on its own.
fn read_record_with(
    file: &File,
    kind: RecordKind,
    expected_key: Option<&[u8]>,
    reads: Reads,
    first_read: u64,
) -> Result<ParsedRecord, ReadError> {
    let meta = file.metadata()?;
    let file_len = meta.len();
    let mut first = vec![0u8; file_len.min(first_read) as usize];
    if !first.is_empty() {
        let read = match (reads.prefix)(file, &mut first, 0) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(RecordError::Truncated.into())
            }
            Err(e) => return Err(e.into()),
        };
        first.truncate(read);
    }
    let read = reads.exact;
    let part = |buf: &mut [u8], offset: u64| -> Result<(), ReadError> {
        let covered = usize::try_from(offset)
            .ok()
            .and_then(|start| first.get(start..)?.get(..buf.len()));
        match covered {
            Some(bytes) => {
                buf.copy_from_slice(bytes);
                Ok(())
            }
            None => read_record_part(read, file, buf, offset),
        }
    };
    // A file shorter than a header comes back from this read as truncated.
    let mut header = RecordHeader::new_zeroed();
    part(header.as_mut_bytes(), 0)?;
    header.check().map_err(RecordError::Header)?;
    let key_len = header.key_len();
    let internal_len = header.internal_len();
    let header_len = header.header_len();
    let body_offset = header.body_offset() as u64;
    if body_offset > file_len {
        return Err(RecordError::LengthMismatch {
            expected: header.record_len(),
            actual: file_len,
        }
        .into());
    }

    let mut key_bytes = vec![0u8; key_len as usize];
    part(&mut key_bytes, format::KEY_OFFSET as u64)?;
    let meta_start = header.meta_region_offset() as u64;
    let mut internal_bytes = vec![0u8; internal_len as usize];
    part(&mut internal_bytes, meta_start)?;
    let mut header_bytes = vec![0u8; header_len as usize];
    part(&mut header_bytes, meta_start + internal_len as u64)?;
    if format::compute_meta_crc(&key_bytes, &internal_bytes, &header_bytes) != header.meta_crc() {
        return Err(RecordError::MetaCrcMismatch.into());
    }
    if let Some(expected) = expected_key {
        if expected != key_bytes.as_slice() {
            return Err(RecordError::KeyMismatch.into());
        }
    }
    if kind == RecordKind::Published && file_len != header.record_len() {
        return Err(RecordError::LengthMismatch {
            expected: header.record_len(),
            actual: file_len,
        }
        .into());
    }
    // A published record's length is exact, so whatever the first read
    // holds past the body offset is the body's start.
    let body_offset = header.body_offset();
    let body_prefix = if kind == RecordKind::Published && first.len() > body_offset {
        Bytes::from(first).slice(body_offset..)
    } else {
        Bytes::new()
    };
    Ok(ParsedRecord {
        header,
        key_bytes,
        internal_bytes,
        header_bytes,
        body_prefix,
    })
}

/// [`read_record_once`], retried on a CRC mismatch up to
/// [`META_READ_ATTEMPTS`] reads in total, yielding the thread between
/// attempts.
///
/// Blocking, and deliberately so: every caller reaches it from Tokio's
/// blocking pool (FDS-STORAGE.EXECUTOR-01), where yielding the thread is the
/// right thing to do and an `.await` is not available.
pub fn read_record_blocking(
    file: &File,
    kind: RecordKind,
    expected_key: Option<&[u8]>,
) -> Result<ParsedRecord, ReadError> {
    retried(|| read_record_once(file, kind, expected_key))
}

/// [`read_published_once`], retried as [`read_record_blocking`] retries.
/// Blocking, for the same reason.
pub fn read_published_blocking(
    file: &File,
    expected_key: &[u8],
) -> Result<ParsedRecord, ReadError> {
    retried(|| read_published_once(file, expected_key))
}

/// `read` once, and again on a CRC mismatch, up to [`META_READ_ATTEMPTS`]
/// reads in total, yielding the thread between attempts.
fn retried(
    mut read: impl FnMut() -> Result<ParsedRecord, ReadError>,
) -> Result<ParsedRecord, ReadError> {
    let mut attempt = 1;
    loop {
        match read() {
            Err(ReadError::Record(e)) if e.is_retryable() && attempt < META_READ_ATTEMPTS => {
                attempt += 1;
                std::thread::yield_now();
            }
            result => return result,
        }
    }
}

/// Read one part of a record (the header, the key, or a metadata segment)
/// at `offset`, filling `buf`. A part the file does not reach means the
/// record is truncated, which is reported as damage,
/// [`RecordError::Truncated`], not as an I/O failure. Past the header, the
/// checks before each read keep it inside the length the file had when the
/// read began, so only a file shrinking meanwhile reaches the error, and
/// nothing in this backend shrinks a record.
fn read_record_part(
    read: fn(&File, &mut [u8], u64) -> io::Result<()>,
    file: &File,
    buf: &mut [u8],
    offset: u64,
) -> Result<(), ReadError> {
    match read(file, buf, offset) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(RecordError::Truncated.into()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::mem::size_of;

    use super::*;

    /// A complete record with a 3-byte key, 4-byte internal segment, 5-byte
    /// header segment, 64-byte capacity, and 10-byte body.
    fn record_bytes() -> Vec<u8> {
        let key = b"key";
        let internal = b"intl";
        let hdr = b"heads";
        let body = b"0123456789";
        let mut header = RecordHeader::new();
        header
            .edit()
            .meta_capacity(64)
            .key_and_metadata(key, internal, hdr)
            .expect("short segments")
            .body(body.len() as u64, crc32c::crc32c(body));
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(key);
        out.extend_from_slice(internal);
        out.extend_from_slice(hdr);
        out.resize(header.body_offset(), 0);
        out.extend_from_slice(body);
        out
    }

    fn file_with(bytes: &[u8]) -> File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(bytes).unwrap();
        f
    }

    fn read(bytes: &[u8], key: Option<&[u8]>) -> Result<ParsedRecord, RecordError> {
        match read_record_once(&file_with(bytes), RecordKind::Published, key) {
            Ok(p) => Ok(p),
            Err(ReadError::Record(e)) => Err(e),
            Err(ReadError::Io(e)) => panic!("unexpected I/O error: {e}"),
        }
    }

    #[test]
    fn valid_record_passes() {
        let parsed = read(&record_bytes(), Some(b"key")).unwrap();
        assert_eq!(parsed.internal_bytes, b"intl");
        assert_eq!(parsed.header_bytes, b"heads");
    }

    #[test]
    fn flipped_magic_is_rejected() {
        let mut bytes = record_bytes();
        bytes[format::MAGIC_OFFSET] ^= 0xFF;
        assert_eq!(
            read(&bytes, None).unwrap_err(),
            RecordError::Header(HeaderError::BadMagic)
        );
    }

    #[test]
    fn flipped_version_is_rejected() {
        let mut bytes = record_bytes();
        bytes[format::VERSION_OFFSET] ^= 0xFF;
        assert!(matches!(
            read(&bytes, None).unwrap_err(),
            RecordError::Header(HeaderError::UnsupportedVersion(_))
        ));
    }

    /// A withdrawn record, sealed as the withdrawal seals it, reads as
    /// withdrawn: a reader tells it apart from a damaged record.
    #[test]
    fn a_withdrawn_record_reads_as_withdrawn() {
        let bytes = with_field(
            record_bytes(),
            format::VERSION_OFFSET,
            &format::WITHDRAWN_VERSION.to_le_bytes(),
        );
        assert_eq!(
            read(&bytes, Some(b"key")).expect_err("a withdrawn record does not read"),
            RecordError::Header(HeaderError::Withdrawn)
        );
    }

    #[test]
    fn corrupted_header_fails_crc() {
        let mut bytes = record_bytes();
        bytes[format::BODY_LEN_OFFSET] ^= 0xFF;
        assert_eq!(
            read(&bytes, None).unwrap_err(),
            RecordError::Header(HeaderError::CrcMismatch)
        );
    }

    #[test]
    fn corrupted_metadata_fails_crc() {
        let mut bytes = record_bytes();
        let at = format::KEY_OFFSET + 3; // right after the 3-byte key
        bytes[at] ^= 0xFF;
        assert_eq!(
            read(&bytes, None).unwrap_err(),
            RecordError::MetaCrcMismatch
        );
    }

    #[test]
    fn different_key_is_rejected() {
        assert_eq!(
            read(&record_bytes(), Some(b"other")).unwrap_err(),
            RecordError::KeyMismatch
        );
    }

    #[test]
    fn truncated_and_extended_files_are_rejected() {
        let bytes = record_bytes();
        assert!(matches!(
            read(&bytes[..bytes.len() - 1], None).unwrap_err(),
            RecordError::LengthMismatch { .. }
        ));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(
            read(&longer, None).unwrap_err(),
            RecordError::LengthMismatch { .. }
        ));
    }

    #[test]
    fn in_flight_record_skips_length_check() {
        let bytes = record_bytes();
        let file = file_with(&bytes[..bytes.len() - 4]);
        read_record_once(&file, RecordKind::InFlight, None).unwrap();
    }

    /// The record with the header field at `offset` overwritten by `value`'s
    /// little-endian bytes, and its header CRC resealed independently of the
    /// code under test, as a forger would.
    fn with_field(mut bytes: Vec<u8>, offset: usize, value: &[u8]) -> Vec<u8> {
        bytes[offset..offset + value.len()].copy_from_slice(value);
        let crc = crc32c::crc32c(&bytes[..format::HEADER_CRC_OFFSET]);
        bytes[format::HEADER_CRC_OFFSET..format::HEADER_CRC_OFFSET + size_of::<u32>()]
            .copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// A `key_len` that puts the key and the body past the end of the file is
    /// rejected by the body-offset bound, before the key's buffer is
    /// allocated. Without the bound, the reader would allocate a 4 GiB
    /// buffer and fail only when the read ran out of file, as `Truncated`.
    #[test]
    fn a_header_length_past_the_end_of_the_file_is_rejected_before_any_read() {
        let bytes = with_field(
            record_bytes(),
            format::KEY_LEN_OFFSET,
            &u32::MAX.to_le_bytes(),
        );
        assert!(matches!(
            read(&bytes, None).expect_err("a key cannot lie past the file's end"),
            RecordError::LengthMismatch { .. }
        ));
    }

    /// A forged `body_len` near the top of its range is rejected as a length
    /// mismatch. Adding it to the body offset must not overflow: that would
    /// panic in a debug build, and in a release build wrap to a smaller
    /// length.
    #[test]
    fn a_huge_body_length_is_rejected_without_overflow() {
        let bytes = with_field(
            record_bytes(),
            format::BODY_LEN_OFFSET,
            &u64::MAX.to_le_bytes(),
        );
        assert!(matches!(
            read(&bytes, None).expect_err("no file is that long"),
            RecordError::LengthMismatch { .. }
        ));
    }

    #[test]
    fn a_file_shorter_than_a_header_is_truncated() {
        assert_eq!(
            read(&record_bytes()[..10], None).unwrap_err(),
            RecordError::Truncated
        );
    }

    /// A record keyed `key`, with the header segment `hdr` and the body
    /// `body`, and a metadata region 64 bytes larger than its segments.
    fn record_with(hdr: &[u8], body: &[u8]) -> Vec<u8> {
        let key = b"key";
        let internal = b"intl";
        let capacity = u32::try_from(key.len() + internal.len() + hdr.len() + 64)
            .expect("a test's segments fit a u32");
        let mut header = RecordHeader::new();
        header
            .edit()
            .meta_capacity(capacity)
            .key_and_metadata(key, internal, hdr)
            .expect("the segments fit the capacity")
            .body(body.len() as u64, crc32c::crc32c(body));
        let mut out = header.as_bytes().to_vec();
        out.extend_from_slice(key);
        out.extend_from_slice(internal);
        out.extend_from_slice(hdr);
        out.resize(header.body_offset(), 0);
        out.extend_from_slice(body);
        out
    }

    /// A body of `len` bytes that differ from their neighbours, so a slice
    /// taken from the wrong place does not compare equal.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn read_published(bytes: &[u8]) -> ParsedRecord {
        read_published_once(&file_with(bytes), b"key").expect("a valid record reads")
    }

    /// A lookup's read of a small record returns the whole body with the
    /// metadata, from its one first read.
    #[test]
    fn a_lookup_read_returns_the_body_with_the_metadata() {
        let parsed = read_published(&record_bytes());
        assert_eq!(parsed.internal_bytes, b"intl");
        assert_eq!(parsed.header_bytes, b"heads");
        assert_eq!(&parsed.body_prefix[..], b"0123456789");
    }

    /// The body prefix ends where the first read did, in a record longer
    /// than it.
    #[test]
    fn a_lookup_read_returns_the_body_up_to_the_end_of_its_first_read() {
        let body = pattern(40_000);
        let bytes = record_with(b"heads", &body);
        let parsed = read_published(&bytes);
        let body_offset = parsed.header.body_offset();
        assert_eq!(
            &parsed.body_prefix[..],
            &body[..LOOKUP_READ_LEN as usize - body_offset]
        );
    }

    /// Metadata reaching past the first read is read part by part, and no
    /// body prefix comes back.
    #[test]
    fn a_lookup_read_reads_metadata_past_its_first_read_on_its_own() {
        let hdr = pattern(LOOKUP_READ_LEN as usize + 5000);
        let bytes = record_with(&hdr, b"0123456789");
        let parsed = read_published(&bytes);
        assert_eq!(parsed.header_bytes, hdr);
        assert!(parsed.body_prefix.is_empty());
    }

    /// Reads that find only the file's first page cached, as the page cache
    /// can leave a record.
    const FIRST_PAGE_CACHED: Reads = Reads {
        exact: <File as FileExt>::read_exact_at,
        prefix: |file, buf, offset| {
            let cached = buf.len().min(4096);
            file.read_exact_at(&mut buf[..cached], offset)
                .map(|()| cached)
        },
    };

    /// A first read that finds only part of its bytes cached keeps them:
    /// the record reads, with the body prefix those bytes hold.
    #[test]
    fn a_lookup_read_keeps_a_partly_cached_first_read() {
        let body = pattern(40_000);
        let file = file_with(&record_with(b"heads", &body));
        let parsed = read_record_with(
            &file,
            RecordKind::Published,
            Some(b"key"),
            FIRST_PAGE_CACHED,
            LOOKUP_READ_LEN,
        )
        .expect("the record reads");
        assert_eq!(parsed.header_bytes, b"heads");
        assert_eq!(
            &parsed.body_prefix[..],
            &body[..4096 - parsed.header.body_offset()]
        );
    }

    /// The lookup's read rejects every record the part-by-part read
    /// rejects, for the same reason.
    #[test]
    fn a_lookup_read_rejects_what_a_part_by_part_read_rejects() {
        let bytes = record_bytes();
        let mut flipped_magic = bytes.clone();
        flipped_magic[format::MAGIC_OFFSET] ^= 0xFF;
        let mut flipped_header = bytes.clone();
        flipped_header[format::BODY_LEN_OFFSET] ^= 0xFF;
        let mut flipped_meta = bytes.clone();
        flipped_meta[format::KEY_OFFSET + 3] ^= 0xFF;
        let mut longer = bytes.clone();
        longer.push(0);
        let cases: [(&str, &[u8], &[u8]); 7] = [
            ("magic", &flipped_magic, b"key"),
            ("header CRC", &flipped_header, b"key"),
            ("metadata CRC", &flipped_meta, b"key"),
            ("key", &bytes, b"other"),
            ("one byte short", &bytes[..bytes.len() - 1], b"key"),
            ("one byte long", &longer, b"key"),
            ("shorter than a header", &bytes[..10], b"key"),
        ];
        let rejection = |read: Result<ParsedRecord, ReadError>| match read {
            Ok(_) => None,
            Err(ReadError::Record(e)) => Some(e),
            Err(ReadError::Io(e)) => panic!("unexpected I/O error: {e}"),
        };
        for (case, bytes, key) in cases {
            let file = file_with(bytes);
            let by_parts = rejection(read_record_once(&file, RecordKind::Published, Some(key)));
            let at_once = rejection(read_published_once(&file, key));
            assert!(by_parts.is_some(), "{case}: the record is rejected");
            assert_eq!(at_once, by_parts, "{case}: both reads reject it alike");
        }
    }
}
