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
    go build -tags libsqlite3 -o /out/deliberi ./cmd/deliberi

FROM alpine:3.24

RUN apk add --no-cache ca-certificates sqlite-libs tini

COPY --from=builder /out/deliberi /usr/local/bin/deliberi
RUN chmod 0755 /usr/local/bin/deliberi

VOLUME /var/lib/deliberi
EXPOSE 6565

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["sh", "-c", "if [ -f /usr/local/share/ca-certificates/caddy.crt ]; then update-ca-certificates; fi && exec /usr/local/bin/deliberi serve"]
