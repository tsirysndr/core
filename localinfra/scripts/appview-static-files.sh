#!/usr/bin/env bash
set -euo pipefail

HTMX_URL="https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js"
HTMX_WS_URL="https://cdn.jsdelivr.net/npm/htmx-ext-ws@2.0.2"
MERMAID_URL="https://cdn.jsdelivr.net/npm/mermaid@11.12.3/dist/mermaid.min.js"
LUCIDE_URL="https://github.com/lucide-icons/lucide/releases/download/0.536.0/lucide-icons-0.536.0.zip"
INTER_URL="https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip"
PLEX_MONO_URL="https://github.com/IBM/plex/releases/download/%40ibm%2Fplex-mono%401.1.0/ibm-plex-mono.zip"
ACTOR_TYPEAHEAD_REPO="https://tangled.org/@jakelazaroff.com/actor-typeahead"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT="$REPO_ROOT/appview/pages/static"
TMP="$(mktemp -d)"
# trap 'rm -rf "$TMP"' EXIT

mkdir -p "$OUT"/{fonts,icons,logos}

curl -fsSL -o "$OUT/htmx.min.js"        "$HTMX_URL"
curl -fsSL -o "$OUT/htmx-ext-ws.min.js" "$HTMX_WS_URL"
curl -fsSL -o "$OUT/mermaid.min.js"     "$MERMAID_URL"

curl -fsSL -o "$TMP/lucide.zip" "$LUCIDE_URL"
unzip -q "$TMP/lucide.zip" -d "$TMP/lucide"
cp -rf "$TMP"/lucide/icons/*.svg "$OUT/icons/"

curl -fsSL -o "$TMP/inter.zip" "$INTER_URL"
unzip -q "$TMP/inter.zip" -d "$TMP/inter"
cp -f "$TMP"/inter/web/InterVariable*.woff2 "$OUT/fonts/"
cp -f "$TMP"/inter/web/InterDisplay*.woff2  "$OUT/fonts/"
cp -f "$TMP"/inter/InterVariable*.ttf       "$OUT/fonts/"

curl -fsSL -o "$TMP/plex.zip" "$PLEX_MONO_URL"
unzip -q "$TMP/plex.zip" -d "$TMP/plex"
cp -f "$TMP"/plex/ibm-plex-mono/fonts/complete/woff2/IBMPlexMono*.woff2 "$OUT/fonts/"

git clone --depth=1 "$ACTOR_TYPEAHEAD_REPO" "$TMP/actor-typeahead"
cp -f "$TMP/actor-typeahead/actor-typeahead.js" "$OUT/"

(cd "$REPO_ROOT" && go build -o "$TMP/dolly" ./cmd/dolly)
TEMPLATE="$REPO_ROOT/appview/pages/templates/fragments/dolly/logo.html"
"$TMP/dolly" -template "$TEMPLATE" -output "$OUT/logos/dolly.png" -size 180x180
"$TMP/dolly" -template "$TEMPLATE" -output "$OUT/logos/dolly.ico" -size 48x48
"$TMP/dolly" -template "$TEMPLATE" -output "$OUT/logos/dolly.svg" -color currentColor -favicon
