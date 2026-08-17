# Development only. Not for production use.

FROM docker.io/oven/bun:alpine AS build

RUN apk add --no-cache git patch

WORKDIR /src
RUN git clone https://github.com/notjuliet/pdsls . && \
    git checkout b52109b6a98953701aa2bf8038568eabc787251a

COPY localinfra/pdsls.patch ./
RUN patch -p1 < pdsls.patch

RUN bun install
RUN bun run build

FROM docker.io/library/caddy:2-alpine
COPY --from=build /src/dist /usr/share/caddy
RUN echo ":80 {" > /etc/caddy/Caddyfile && \
    echo "    root * /usr/share/caddy" >> /etc/caddy/Caddyfile && \
    echo "    file_server" >> /etc/caddy/Caddyfile && \
    echo "    try_files {path} {path}/ /index.html" >> /etc/caddy/Caddyfile && \
    echo "}" >> /etc/caddy/Caddyfile
EXPOSE 80
