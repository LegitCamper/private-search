FROM rust:alpine AS builder

WORKDIR /workspace

COPY . .

RUN apk add build-base pkgconfig openssl-libs-static openssl openssl-dev sqlite sqlite-libs sqlite-dev

RUN cargo install --path .

FROM alpine as runtime

WORKDIR /app

RUN adduser -D -u 1000 appuser

COPY --from=builder /workspace/target/release/private-search .

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
