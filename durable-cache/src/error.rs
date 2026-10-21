// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The two error types a caller can tell apart, and the operations whose
//! refusals and failures the messages name.
//!
//! The crate has no error type of its own: every fallible function returns
//! `pingora_error::Result`, as the Pingora crates themselves do. Where a
//! lower-level error becomes a failure, `pingora_error::OrErr` gives it one
//! of the predefined Pingora error types (`File*Error`, `InternalError`)
//! and a context saying what was being done, and keeps the lower-level
//! error as the cause.

use std::fmt;

use pingora_error::{Error, ErrorType, Result};

/// `update_meta` addressed to a key with no stored object: the
/// distinguishable error FRE-COMPLIANCE.METAABSENT-01 requires.
pub(crate) const KEY_NOT_FOUND: ErrorType = ErrorType::Custom("key_not_found");

/// A change refused because the backend is read-only: the process is the
/// outgoing side of a hot upgrade, and a purge client retries the change
/// against the incoming process (FRE-COMPAT.HOTUPGRADE-01).
pub(crate) const CACHE_READ_ONLY: ErrorType = ErrorType::Custom("cache_read_only");

/// A store's `finish` when an invalidating purge of its key ended the store
/// while it was in flight: the store published nothing
/// (FRE-COMPLIANCE.PURGE-01).
pub(crate) const STORE_PURGED: ErrorType = ErrorType::Custom("store_purged");

/// The operations that change the shared cache on a caller's request, named
/// in one place for the messages about them: a read-only refusal, a
/// missing object, a withdrawn record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Purge,
    Expire,
    UpdateMeta,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Op::Purge => "purge",
            Op::Expire => "expire",
            Op::UpdateMeta => "update_meta",
        })
    }
}

/// Refuse `op` because the backend is read-only: a change this process will
/// not make durable fails rather than being reported as applied.
pub(crate) fn refuse_read_only<T>(op: Op) -> Result<T> {
    Error::e_explain(
        CACHE_READ_ONLY,
        format!("{op} refused: the cache is read-only in the outgoing process of a hot upgrade"),
    )
}
