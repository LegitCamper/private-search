# Private Search

A self-hostable private search.

No tracking. FAST AF (I think). Run it yourself.

## Quick Start (Docker)

You can deploy your own instance using Docker with a single command:

```bash
docker run -d \
  -p 8080:8080 \
  -v private-search:/app/data \
  ghcr.io/legitcamper/private-search
```

http://localhost:8080

## Upstream Request Limits

Brave, DuckDuckGo and Wiby page fetches are spaced at least three seconds apart per
engine, shared across queries and text/image searches within one process.
Cache hits make no upstream requests. Set `ENGINE_REQUEST_INTERVAL_SECS` to
change the spacing (`0` disables pacing). Queue time does not consume
`ENGINE_TIMEOUT_SECS`, which applies only to the upstream fetch.
When proxy racing is enabled, one fetch can still try multiple routes
(`ENGINE_PROXY_RACERS`, default 1, max 3; `ENGINE_PROXY_WAVES`, default 1,
max 2) — the defaults keep amplification at one upstream request per fetch,
plus the existing direct fallback.

Failures trigger exponential cooldowns (`ENGINE_COOLDOWN_BASE_SECS`, default
60; `ENGINE_COOLDOWN_MAX_SECS`, default 1800). Pagination-session expiry does
not disable an engine globally. DuckDuckGo continuation forms remain in
memory for up to one hour; restarting the server loses those sessions.

Pacing reduces request bursts, but cannot guarantee that upstream services
will accept scraping traffic. Existing cached results and exhaustion flags
remain until cache expiry (`CACHE_MAX_AGE_SECS`, default 86400).

Pacing waits are bounded: an engine already queued behind another waiter is
reported as busy (retry in 5s) rather than holding the request open, and a
cooldown that starts while waiting cancels the fetch before it goes out.

## Engines

General web results come from Brave, DuckDuckGo, Wikipedia and Wiby. Wiby
indexes the small, lightweight web — it adds an independent source that is
rarely blocked, but it is not a breadth replacement for a major index, so
expect fewer and more niche hits from it. Programming queries additionally
fan out across the site-specific adapters (Stack Exchange, GitHub, GitLab,
crates.io, and so on). There is no Google adapter.

Blocked engines surface in the per-engine status panel with a retry
countdown, and the search page keeps a **More results** and **Restart
search** control so a stalled or expired search can be resumed without a
reload. Image results load on click, so no third-party image is fetched by
the browser until asked for.

## Reverse Proxies

Rate limiting keys off the actual socket peer. If you terminate TLS in front
of this app, list the proxy addresses in `TRUSTED_PROXY_IPS`
(comma-separated IPs) so forwarded client-IP headers are honoured from those
peers only; every other peer's headers are ignored. An unparseable value
fails startup. Your proxy must *overwrite* (not append to) the client-IP
header, or a client can still choose its own rate-limit bucket.

## Privacy Caveats

The app's own logs and error messages don't include query text. Rocket's
access log and any reverse-proxy log still record the request URI, which
contains the query — turn those off or scrub them if that matters.

## Upgrading

The cache schema is version 3. A version mismatch drops and recreates the
cache database: cached results disappear and order tokens issued by the old
build stop working, so in-flight searches must be restarted. Static assets
are versioned by package version and revalidate on each release.
