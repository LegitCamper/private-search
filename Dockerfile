FROM rust:alpine AS builder

WORKDIR /workspace

COPY . .

RUN apk add build-base pkgconfig openssl-libs-static openssl openssl-dev sqlite sqlite-libs sqlite-dev

RUN cargo install --path .

FROM alpine as runtime

WORKDIR /app

RUN adduser -D -u 1000 appuser \
 && mkdir /cache \
 && chown -R appuser:appuser /cache /app

COPY --from=builder /workspace/target/release/private-search .

COPY ./static ./static
COPY ./templates ./templates

EXPOSE 8080
USER appuser

ENV ROCKET_ADDRESS=0.0.0.0
ENV ROCKET_PORT=8080
ENV CACHE_DB_PATH=/cache/cache.db

CMD [ "./private-search" ]
