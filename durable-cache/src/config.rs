// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

/// What a store does to make itself durable before it is published
/// (ADR-PERSISTENCE.DURABILITY-01). Invalidating purges are synchronised
/// whichever is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// Each store's temporary is synchronised to stable storage before it
    /// is renamed into place. A host crash or a power loss can lose a
    /// store, but never leave a published record over data that is not on
    /// stable storage (HLR-PERSISTENCE.CRASHSAFE-01). The default.
    #[default]
    Strict,
    /// Stores are renamed into place without being synchronised first, so
    /// finishing one never waits for the device. A process crash or an
    /// orderly restart still loses nothing, as the kernel keeps the data
    /// (HLR-PERSISTENCE.DURABLE-01). A host crash or a power loss may lose
    /// the stores the kernel had not written back yet, and may leave a
    /// published record whose body never reached the device: short, which
    /// a lookup reports as a miss, or zero-filled under a valid header,
    /// which a hit would serve. HLR-PERSISTENCE.CRASHSAFE-01 does not hold.
    NoStoreSync,
}

/// Configuration for [`crate::FsCache`].
#[derive(Debug, Clone)]
pub struct FsCacheConfig {
    root: PathBuf,
    meta_headroom: u32,
    durability: Durability,
    noatime: bool,
    #[cfg(feature = "body-crc-verify")]
    body_crc_verification: bool,
}

impl FsCacheConfig {
    /// The metadata headroom the design specifies as the default, in bytes
    /// (FDS-STORAGE.METAUPDATE-01).
    const DEFAULT_META_HEADROOM: u32 = 512;

    /// A configuration for a cache rooted at `root` that reserves
    /// `meta_headroom` bytes of growth room in each record's metadata
    /// region. Passing `None` selects the design's default, 512 bytes.
    pub fn new(root: impl Into<PathBuf>, meta_headroom: Option<u32>) -> Self {
        let meta_headroom = meta_headroom.unwrap_or(FsCacheConfig::DEFAULT_META_HEADROOM);
        FsCacheConfig {
            root: root.into(),
            meta_headroom,
            durability: Durability::default(),
            noatime: false,
            #[cfg(feature = "body-crc-verify")]
            body_crc_verification: false,
        }
    }

    /// Choose what a store does to make itself durable before it is
    /// published; [`Durability::Strict`] unless chosen otherwise.
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// Open records to read them without updating their access time
    /// (`O_NOATIME`, Linux only); off unless switched on. Under the default
    /// `relatime`, the first read of a record after a store wrote it updates
    /// its access time, and on ext4 that starts a journal transaction, which
    /// can wait: even for a hit served on the async worker. Nothing in this
    /// crate reads access times, and a file system mounted `noatime` or
    /// `lazytime` needs no option.
    ///
    /// The kernel allows `O_NOATIME` only to the owner of a file. With the
    /// option on, [`crate::FsCache::new`] fails if this process does not own
    /// the cache root, or on a platform without the flag, and a read of a
    /// record that another user owns fails.
    pub fn with_noatime(mut self, enabled: bool) -> Self {
        self.noatime = enabled;
        self
    }

    /// Switch the read-time body check on or off; it is off unless switched
    /// on. When on, a read that delivers a whole body from its start
    /// compares it with the CRC-32C stored with it, and on a mismatch logs
    /// an error naming the file; the response still ends normally, since
    /// every byte has already been delivered (ADR-PERSISTENCE.DURABILITY-01).
    #[cfg(feature = "body-crc-verify")]
    pub fn with_body_crc_verification(mut self, enabled: bool) -> Self {
        self.body_crc_verification = enabled;
        self
    }

    /// Root of the sharded object-file tree (SDS-LAYOUT.SHARDING-01).
    #[inline]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Growth headroom reserved in the metadata region, in bytes
    /// (FDS-STORAGE.METAUPDATE-01). Zero makes any metadata change that grows
    /// the metadata withdraw the record, to be fetched again.
    #[inline]
    pub fn meta_headroom(&self) -> u32 {
        self.meta_headroom
    }

    /// What a store does to make itself durable before it is published; see
    /// [`Self::with_durability`].
    #[inline]
    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Whether records are opened without updating their access time; see
    /// [`Self::with_noatime`].
    #[inline]
    pub fn noatime(&self) -> bool {
        self.noatime
    }

    /// Whether reads check the body against its stored CRC-32C; see
    /// [`Self::with_body_crc_verification`].
    #[cfg(feature = "body-crc-verify")]
    #[inline]
    pub fn body_crc_verification(&self) -> bool {
        self.body_crc_verification
    }

    /// The metadata region's capacity for segments of `segments_len` bytes:
    /// the segments plus the headroom (SDS-LAYOUT.OBJECTFILE-01).
    #[inline]
    pub(crate) fn meta_capacity_for(&self, segments_len: u32) -> u32 {
        segments_len.saturating_add(self.meta_headroom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_opened_as_usual_unless_noatime_is_switched_on() {
        let config = FsCacheConfig::new("/x", None);
        assert!(!config.noatime());
        assert!(config.with_noatime(true).noatime());
    }

    #[test]
    fn capacity_is_segments_plus_headroom() {
        let c = FsCacheConfig::new("/x", None);
        assert_eq!(c.meta_capacity_for(0), 512);
        assert_eq!(c.meta_capacity_for(513), 1025);
        let c = FsCacheConfig::new("/x", Some(0));
        assert_eq!(c.meta_capacity_for(513), 513);
    }
}
