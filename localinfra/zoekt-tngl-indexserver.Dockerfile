# Development only. Not for production use.

FROM golang:1.25-alpine AS builder

RUN apk add --no-cache git

ENV CGO_ENABLED=0
ENV GOCACHE=/go/cache
ENV GOMODCACHE=/go/mod
ENV GOBIN=/out

WORKDIR /src

COPY go.mod go.sum ./
RUN --mount=type=cache,target=/go/cache \
    --mount=type=cache,target=/go/mod \
    go mod download

# zoekt-git-index is invoked at runtime by zoekt-tngl-indexserver via PATH.
RUN --mount=type=cache,target=/go/cache \
    --mount=type=cache,target=/go/mod \
    go install github.com/sourcegraph/zoekt/cmd/zoekt-git-index@latest

COPY . .
RUN --mount=type=cache,target=/go/cache \
    --mount=type=cache,target=/go/mod \
    go build -o /out/zoekt-tngl-indexserver ./cmd/zoekt-tngl-indexserver

FROM alpine:3.20

RUN apk add --no-cache git ca-certificates tini

# Trust dev CA in the system bundle so git/curl/openssl all accept caddy certs.
COPY localinfra/certs/root.crt /usr/local/share/ca-certificates/caddy.crt
RUN update-ca-certificates

COPY --from=builder /out/zoekt-tngl-indexserver /usr/local/bin/zoekt-tngl-indexserver
COPY --from=builder /out/zoekt-git-index /usr/local/bin/zoekt-git-index
RUN chmod 0755 /usr/local/bin/zoekt-tngl-indexserver /usr/local/bin/zoekt-git-index

EXPOSE 6060

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["zoekt-tngl-indexserver", "serve", "-index_dir", "/data/index"]
