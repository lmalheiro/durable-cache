// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The stand-in for `uring` where stores cannot write through an `io_uring`
//! ring: elsewhere than on Linux, and on Linux on the architectures the
//! `io-uring` crate has no bindings for (see `build.rs`). No option asks for
//! a ring there, so a backend has none, and every store writes as without
//! one. `lib.rs` declares it as `uring`, so that the code that writes on the
//! ring reads the same on every platform.

use std::fs::File;
use std::io;
use std::sync::Arc;

use bytes::Bytes;

use crate::hooks::Hooks;
use crate::FsCacheConfig;

/// No ring: the type has no value, so a backend's ring is always `None`.
pub(crate) enum Ring {}

impl Ring {
    /// No ring: no option asks for one.
    pub(crate) fn new(
        _config: &FsCacheConfig,
        _hooks: &Hooks,
    ) -> pingora_error::Result<Option<Self>> {
        Ok(None)
    }

    /// Never called: there is no ring to write on.
    pub(crate) async fn write_all_at(
        &self,
        _file: Arc<File>,
        _data: Bytes,
        _offset: u64,
    ) -> io::Result<()> {
        match *self {}
    }
}
