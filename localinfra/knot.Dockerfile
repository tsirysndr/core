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
    go build -tags libsqlite3 -o /out/knot ./cmd/knot

FROM alpine:3.20

RUN apk add --no-cache git openssh-server tini sqlite-libs su-exec ca-certificates shadow openssl bash

RUN groupadd -g 1000 -f git && \
    useradd -u 1000 -g 1000 -d /home/git -s /bin/sh -m git && \
    echo "git:$(openssl rand -hex 16)" | chpasswd

COPY --from=builder /out/knot /usr/local/bin/knot
RUN chmod 0755 /usr/local/bin/knot

COPY <<'EOF' /usr/local/bin/knot-keys-wrapper
#!/bin/sh
exec /usr/local/bin/knot keys -output authorized-keys \
    -internal-api "http://${KNOT_SERVER_INTERNAL_LISTEN_ADDR:-127.0.0.1:5444}" \
    -git-dir "${KNOT_REPO_SCAN_PATH:-/home/git/repositories}" \
    -log-path "/tmp/knotguard.log"
EOF
RUN chmod +x /usr/local/bin/knot-keys-wrapper

# sshd config
COPY <<'EOF' /etc/ssh/sshd_config.d/knot.conf
PermitRootLogin no
PasswordAuthentication no
ChallengeResponseAuthentication no

Match User git
    AuthorizedKeysCommand /usr/local/bin/knot-keys-wrapper
    AuthorizedKeysCommandUser nobody
EOF

RUN echo 'Include /etc/ssh/sshd_config.d/*.conf' >> /etc/ssh/sshd_config

COPY <<'EOF' /etc/ssh/sshd_config.d/host-keys.conf
HostKey /etc/ssh/keys/ssh_host_rsa_key
HostKey /etc/ssh/keys/ssh_host_ecdsa_key
HostKey /etc/ssh/keys/ssh_host_ed25519_key
EOF

RUN mkdir -p /home/git/.config/git
COPY <<'EOF' /home/git/.config/git/config
[user]
    name = Tangled
    email = noreply@tangled.org
[receive]
    advertisePushOptions = true
[uploadpack]
    allowFilter = true
    allowReachableSHA1InWant = true
EOF
RUN mkdir -p /home/git/repositories && chown -R git:git /home/git

COPY <<'EOF' /usr/local/bin/knot-entrypoint.sh
#!/bin/sh
set -eu
[ -z "${KNOT_SERVER_OWNER:-}" ] && [ -r /shared/owner-did ] && \
    export KNOT_SERVER_OWNER="$(cat /shared/owner-did)"
: "${KNOT_SERVER_OWNER:?set via env or /shared/owner-did}"

mkdir -p /etc/ssh/keys
[ -f /etc/ssh/keys/ssh_host_rsa_key ]     || ssh-keygen -t rsa     -f /etc/ssh/keys/ssh_host_rsa_key     -q -N ""
[ -f /etc/ssh/keys/ssh_host_ecdsa_key ]   || ssh-keygen -t ecdsa   -f /etc/ssh/keys/ssh_host_ecdsa_key   -q -N ""
[ -f /etc/ssh/keys/ssh_host_ed25519_key ] || ssh-keygen -t ed25519 -f /etc/ssh/keys/ssh_host_ed25519_key -q -N ""

/usr/sbin/sshd -D -e &
exec su-exec git /usr/local/bin/knot server
EOF
RUN chmod +x /usr/local/bin/knot-entrypoint.sh

VOLUME /home/git
EXPOSE 22 5555

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["/usr/local/bin/knot-entrypoint.sh"]
