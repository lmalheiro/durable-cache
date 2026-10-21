// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

/// Configuration for [`crate::FsCache`].
#[derive(Debug, Clone)]
pub struct FsCacheConfig {
    root: PathBuf,
    meta_headroom: u32,
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
            #[cfg(feature = "body-crc-verify")]
            body_crc_verification: false,
        }
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
    fn capacity_is_segments_plus_headroom() {
        let c = FsCacheConfig::new("/x", None);
        assert_eq!(c.meta_capacity_for(0), 512);
        assert_eq!(c.meta_capacity_for(513), 1025);
        let c = FsCacheConfig::new("/x", Some(0));
        assert_eq!(c.meta_capacity_for(513), 513);
    }
}
