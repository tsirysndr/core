FROM docker.io/library/rust:1-alpine3.23 AS builder
RUN apk add --no-cache build-base musl-dev cmake perl pkgconfig
WORKDIR /src
COPY . ./
RUN rm -f .cargo/config.toml
RUN cargo build --release --bin hydrant
RUN strip target/release/hydrant

FROM docker.io/library/alpine:3.23
RUN apk add --no-cache ca-certificates
COPY --from=builder /src/target/release/hydrant /usr/local/bin/hydrant
ENV HYDRANT_DATABASE_PATH=/var/lib/hydrant
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/hydrant"]
