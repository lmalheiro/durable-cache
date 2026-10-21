// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Disposable proxy fixture (Appendix B.2's Docker-harness pattern:
//! "origin fixed, proxy configuration the only variable" — the same
//! `fixture-proxy` binary points at either cache-tests' origin or
//! `mock-origin` via `--upstream-addr`).
//!
//! Wires `pingora_cache::HttpCache` onto `FsCache` (`durable-cache`)
//! plus a stock eviction manager, following the minimal filter set
//! `ProxyHttp`'s own doc comments call for: `request_cache_filter` enables
//! the cache, `cache_key_callback` derives a key from the request (host +
//! path + query — good enough for a single-upstream fixture; a real
//! integrator with virtual hosting would need more), `response_cache_filter`
//! defers entirely to the origin's own `Cache-Control`/`Expires` (no
//! implicit default freshness: this is a compliance fixture, not a
//! production default), and `cache_vary_filter` folds the response's
//! `Vary`-named request fields into the variance key via `VarianceBuilder`,
//! exactly as FRE-COMPLIANCE.VARY-01 describes. `is_purge` recognizes the
//! `PURGE` method; the framework's own `proxy_purge` handles the rest
//! (`HttpCache::purge`, the canned 200/404/405 response) — no separate
//! purge module needed on this side, unlike the plan originally assumed.

use async_trait::async_trait;
use pingora_cache::eviction::simple_lru::Manager as EvictionManager;
use pingora_cache::filters::{request_cacheable, resp_cacheable};
use pingora_cache::key::HashBinary;
use pingora_cache::{
    cache_control::CacheControl, CacheKey, CacheMetaDefaults, NoCacheReason, RespCacheable,
};
use pingora_core::prelude::HttpPeer;
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

/// No implicit freshness for any status: a response is cacheable here only
/// when the origin says so explicitly via `Cache-Control`/`Expires`. Right
/// for a compliance fixture, where caching decisions must trace to the
/// origin's own headers, never to a default this proxy invented.
const CACHE_DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(|_| None, 0, 0);

pub struct FixtureProxy {
    pub upstream_addr: String,
    pub upstream_tls: bool,
    pub upstream_sni: String,
    /// The storage backend under test: this crate's `FsCache`, or the stock
    /// `MemCache` that TC-COMPLIANCE.RFC-01 compares it against.
    pub cache: &'static (dyn pingora_cache::storage::Storage + Sync),
    pub eviction: &'static EvictionManager,
}

/// Whether any occurrence of the (possibly repeated, possibly
/// comma-list-valued) `Vary` header contains a bare `*` field -- checked
/// across every header instance and every comma-separated segment within
/// each, so `Vary: *`, `Vary: foo, *`, and two separate `Vary: *` lines
/// are all recognised alike.
fn has_vary_star(resp: &ResponseHeader) -> bool {
    resp.headers.get_all(http::header::VARY).iter().any(|v| {
        v.to_str()
            .map(|s| s.split(',').any(|field| field.trim() == "*"))
            .unwrap_or(false)
    })
}

fn is_purge_request(req: &RequestHeader) -> bool {
    req.method.as_str() == "PURGE"
}

fn cache_key_from_request(req: &RequestHeader) -> CacheKey {
    let host = req
        .headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let primary = format!("{host}{}", req.uri);
    // `CacheKey::new` hashes the primary bytes exactly as given; a caller
    // combining several logical components must frame them unambiguously.
    CacheKey::new(primary, "")
}

#[async_trait]
impl ProxyHttp for FixtureProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let peer = HttpPeer::new(
            self.upstream_addr.as_str(),
            self.upstream_tls,
            self.upstream_sni.clone(),
        );
        Ok(Box::new(peer))
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        // A PURGE needs the cache enabled too: `proxy_purge` only calls
        // `HttpCache::purge` when `session.cache.enabled()`, and answers a
        // canned 405 otherwise. `request_cacheable` admits only GET/HEAD,
        // so PURGE is enabled explicitly -- with the same key a GET for the
        // URL would get, which is exactly the object to purge.
        let req = session.req_header();
        if request_cacheable(req) || is_purge_request(req) {
            session
                .cache
                .enable(self.cache, Some(self.eviction), None, None, None);
        }
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        Ok(cache_key_from_request(session.req_header()))
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        // RFC 9110 12.5.5: `Vary: *` means a stored response can never be
        // reused for a later request (every request is potentially
        // unique), so the response is uncacheable outright -- distinct
        // from an ordinary named `Vary` field, which `cache_vary_filter`
        // below still folds into a variance key for a response this
        // filter does decide to cache. `resp_cacheable` has no opinion on
        // `Vary` at all, so this has to be decided here first.
        if has_vary_star(resp) {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom(
                "vary-star",
            )));
        }

        let cc = CacheControl::from_resp_headers(resp);
        let authorization_present = session
            .req_header()
            .headers
            .contains_key(http::header::AUTHORIZATION);
        Ok(resp_cacheable(
            cc.as_ref(),
            resp.clone(),
            authorization_present,
            &CACHE_DEFAULTS,
        ))
    }

    fn cache_vary_filter(
        &self,
        meta: &pingora_cache::CacheMeta,
        _ctx: &mut Self::CTX,
        req: &RequestHeader,
    ) -> Option<HashBinary> {
        let vary_value = meta.response_header().headers.get(http::header::VARY)?;
        let vary_value = vary_value.to_str().ok()?;

        let mut builder = pingora_cache::VarianceBuilder::new();
        for field_name in vary_value.split(',').map(|s| s.trim()) {
            if field_name.is_empty() || field_name == "*" {
                continue;
            }
            let value = req
                .headers
                .get(field_name)
                .map(|v| v.as_bytes())
                .unwrap_or(b"");
            builder.add_value(field_name, value);
        }
        builder.finalize()
    }

    fn is_purge(&self, session: &Session, _ctx: &Self::CTX) -> bool {
        is_purge_request(session.req_header())
    }
}
