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
