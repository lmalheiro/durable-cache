// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The on-disk record format (SDS-LAYOUT.OBJECTFILE-01): the record header,
//! the offsets derived from it, how a valid header is built and sealed, and
//! what the metadata checksum covers.
//!
//! The tests byte-flip and truncate records at the offsets defined here
//! rather than duplicating the format: `durable-cache/tests/*` from
//! inside the crate, and `test/harness`'s tamper helpers through
//! `test_support` (the `test-support` feature).

use std::mem::{offset_of, size_of};
use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

// IMP-LAYOUT.OBJECTFILE-01: satisfy SDS-LAYOUT.OBJECTFILE-01
/// The fixed record header, exactly as it lies on disk
/// (SDS-LAYOUT.OBJECTFILE-01, "Record header: one aligned block").
///
/// The backend works on it directly: a record's first [`HEADER_LEN`] bytes
/// are read straight into it and written straight from it. The fields are
/// little-endian integer types, so the block is byte-identical on every
/// host and the only byte swapping happens in the getters and in
/// [`HeaderEdit`]'s setters on a big-endian one. The fields are private:
/// they are read through the getters and changed only through
/// [`Self::edit`], whose guard reseals the header when it is dropped, so a
/// header built here never carries a stale `header_crc`. `#[repr(C)]` is
/// load-bearing: it fixes the field order the offsets below are derived
/// from.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct RecordHeader {
    /// Identifies a record of this crate: [`MAGIC`], the bytes `PDCR`. A file
    /// without it is not a record: a lookup reports a miss, and the restart
    /// scan reclaims it.
    magic: U32,
    /// The record format revision, [`FORMAT_VERSION`], or
    /// [`WITHDRAWN_VERSION`] once the record is withdrawn. A record of any
    /// other revision is a miss, never decoded, and the restart scan
    /// reclaims it (FRE-COMPAT.FORMATUPGRADE-01).
    version: U32,
    /// The body's exact length in bytes. Zero while the store is in flight:
    /// `finish` patches it in, and until then a partial hit takes the valid
    /// length from the in-flight registry instead. A published record's file
    /// must be exactly [`Self::body_offset`] plus this long, or it is a miss.
    body_len: U64,
    /// The metadata region's size as allocated in this record: the two
    /// segments `CacheMeta::serialize` produces, internal and header (see
    /// `internal_len` and `header_len`), plus the headroom configured when it
    /// was written. It places the body, and a metadata update stays in place
    /// only while the new segments fit in it (FDS-STORAGE.METAUPDATE-01).
    meta_capacity: U32,
    /// The length of the serialised `CompactCacheKey` right after the
    /// header. It varies per key, since the variance and the user tag are
    /// optional, and it places the metadata region. A lookup compares the
    /// stored key with the one it looked up; the restart scan decodes it to
    /// admit the record.
    key_len: U32,
    /// The length of `CacheMeta`'s internal segment, at the start of the
    /// metadata region. An in-place metadata update changes it.
    internal_len: U32,
    /// The length of `CacheMeta`'s header segment, right after the internal
    /// one. The region's bytes after it are free headroom. An in-place
    /// metadata update changes it.
    header_len: U32,
    /// CRC-32C over the key and both metadata segments. A mismatch means a
    /// torn or damaged region: the reader re-reads, since an in-place update
    /// may be under way, and then reports a miss.
    meta_crc: U32,
    /// CRC-32C over the body's `body_len` bytes, folded chunk by chunk as the
    /// body streams in and patched in by `finish`; zero in a build without
    /// the `body-crc` feature. Serving a hit does not check it: integrity at
    /// rest is the storage layer's (ADR-PERSISTENCE.DURABILITY-01). A build
    /// with `body-crc-verify` can switch on a check that logs a mismatch once
    /// a read has delivered the whole body from its start. A stored zero is
    /// never checked.
    body_crc: U32,
    /// CRC-32C over the header's first 40 bytes, every field above. It is
    /// recomputed whenever an edit ends (see [`HeaderEdit`]), and an
    /// in-place metadata update writes the header last, so this is the
    /// update's commit point. A torn or damaged header fails it.
    header_crc: U32,
}

