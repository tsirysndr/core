# Development only. Not for production use.

FROM golang:1.25-alpine

RUN apk add --no-cache git build-base sqlite-dev tini sqlite-libs ca-certificates

# air for live-reload
RUN go install github.com/air-verse/air@v1.65.1 && \
    mv /go/bin/air /usr/local/bin/air

# goat for generating OAuth client key (moved out of indigo)
RUN go install github.com/bluesky-social/goat@v0.2.3 && \
    mv /go/bin/goat /usr/local/bin/goat

ENV CGO_ENABLED=1
ENV GOCACHE=/go/cache
ENV GOMODCACHE=/go/mod

# Generates OAuth client key on first run. Persists to appview-data so re-runs
# reuse the same key. Mirrors flake.nix:221-222.
COPY <<'EOF' /usr/local/bin/appview-entrypoint.sh
#!/bin/sh
set -eu

SECRET=/var/lib/appview/oauth-secret
KID=/var/lib/appview/oauth-kid

if [ ! -s "$SECRET" ]; then
    mkdir -p /var/lib/appview
    goat key generate -t P-256 \
        | grep -A1 'Secret Key' | tail -n1 | awk '{print $1}' \
        > "$SECRET"
    date +%s > "$KID"
    echo "[oauth] generated kid=$(cat $KID)"
fi

export TANGLED_OAUTH_CLIENT_SECRET="$(cat $SECRET)"
export TANGLED_OAUTH_CLIENT_KID="$(cat $KID)"

# Pulled in from init-accounts via /shared (mounted ro).
[ -r /shared/label-defaults ] && export TANGLED_LABEL_DEFAULTS="$(cat /shared/label-defaults)"
[ -r /shared/label-gfi ]      && export TANGLED_LABEL_GFI="$(cat /shared/label-gfi)"

exec air -c /src/.air/appview.toml
EOF
RUN chmod +x /usr/local/bin/appview-entrypoint.sh

WORKDIR /src

EXPOSE 3000

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/usr/local/bin/appview-entrypoint.sh"]
