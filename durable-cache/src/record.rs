// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The read-time validation of a record (SDS-LAYOUT.OBJECTFILE-01), which
//! makes a damaged or foreign record a miss rather than a corrupt read. The
//! format it validates, the header included, is defined in `format.rs`.

use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};

use zerocopy::{FromZeros, IntoBytes};

use crate::format::{self, HeaderError, RecordHeader};

/// How many times a reader reads the header and metadata region on a CRC
/// mismatch: the first read plus three more (SDS-LAYOUT.OBJECTFILE-01,
/// "Metadata updates in place"). A lookup then reads once more holding the
/// key's publication lock (see `crate::lookup`); a caller that already holds
/// it takes a mismatch that persists as damage.
pub const META_READ_ATTEMPTS: u32 = 4;

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
    read_record_with(file, kind, expected_key, <File as FileExt>::read_exact_at)
}

/// [`read_record_once`] from the page cache only, for the
/// worker's fast path (`crate::nowait`): a read that would wait for the
/// device fails with an I/O error of kind `WouldBlock`.
#[cfg(target_os = "linux")]
pub fn read_record_nowait(
    file: &File,
    kind: RecordKind,
    expected_key: Option<&[u8]>,
) -> Result<ParsedRecord, ReadError> {
    read_record_with(file, kind, expected_key, crate::nowait::read_exact_at)
}

/// [`read_record_once`], reading each part of the record with `read`.
fn read_record_with(
    file: &File,
    kind: RecordKind,
    expected_key: Option<&[u8]>,
    read: fn(&File, &mut [u8], u64) -> io::Result<()>,
) -> Result<ParsedRecord, ReadError> {
    let meta = file.metadata()?;
    let file_len = meta.len();
    // A file shorter than a header comes back from this read as truncated.
    let mut header = RecordHeader::new_zeroed();
    read_record_part(read, file, header.as_mut_bytes(), 0)?;
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
    read_record_part(read, file, &mut key_bytes, format::KEY_OFFSET as u64)?;
    let meta_start = header.meta_region_offset() as u64;
    let mut internal_bytes = vec![0u8; internal_len as usize];
    read_record_part(read, file, &mut internal_bytes, meta_start)?;
    let mut header_bytes = vec![0u8; header_len as usize];
    read_record_part(
        read,
        file,
        &mut header_bytes,
        meta_start + internal_len as u64,
    )?;
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
    Ok(ParsedRecord {
        header,
        key_bytes,
        internal_bytes,
        header_bytes,
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
    let mut attempt = 1;
    loop {
        match read_record_once(file, kind, expected_key) {
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
}