/// Byte length of the fixed record header.
pub const HEADER_LEN: usize = size_of::<RecordHeader>();

/// Identifies this crate's record format: the four bytes `PDCR`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"PDCR");
/// Current record format revision. The backend decides when its format
/// changes; a Pingora release does not by itself (FRE-COMPAT.FORMATUPGRADE-01).
/// Bump it whenever records written before the change would no longer be
/// read back exactly: a change to this layout, or a `pingora-cache` release
/// whose metadata encoding differs, which the golden-bytes test of
/// TC-COMPAT.FORMATUPGRADE-01 flags. Every record of another revision is a
/// miss, so a bump starts the cache cold.
pub const FORMAT_VERSION: u32 = 1;
/// The version of a withdrawn record: one whose new metadata outgrew its
/// region, so that a metadata update withdrew it rather than write it
/// (FDS-STORAGE.METAUPDATE-01). Its header is otherwise as it was, and
/// sealed. Every reader treats the key as holding nothing, so the next
/// request fetches the object again, and its store replaces the file; the
/// restart scan reclaims one it finds (FDS-STORAGE.RECLAIM-01).
pub const WITHDRAWN_VERSION: u32 = 0;

pub const MAGIC_OFFSET: usize = offset_of!(RecordHeader, magic);
pub const VERSION_OFFSET: usize = offset_of!(RecordHeader, version);
pub const BODY_LEN_OFFSET: usize = offset_of!(RecordHeader, body_len);
pub const META_CAPACITY_OFFSET: usize = offset_of!(RecordHeader, meta_capacity);
pub const KEY_LEN_OFFSET: usize = offset_of!(RecordHeader, key_len);
pub const INTERNAL_LEN_OFFSET: usize = offset_of!(RecordHeader, internal_len);
pub const HEADER_LEN_OFFSET: usize = offset_of!(RecordHeader, header_len);
pub const META_CRC_OFFSET: usize = offset_of!(RecordHeader, meta_crc);
pub const BODY_CRC_OFFSET: usize = offset_of!(RecordHeader, body_crc);
pub const HEADER_CRC_OFFSET: usize = offset_of!(RecordHeader, header_crc);

// --- Frozen wire contract -------------------------------------------------
//
// Deriving the offsets removes one hazard (a literal drifting away from the
// field it describes) but introduces another: reordering a field in
// `RecordHeader` would silently relocate it on disk, orphaning every object
// already stored by a previous build. These assertions are the format's
// current specification, stated once, and they turn that accident into a
// build failure instead of a data-loss incident.
//
// Changing any of these numbers is a format break: it requires bumping
// `FORMAT_VERSION`, since `record::read_record_once` rejects a record of any
// other version, and every stored object would otherwise be misread.
const _: () = {
    assert!(HEADER_LEN == 44);
    assert!(MAGIC_OFFSET == 0);
    assert!(VERSION_OFFSET == 4);
    assert!(BODY_LEN_OFFSET == 8);
    assert!(META_CAPACITY_OFFSET == 16);
    assert!(KEY_LEN_OFFSET == 20);
    assert!(INTERNAL_LEN_OFFSET == 24);
    assert!(HEADER_LEN_OFFSET == 28);
    assert!(META_CRC_OFFSET == 32);
    assert!(BODY_CRC_OFFSET == 36);
    assert!(HEADER_CRC_OFFSET == 40);
    // No interior or trailing padding: the header is exactly its fields.
    // Padding would mean bytes that no field owns -- silent dead space in a
    // format meant to be exhaustive.
    // Nine u32 fields and one u64, and nothing else.
    assert!(HEADER_LEN == 9 * size_of::<u32>() + size_of::<u64>());
};

/// The key segment starts immediately after the fixed header.
pub const KEY_OFFSET: usize = HEADER_LEN;

