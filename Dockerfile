FROM rust:alpine AS builder

WORKDIR /workspace

COPY . .

RUN apk add build-base pkgconfig openssl-libs-static openssl openssl-dev sqlite sqlite-libs sqlite-dev

RUN cargo install --path .

FROM alpine as runtime

WORKDIR /app

COPY --from=builder /workspace/target/release/private-search .

COPY ./static ./static
COPY ./templates ./templates

RUN adduser -D appuser
USER appuser

EXPOSE 8080

ENV ROCKET_ADDRESS=0.0.0.0
ENV ROCKET_PORT=8080

CMD [ "./private-search" ]
