FROM rust:1.96-slim-trixie AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates pkg-config perl make cmake clang mold \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=mold"
WORKDIR /src
COPY . ./
RUN rm -f .cargo/config.toml
RUN cargo build --release --bin hydrant
RUN strip target/release/hydrant

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/hydrant /usr/local/bin/hydrant
ENV HYDRANT_DATABASE_PATH=/var/lib/hydrant
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/hydrant"]