impl RecordHeader {
    /// A sealed header of this format revision: `magic` and `version`
    /// filled in, every other field zero. Set the rest with [`Self::edit`].
    pub fn new() -> Self {
        let mut header = RecordHeader {
            magic: MAGIC.into(),
            version: FORMAT_VERSION.into(),
            ..RecordHeader::new_zeroed()
        };
        header.seal();
        header
    }

    /// Change the header's fields. The returned guard reseals the header
    /// when it is dropped, at the end of the statement for a chain of
    /// setters, and while it lives the borrow checker keeps the header from
    /// being read or written:
    ///
    /// ```ignore
    /// header.edit().meta_capacity(capacity).key_and_metadata(key, internal, headers);
    /// ```
    pub fn edit(&mut self) -> HeaderEdit<'_> {
        HeaderEdit(self)
    }

    /// Check that this is a header of this format and that its fields agree:
    /// the magic, then the version, then `header_crc`, then that the
    /// metadata segments fit the region's capacity. A header read from disk
    /// is trusted only once this passes; until then none of the lengths it
    /// carries is used. A withdrawn header is reported as such only when its
    /// checksum verifies; a torn one is a version mismatch.
    pub fn check(&self) -> Result<(), HeaderError> {
        if self.magic() != MAGIC {
            return Err(HeaderError::BadMagic);
        }
        let version = self.version();
        if version == WITHDRAWN_VERSION && self.compute_header_crc() == self.header_crc() {
            return Err(HeaderError::Withdrawn);
        }
        if version != FORMAT_VERSION {
            return Err(HeaderError::UnsupportedVersion(version));
        }
        if self.compute_header_crc() != self.header_crc() {
            return Err(HeaderError::CrcMismatch);
        }
        let segments = self.internal_len() as u64 + self.header_len() as u64;
        let capacity = self.meta_capacity();
        if segments > capacity as u64 {
            return Err(HeaderError::SegmentsExceedCapacity { segments, capacity });
        }
        Ok(())
    }

    /// CRC-32C over the header's bytes before `header_crc`.
    pub fn compute_header_crc(&self) -> u32 {
        crc32c::crc32c(&self.as_bytes()[..HEADER_CRC_OFFSET])
    }

    /// Recompute and store `header_crc`; called by [`HeaderEdit`]'s drop
    /// and by [`Self::new`].
    fn seal(&mut self) {
        let crc = self.compute_header_crc();
        self.header_crc.set(crc);
    }

    // The fields as native integers. What each one means is documented on
    // the field.

    pub fn magic(&self) -> u32 {
        self.magic.get()
    }

    pub fn version(&self) -> u32 {
        self.version.get()
    }

    pub fn body_len(&self) -> u64 {
        self.body_len.get()
    }

    pub fn meta_capacity(&self) -> u32 {
        self.meta_capacity.get()
    }

    pub fn key_len(&self) -> u32 {
        self.key_len.get()
    }

    pub fn internal_len(&self) -> u32 {
        self.internal_len.get()
    }

    pub fn header_len(&self) -> u32 {
        self.header_len.get()
    }

    pub fn meta_crc(&self) -> u32 {
        self.meta_crc.get()
    }

    // Read only by the read-time body check and the tests.
    #[cfg(any(test, feature = "body-crc-verify"))]
    pub fn body_crc(&self) -> u32 {
        self.body_crc.get()
    }

    pub fn header_crc(&self) -> u32 {
        self.header_crc.get()
    }

    /// Offset of the metadata region: right after the key, unpadded
    /// (SDS-LAYOUT.OBJECTFILE-01, "Segment placement"). Nothing reads the
    /// region in place, so its segments need no alignment.
    pub fn meta_region_offset(&self) -> usize {
        KEY_OFFSET + self.key_len.get() as usize
    }

    /// Offset of the body: right after the metadata region, at its recorded
    /// capacity. A reader computes this from the header alone.
    pub fn body_offset(&self) -> usize {
        self.meta_region_offset() + self.meta_capacity.get() as usize
    }

    /// The full length of a published record: body offset plus body. The
    /// sum saturates rather than overflow, since `body_len` comes from disk:
    /// a saturated length equals no real file's, so a header claiming one is
    /// rejected as a length mismatch.
    pub fn record_len(&self) -> u64 {
        (self.body_offset() as u64).saturating_add(self.body_len.get())
    }
}

