// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The read-time body check (ADR-PERSISTENCE.DURABILITY-02), with the
//! `body-crc-verify` feature: a hit that delivers a whole body from its
//! start compares it with the CRC-32C stored with it, and logs an error on
//! a mismatch. Without the feature, [`BodyCheck`] is empty and checks
//! nothing, as `crate::metrics` counts nothing without its own, so that a
//! hit's code reads the same in every build.

#[cfg(feature = "body-crc-verify")]
mod imp {
    use std::path::Path;

    use crate::FsCache;

    /// The body check of a hit, while one runs: switched on in the
    /// configuration, for a published record that carries a checksum, until
    /// the read seeks.
    pub struct BodyCheck(Option<Running>);

    /// A check in progress.
    struct Running {
        /// CRC-32C over the bytes delivered so far.
        running: u32,
        /// The record's `body_crc`.
        stored: u32,
    }

    impl BodyCheck {
        /// A check of a record whose header stores `stored`, if `cache` has
        /// checking switched on. A stored zero is not checked: it is what a
        /// build without the `body-crc` feature records, and the checksum of
        /// an empty body.
        pub fn start(cache: &FsCache, stored: u32) -> Self {
            BodyCheck(
                (cache.config.body_crc_verification() && stored != 0)
                    .then_some(Running { running: 0, stored }),
            )
        }

        /// No check: for a partial hit, as a store in flight has no
        /// checksum yet.
        pub fn none() -> Self {
            BodyCheck(None)
        }

        /// Take in the next bytes delivered.
        pub fn update(&mut self, bytes: &[u8]) {
            if let Some(check) = &mut self.0 {
                check.running = crc32c::crc32c_append(check.running, bytes);
            }
        }

        /// The whole body of the file at `path` has been delivered: log a
        /// mismatch, and end the check. The response still ends normally:
        /// its bytes are already on their way to the client, and an error
        /// now could only cut the connection.
        pub fn finish(&mut self, path: &Path) {
            let Some(check) = self.0.take() else {
                return;
            };
            if check.running != check.stored {
                log::error!(
                    "the body of {} does not match its stored checksum \
                     (stored {:#010x}, computed {:#010x}); it was served as read",
                    path.display(),
                    check.stored,
                    check.running
                );
            }
        }

        /// The read sought: end the check, as the stored checksum covers a
        /// whole body only.
        pub fn abandon(&mut self) {
            self.0 = None;
        }
    }
}

#[cfg(not(feature = "body-crc-verify"))]
mod imp {
    use std::path::Path;

    use crate::FsCache;

    /// No body check: this build lacks the `body-crc-verify` feature.
    pub struct BodyCheck;

    impl BodyCheck {
        pub fn start(_cache: &FsCache, _stored: u32) -> Self {
            BodyCheck
        }

        pub fn none() -> Self {
            BodyCheck
        }

        pub fn update(&mut self, _bytes: &[u8]) {}

        pub fn finish(&mut self, _path: &Path) {}

        pub fn abandon(&mut self) {}
    }
}

pub(super) use imp::BodyCheck;
