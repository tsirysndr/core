FROM rust:1.96-slim-trixie AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates pkg-config perl make cmake clang mold \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=mold"
ARG BOBBIN_PROFILE=release
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY lexicons ./lexicons
COPY bobbin ./bobbin
COPY shuttle ./shuttle
RUN cargo build --profile ${BOBBIN_PROFILE} --bin bobbin --package bobbin
RUN if [ "${BOBBIN_PROFILE}" = "release" ]; then strip target/${BOBBIN_PROFILE}/bobbin; fi

FROM debian:trixie-slim
ARG BOBBIN_PROFILE=release
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/${BOBBIN_PROFILE}/bobbin /usr/local/bin/bobbin
ENV BOBBIN_BIND=0.0.0.0:8090
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/bobbin"]
