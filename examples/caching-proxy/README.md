# caching-proxy

A reverse proxy that runs on the Pingora® framework and caches its
upstream's responses on disk with [durable-cache](../../README.md). It is an
example, meant to be read and copied ([`src/main.rs`](src/main.rs)), and is
not published.

## What it does

- It forwards every request to one upstream, over plain HTTP, asking the
  origin for its own host name.
- It caches the responses to `GET` and `HEAD` that the origin marks
  cacheable with `Cache-Control` or `Expires`, for as long as the origin
  says. A response with `Vary` is stored once per variant, and one with
  `Vary: *` not at all.
- It marks every response with `x-cache-status`: `miss`, `hit`, `stale`,
  `expired`, `revalidated`, or `bypass` when the cache was not used.
- Its cache outlives the process. Once the cache takes 1 GiB on disk
  (`CACHE_CAPACITY`), the least recently used objects are evicted.
- It supports hot upgrades, through `durable_cache::hotupgrade::install`.

## Running it

The workspace's mock origin serves `/hello` with `Cache-Control: max-age=60`
and prints its address:

```bash
cargo run -p durable-cache-mock-origin
# mock-origin listening on http://127.0.0.1:41281
```

In another terminal, point the proxy at it:

```bash
cargo run -p caching-proxy -- --upstream 127.0.0.1:41281 \
    --cache-dir /tmp/caching-proxy
```

The proxy listens on `127.0.0.1:6188` (`--listen` changes it). The first
request goes to the origin; the second is answered from the cache:

```bash
curl -si http://127.0.0.1:6188/hello | grep -i x-cache-status   # miss
curl -si http://127.0.0.1:6188/hello | grep -i x-cache-status   # hit
```

Stop the proxy with Ctrl-C and start it again: while the object is fresh,
the same request is still a `hit`, read from the cache directory.

## Hot upgrade

A new process can take the listening socket over from the running one
without refusing a connection. Start it with `-u` and the same arguments:

```bash
cargo run -p caching-proxy -- -u --upstream 127.0.0.1:41281 \
    --cache-dir /tmp/caching-proxy
```

It waits for the hand-over, logging `No incoming socket transfer, sleep 1s
and try again` until then. Send the old process `SIGQUIT`:

```bash
kill -QUIT <ID of the old process>
```

The old process hands the socket over and stops writing to the cache. It
exits a fixed 5 s plus the grace period later, 300 s by default
(`grace_period_seconds`, which a configuration file given with `-c` can
set). The new process serves from the cache directory at once. The two
processes meet at `/tmp/pingora_upgrade.sock`, the default `upgrade_sock`.

## Testing

```bash
cargo test -p caching-proxy
```

runs the proxy in front of the mock origin, requests a URL twice, and checks
that the second response is a `hit` and that the origin was asked once.

## Limits

- Plain HTTP to the upstream. For HTTPS, enable a TLS feature of
  `pingora-core` and `pingora-proxy`, such as `openssl`, and give
  `HttpPeer::new` `true` and the server name.
- One upstream, so the cache key is the path and query alone.
- No way to purge an object; [`test/fixture-proxy`](../../test/fixture-proxy)
  shows one, with `ProxyHttp::is_purge`.

Pingora is a trademark or registered trademark of Cloudflare, Inc.
