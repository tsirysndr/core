FROM docker.io/library/rust:1-alpine3.23 AS builder
RUN apk add --no-cache build-base musl-dev cmake perl pkgconfig
WORKDIR /src
COPY . ./
RUN rm -f .cargo/config.toml
RUN cargo build --release --bin slingshot --package slingshot
RUN strip target/release/slingshot

FROM docker.io/library/alpine:3.23
RUN apk add --no-cache ca-certificates
WORKDIR /app
COPY --from=builder /src/target/release/slingshot /usr/local/bin/slingshot
COPY --from=builder /src/slingshot/static /app/static
ENV SLINGSHOT_CACHE_DIR=/var/lib/slingshot
ENV SLINGSHOT_BIND=[::]:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/slingshot"]
