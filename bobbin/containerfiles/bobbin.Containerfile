FROM docker.io/library/rust:1-alpine3.23 AS builder
RUN apk add --no-cache build-base musl-dev cmake perl pkgconfig
ARG BOBBIN_PROFILE=release
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY lexicons ./lexicons
COPY bobbin ./bobbin
RUN cargo build --profile ${BOBBIN_PROFILE} --bin bobbin --package bobbin
RUN if [ "${BOBBIN_PROFILE}" = "release" ]; then strip target/${BOBBIN_PROFILE}/bobbin; fi

FROM docker.io/library/alpine:3.23
ARG BOBBIN_PROFILE=release
RUN apk add --no-cache ca-certificates
COPY --from=builder /src/target/${BOBBIN_PROFILE}/bobbin /usr/local/bin/bobbin
ENV BOBBIN_BIND=0.0.0.0:8090
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/bobbin"]
