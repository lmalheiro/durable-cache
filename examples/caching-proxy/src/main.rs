// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! A reverse proxy that caches its upstream's responses on disk, with
//! `durable-cache` as the storage behind the Pingora HTTP cache.
//!
//! Every request goes to one upstream, over plain HTTP. The response to a
//! `GET` or `HEAD` that the origin marks cacheable, with `Cache-Control` or
//! `Expires`, is stored in the cache directory and served from there while it
//! is fresh; being on disk, it outlives the process. Each response carries
//! `x-cache-status`, which says whether the cache answered it. `README.md`
//! shows how to run it.

use async_trait::async_trait;
use clap::Parser;
use durable_cache::{hotupgrade, FsCache, FsCacheConfig};
use pingora_cache::cache_control::CacheControl;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::filters::{request_cacheable, resp_cacheable};
use pingora_cache::key::HashBinary;
use pingora_cache::{
    CacheKey, CacheMeta, CacheMetaDefaults, CachePhase, NoCacheReason, RespCacheable,
    VarianceBuilder,
};
use pingora_core::prelude::{HttpPeer, Opt, Server};
use pingora_error::Result;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{http_proxy_service, ProxyHttp, Session};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;

/// The size of the cache, in bytes of records on disk. Once the cached
/// objects outgrow it, the least recently used are evicted.
const CACHE_CAPACITY: usize = 1 << 30;

/// No freshness of the proxy's own: a response is cached only when the origin
/// says for how long.
const CACHE_DEFAULTS: CacheMetaDefaults = CacheMetaDefaults::new(|_| None, 0, 0);

#[derive(Parser)]
#[command(about = "A reverse proxy that caches its upstream's responses on disk")]
struct Cli {
    /// The address to accept client connections on.
    #[arg(long, default_value = "127.0.0.1:6188")]
    listen: String,

    /// The origin to forward requests to, as host:port, over plain HTTP.
    #[arg(long)]
    upstream: String,

    /// The cache directory, created if it does not exist.
    #[arg(long)]
    cache_dir: PathBuf,

    /// The Pingora server options: `-u` takes the listening socket over from
    /// a running process (a hot upgrade), `-c` reads a configuration file,
    /// and `-d` runs in the background.
    #[command(flatten)]
    server: Opt,
}

struct CachingProxy {
    /// Where requests go, resolved once at startup.
    upstream: SocketAddr,
    /// The `Host` the origin is asked for: the upstream as given.
    upstream_host: String,
    cache: &'static FsCache,
    eviction: &'static Manager,
}

/// The field names a response's `Vary` lists, across every `Vary` line.
fn vary_fields(resp: &ResponseHeader) -> impl Iterator<Item = &str> {
    resp.headers
        .get_all("vary")
        .into_iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|field| !field.is_empty())
}

#[async_trait]
impl ProxyHttp for CachingProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        Ok(Box::new(HttpPeer::new(self.upstream, false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        // The origin is asked for its own name, not for the proxy's.
        upstream_request.insert_header("host", &self.upstream_host)
    }

    fn request_cache_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<()> {
        // Only `GET` and `HEAD` go through the cache.
        if request_cacheable(session.req_header()) {
            session
                .cache
                .enable(self.cache, Some(self.eviction), None, None, None);
        }
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session, _ctx: &mut Self::CTX) -> Result<CacheKey> {
        // With a single upstream, the path and query name an object.
        Ok(CacheKey::new(session.req_header().uri.to_string(), ""))
    }

    fn response_cache_filter(
        &self,
        session: &Session,
        resp: &ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<RespCacheable> {
        // `Vary: *` makes every request a different one, so a stored copy
        // could never be reused.
        if vary_fields(resp).any(|field| field == "*") {
            return Ok(RespCacheable::Uncacheable(NoCacheReason::Custom("vary: *")));
        }
        let cache_control = CacheControl::from_resp_headers(resp);
        let authorized = session.req_header().headers.contains_key("authorization");
        Ok(resp_cacheable(
            cache_control.as_ref(),
            resp.clone(),
            authorized,
            &CACHE_DEFAULTS,
        ))
    }

    fn cache_vary_filter(
        &self,
        meta: &CacheMeta,
        _ctx: &mut Self::CTX,
        req: &RequestHeader,
    ) -> Option<HashBinary> {
        // A response with `Vary` is stored once for each combination of the
        // values the request has for the fields it names.
        let mut variance = VarianceBuilder::new();
        for field in vary_fields(meta.response_header()) {
            let value = req.headers.get(field).map_or(&b""[..], |v| v.as_bytes());
            variance.add_value(field, value);
        }
        variance.finalize()
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        let status = match session.cache.phase() {
            CachePhase::Hit => "hit",
            CachePhase::Miss => "miss",
            CachePhase::Stale | CachePhase::StaleUpdating => "stale",
            CachePhase::Expired => "expired",
            CachePhase::Revalidated | CachePhase::RevalidatedNoCache(_) => "revalidated",
            // The cache served nothing and stores nothing: not a `GET` or
            // `HEAD`, or a response the origin did not mark cacheable.
            _ => "bypass",
        };
        upstream_response.insert_header("x-cache-status", status)
    }
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    let upstream = cli
        .upstream
        .to_socket_addrs()
        .expect("cannot resolve --upstream")
        .next()
        .expect("--upstream resolves to no address");

    let mut server = Server::new(cli.server).expect("cannot set up the server");
    server.bootstrap();

    // `Storage` methods take `&'static self`, so the cache and its eviction
    // manager live as long as the process.
    let cache = FsCache::new(FsCacheConfig::new(&cli.cache_dir, None))
        .expect("cannot open the cache directory");
    let cache: &'static FsCache = Box::leak(Box::new(cache));
    let eviction: &'static Manager = Box::leak(Box::new(Manager::new(CACHE_CAPACITY)));

    // A background service that rebuilds the eviction manager's view of the
    // cache from disk, and a watcher that makes the cache read-only when
    // this process hands its socket over in a hot upgrade. Required for hot
    // upgrades: without it, two processes write the cache at once.
    hotupgrade::install(&mut server, cache, eviction, None);

    let proxy = CachingProxy {
        upstream,
        upstream_host: cli.upstream,
        cache,
        eviction,
    };
    let mut service = http_proxy_service(&server.configuration, proxy);
    service.add_tcp(&cli.listen);
    server.add_service(service);
    server.run_forever();
}