/// Why a header block is not a valid header of this format
/// ([`RecordHeader::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u32),
    #[error("withdrawn: its metadata outgrew its region")]
    Withdrawn,
    #[error("header CRC mismatch")]
    CrcMismatch,
    #[error("metadata segments ({segments} bytes) exceed the recorded capacity {capacity}")]
    SegmentsExceedCapacity { segments: u64, capacity: u32 },
}

/// A key or metadata segment longer than the header's 32-bit length fields
/// can record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a {what} of {len} bytes is too long for the record header's 32-bit length fields")]
pub struct TooLong {
    pub what: &'static str,
    pub len: usize,
}

/// The length of a key or metadata segment, as the header records it: at
/// most 4 GiB, which no cache key or HTTP header set comes near. Anything
/// longer fails the store, rather than being cut short.
fn field_len(what: &'static str, len: usize) -> Result<u32, TooLong> {
    u32::try_from(len).map_err(|_| TooLong { what, len })
}

/// The two metadata segments' combined length, from which the metadata
/// region's capacity is computed. A sum past `usize::MAX` saturates, and
/// fails like any other length too long to record.
pub fn segments_len(internal: usize, header: usize) -> Result<u32, TooLong> {
    field_len("metadata", internal.saturating_add(header))
}

/// An edit of a [`RecordHeader`], from [`RecordHeader::edit`]. Its setters
/// change fields that belong together in one call, and dropping it reseals
/// the header, so no edit can leave `header_crc` stale. The one way around
/// it is `std::mem::forget`, which nothing here calls.
#[must_use = "the header is resealed when the edit is dropped; an unused edit changes nothing"]
pub struct HeaderEdit<'a>(&'a mut RecordHeader);

impl HeaderEdit<'_> {
    /// The metadata region's capacity: the segments plus the headroom.
    pub fn meta_capacity(&mut self, capacity: u32) -> &mut Self {
        self.0.meta_capacity.set(capacity);
        self
    }

    /// The key and the two metadata segments: their lengths and
    /// `meta_crc`, the checksum that covers all three, so they can never
    /// disagree. A metadata update passes the record's own key, which leaves
    /// `key_len` as it was. Fails, changing nothing, if any of the three is
    /// too long for its length field.
    pub fn key_and_metadata(
        &mut self,
        key: &[u8],
        internal: &[u8],
        header: &[u8],
    ) -> Result<&mut Self, TooLong> {
        let key_len = field_len("key", key.len())?;
        let internal_len = field_len("internal metadata segment", internal.len())?;
        let header_len = field_len("header metadata segment", header.len())?;
        self.0.key_len.set(key_len);
        self.0.internal_len.set(internal_len);
        self.0.header_len.set(header_len);
        self.0.meta_crc.set(compute_meta_crc(key, internal, header));
        Ok(self)
    }

    /// The body's length and checksum, patched in when a store finishes.
    pub fn body(&mut self, len: u64, crc: u32) -> &mut Self {
        self.0.body_len.set(len);
        self.0.body_crc.set(crc);
        self
    }

    /// Withdraw the record: [`WITHDRAWN_VERSION`] in place of the format
    /// version, every other field kept.
    pub fn withdraw(&mut self) -> &mut Self {
        self.0.version.set(WITHDRAWN_VERSION);
        self
    }
}

impl Drop for HeaderEdit<'_> {
    fn drop(&mut self) {
        self.0.seal();
    }
}

