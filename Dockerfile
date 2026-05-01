FROM docker.io/rust:1-slim-bookworm AS builder

WORKDIR /workspace

COPY . .

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    libsqlite3-dev \
    lld \
    curl \
    ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN curl -L --proto '=https' --tlsv1.2 -sSf \
    https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash; \
    cargo binstall cargo-sonic

RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    set -eux; \
    cargo sonic \
        --target-cpus=x86-64-v3,skylake \
        --compress=zstd \
        --compression-level=10 \
        --parallelism=2 \
        --loader=embedded \
        build --release; \
    objcopy --compress-debug-sections target/sonic/x86_64-unknown-linux-gnu/release/private-search ./main


FROM docker.io/rust:1-slim-bookworm AS runtime

WORKDIR /app

COPY --from=builder /workspace/main ./

COPY ./static ./static
COPY ./templates ./templates

EXPOSE 8080

ENV ROCKET_ADDRESS=0.0.0.0
ENV ROCKET_PORT=8080

CMD ["./main"]
