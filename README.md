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

Brave and DuckDuckGo page fetches are spaced at least three seconds apart per
engine, shared across queries and text/image searches within one process.
Cache hits make no upstream requests. Set `ENGINE_REQUEST_INTERVAL_SECS` to
change the spacing (`0` disables pacing). Queue time does not consume
`ENGINE_TIMEOUT_SECS`, which applies only to the upstream fetch.
When proxy racing is enabled, one fetch can still try multiple routes.

Failures trigger exponential cooldowns (`ENGINE_COOLDOWN_BASE_SECS`, default
60; `ENGINE_COOLDOWN_MAX_SECS`, default 1800). Pagination-session expiry does
not disable an engine globally. DuckDuckGo continuation forms remain in
memory for up to one hour; restarting the server loses those sessions.

Pacing reduces request bursts, but cannot guarantee that upstream services
will accept scraping traffic. Existing cached results and exhaustion flags
remain until cache expiry (`CACHE_MAX_AGE_SECS`, default 86400).
