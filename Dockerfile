FROM rust:1-bookworm AS builder

WORKDIR /workspace

COPY . .

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    libsqlite3-dev \
    lld \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-sonic --locked

RUN cargo sonic \
    --target-cpus=x86-64-v3,skylake \
    --compress=zstd \
    --compression-level=10 \
    -p 2 \
    --loader=bundle \
    build --release

FROM alpine as runtime

WORKDIR /app

RUN adduser -D -u 1000 appuser

# COPY --from=builder /workspace/target/release/private-search .
COPY --from=builder /workspace/target/sonic/x86_64-unknown-linux-gnu/release/private-search ./
COPY --from=builder /workspace/target/sonic/x86_64-unknown-linux-gnu/release/private-search.bundle ./private-search.bundle

COPY ./static ./static
COPY ./templates ./templates

COPY entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/entrypoint.sh

ENTRYPOINT ["entrypoint.sh"]

USER appuser

EXPOSE 8080

ENV ROCKET_ADDRESS=0.0.0.0
ENV ROCKET_PORT=8080

CMD ["./private-search"]
