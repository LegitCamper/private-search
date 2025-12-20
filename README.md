# Private Search

A self-hostable private search.

No tracking. FAST AF (I think). Run it yourself.


Search requests and caching are handled by the backend library:

- **https://github.com/legitcamper/private-search-engines**

## Quick Start (Docker)

You can deploy your own instance using Docker with a single command:

```bash
docker run -d \
  -p 8080:8080 \
  -v private-search:/app/data \
  ghcr.io/legitcamper/private-search
```

http://localhost:8080
