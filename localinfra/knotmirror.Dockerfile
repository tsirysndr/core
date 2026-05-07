# Development only. Not for production use.

FROM golang:1.25-alpine AS build

RUN apk add --no-cache git

WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -o /knotmirror ./cmd/knotmirror

FROM alpine:3.22

RUN apk add --no-cache git tini ca-certificates

# Trust dev CA in the system bundle so git/curl/openssl all accept caddy certs.
COPY localinfra/certs/root.crt /usr/local/share/ca-certificates/caddy.crt
RUN update-ca-certificates

COPY --from=build /knotmirror /usr/local/bin/knotmirror

EXPOSE 7000

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/usr/local/bin/knotmirror", "serve"]
