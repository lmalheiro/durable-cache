// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;
use std::time::Duration;

/// One scripted response: a status, a header set, and a body delivered as
/// a sequence of chunks, each optionally delayed — enough to drive
/// TC-COMPAT.UNSIZED-01 (irregular chunk sizes, no declared length),
/// TC-COMPAT.STREAMING-01 (throttled delivery so a follower's promptness
/// is observable), and ordinary fixed responses (headers/freshness/etc.
/// tests) alike.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Chunk>,
    /// When `false`, no `Content-Length` is sent and the connection close
    /// (or chunked transfer-encoding, per the HTTP version negotiated)
    /// signals the end — the unsized-body case FRE-COMPAT.UNSIZED-01 needs.
    pub declare_length: bool,
    /// When set, a conditional request whose `If-None-Match` carries this
    /// entity tag is answered `304 (Not Modified)` with
    /// [`Self::revalidated_headers`] and no body, so a cache can freshen a
    /// stored response instead of replacing it
    /// (FRE-COMPLIANCE.REVALIDATION-01).
    pub etag: Option<String>,
    /// The header set a `304` carries, merged into the stored response by
    /// `pingora-cache`.
    pub revalidated_headers: Vec<(String, String)>,
}

/// A 200 with no headers and no body: the base a test fills in, so adding
/// a field here does not break every scenario literal.
impl Default for Scenario {
    fn default() -> Self {
        Scenario {
            status: 200,
            headers: Vec::new(),
            chunks: Vec::new(),
            declare_length: true,
            etag: None,
            revalidated_headers: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub data: Bytes,
    /// Delay *before* this chunk is written, letting a test pause a body
    /// mid-stream (e.g. to attach a follower, or to hold a store open).
    pub delay_before: Duration,
}

impl Scenario {
    pub fn fixed(status: u16, headers: Vec<(String, String)>, body: impl Into<Bytes>) -> Self {
        Scenario {
            status,
            headers,
            chunks: vec![Chunk {
                data: body.into(),
                delay_before: Duration::ZERO,
            }],
            declare_length: true,
            etag: None,
            revalidated_headers: Vec::new(),
        }
    }

    /// The same response, tagged so that a later conditional request is
    /// answered `304` with `revalidated_headers`.
    pub fn revalidatable(
        mut self,
        etag: impl Into<String>,
        revalidated_headers: Vec<(String, String)>,
    ) -> Self {
        let etag = etag.into();
        self.headers.push(("etag".into(), etag.clone()));
        self.etag = Some(etag);
        self.revalidated_headers = revalidated_headers;
        self
    }

    pub fn total_len(&self) -> usize {
        self.chunks.iter().map(|c| c.data.len()).sum()
    }
}