/// CRC-32C over the key segment followed by the two metadata segments: the
/// value `meta_crc` must hold.
pub fn compute_meta_crc(key: &[u8], internal: &[u8], header: &[u8]) -> u32 {
    let crc = crc32c::crc32c(key);
    let crc = crc32c::crc32c_append(crc, internal);
    crc32c::crc32c_append(crc, header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_header_starts_with_the_magic_and_this_version_and_is_sealed() {
        let header = RecordHeader::new();
        assert_eq!(&header.as_bytes()[..4], b"PDCR");
        assert_eq!(header.magic(), MAGIC);
        assert_eq!(header.version(), FORMAT_VERSION);
        assert_eq!(header.header_crc(), header.compute_header_crc());
        assert_eq!(header.check(), Ok(()));
    }

    #[test]
    fn check_rejects_a_foreign_magic_or_version_and_a_stale_crc() {
        let sealed = RecordHeader::new();
        let mut foreign = sealed;
        foreign.magic.set(0);
        assert_eq!(foreign.check(), Err(HeaderError::BadMagic));
        let mut older = sealed;
        older.version.set(FORMAT_VERSION + 1);
        assert_eq!(
            older.check(),
            Err(HeaderError::UnsupportedVersion(FORMAT_VERSION + 1))
        );
        let mut stale = sealed;
        stale.body_len.set(1);
        assert_eq!(stale.check(), Err(HeaderError::CrcMismatch));
        // Sealed, so the CRC passes; the segments outgrow the region.
        let mut overfull = sealed;
        overfull
            .edit()
            .meta_capacity(4)
            .key_and_metadata(b"key", b"intl", b"heads")
            .expect("short segments");
        assert_eq!(
            overfull.check(),
            Err(HeaderError::SegmentsExceedCapacity {
                segments: 9,
                capacity: 4
            })
        );
    }

    /// A withdrawn header checks as withdrawn whatever its lengths say, but
    /// only once sealed: torn, it is a version mismatch.
    #[test]
    fn a_withdrawn_header_checks_as_withdrawn_only_when_sealed() {
        let mut header = RecordHeader::new();
        header
            .edit()
            .meta_capacity(4)
            .key_and_metadata(b"key", b"intl", b"heads")
            .expect("short segments")
            .withdraw();
        assert_eq!(header.version(), WITHDRAWN_VERSION);
        assert_eq!(header.check(), Err(HeaderError::Withdrawn));
        let mut torn = header;
        torn.header_crc.set(header.header_crc() ^ 1);
        assert_eq!(
            torn.check(),
            Err(HeaderError::UnsupportedVersion(WITHDRAWN_VERSION))
        );
    }

    /// A key or metadata segment longer than the header's 32-bit length
    /// fields can record fails, rather than being cut short; the longest
    /// that fits passes. Lengths stand in for slices: allocating 4 GiB to
    /// test this would be absurd.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn a_length_the_header_cannot_record_fails() {
        let max = u32::MAX as usize;
        assert_eq!(field_len("key", max), Ok(u32::MAX));
        assert_eq!(
            field_len("key", max + 1),
            Err(TooLong {
                what: "key",
                len: max + 1
            })
        );
        assert_eq!(segments_len(4, 5), Ok(9));
        assert_eq!(
            segments_len(max, 1),
            Err(TooLong {
                what: "metadata",
                len: max + 1
            })
        );
    }

    #[test]
    fn an_edit_reseals_the_header_when_it_ends() {
        let mut header = RecordHeader::new();
        let before = header.header_crc();
        header
            .edit()
            .meta_capacity(512)
            .key_and_metadata(b"key", b"intl", b"heads")
            .expect("short segments")
            .body(10, 0xABCD);
        assert_ne!(header.header_crc(), before);
        assert_eq!(header.header_crc(), header.compute_header_crc());
        assert_eq!(header.key_len(), 3);
        assert_eq!(header.internal_len(), 4);
        assert_eq!(header.header_len(), 5);
        assert_eq!(
            header.meta_crc(),
            compute_meta_crc(b"key", b"intl", b"heads")
        );
        assert_eq!((header.body_len(), header.body_crc()), (10, 0xABCD));
    }
}
