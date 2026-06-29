# Development only. Not for production use.

FROM golang:1.25-alpine AS builder

RUN apk add --no-cache git build-base sqlite-dev

ENV CGO_ENABLED=1
ENV GOCACHE=/go/cache
ENV GOMODCACHE=/go/mod

WORKDIR /src

COPY go.mod go.sum ./
RUN --mount=type=cache,target=/go/cache \
    --mount=type=cache,target=/go/mod \
    go mod download

COPY . .
RUN --mount=type=cache,target=/go/cache \
    --mount=type=cache,target=/go/mod \
    go build -tags libsqlite3 -o /out/spindle ./cmd/spindle && \
    go build -tags libsqlite3 -o /out/spindle-microvm-run ./cmd/spindle-microvm-run

FROM alpine:3.24

RUN apk add --no-cache \
    bash \
    ca-certificates \
    e2fsprogs \
    git \
    iproute2 \
    qemu-system-x86_64 \
    shadow \
    slirp4netns \
    sqlite-libs \
    tini \
    util-linux


COPY --from=builder /out/spindle /usr/local/bin/spindle
COPY --from=builder /out/spindle-microvm-run /usr/local/bin/spindle-microvm-run
RUN chmod 0755 /usr/local/bin/spindle /usr/local/bin/spindle-microvm-run

COPY <<'EOF' /usr/local/bin/spindle-entrypoint.sh
#!/bin/sh
set -eu

[ -z "${SPINDLE_SERVER_OWNER:-}" ] && [ -r /shared/owner-did ] && \
    export SPINDLE_SERVER_OWNER="$(cat /shared/owner-did)"
: "${SPINDLE_SERVER_OWNER:?set via env or /shared/owner-did}"

mkdir -p /var/lib/spindle /var/lib/spindle/overlays /var/log/spindle

if [ -f /usr/local/share/ca-certificates/caddy.crt ]; then
    update-ca-certificates
fi

exec /usr/local/bin/spindle run
EOF
RUN chmod +x /usr/local/bin/spindle-entrypoint.sh

VOLUME /var/lib/spindle
EXPOSE 6555

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/usr/local/bin/spindle-entrypoint.sh"]
