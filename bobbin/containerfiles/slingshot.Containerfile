FROM rust:1.96-slim-trixie AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates pkg-config perl make cmake clang mold \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=mold"
WORKDIR /src
COPY . ./
RUN rm -f .cargo/config.toml
RUN cargo build --release --bin slingshot --package slingshot
RUN strip target/release/slingshot

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /src/target/release/slingshot /usr/local/bin/slingshot
COPY --from=builder /src/slingshot/static /app/static
ENV SLINGSHOT_CACHE_DIR=/var/lib/slingshot
ENV SLINGSHOT_BIND=[::]:8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/slingshot"]
