#!/usr/bin/env bash
# delicurl — curl through service auth for deliberi endpoints.
#
# Authenticates against a PDS with an app password, obtains a service
# auth JWT, and proxies the request to the target URL.
#
# Usage:
#   export DELICURL_USER=alice.tngl.sh
#   export DELICURL_PASSWORD="xxxx-xxxx-xxxx-xxxx"
#
#   # GET (no --data, no -X)
#   ./cmd/delicurl.sh \
#     http://localhost:6565/xrpc/org.tangled.temp.notification.getUnreadCount
#
#   # POST with body
#   ./cmd/delicurl.sh --data '{"read":true}' \
#     http://localhost:6565/xrpc/org.tangled.temp.notification.updateSeen
#
#   # POST without body (e.g. markAllRead)
#   ./cmd/delicurl.sh -X POST \
#     http://localhost:6565/xrpc/org.tangled.temp.notification.markAllRead

set -euo pipefail

PDS="${DELICURL_PDS:-https://pds.tngl.boltless.dev}"
USER="${DELICURL_USER:-}"
PASSWORD="${DELICURL_PASSWORD:-}"
AUD="${DELICURL_AUD:-did:web:deliberi.tngl.boltless.dev}"
LXM=""
DATA=""
HTTP_METHOD=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --method|-m) LXM="$2";     shift 2 ;;
    --data|-d)   DATA="$2";    shift 2 ;;
    -X)          HTTP_METHOD="$2"; shift 2 ;;
    --pds)       PDS="$2";     shift 2 ;;
    --aud)       AUD="$2";     shift 2 ;;
    *)           break ;;
  esac
done

URL="${1:-}"
if [[ -z "$URL" ]]; then
  echo "Usage: $0 [-X METHOD] [--method lxm] [--data '{}'] [--pds url] [--aud did:web:...] <url>"
  echo ""
  echo "Environment: DELICURL_USER, DELICURL_PASSWORD (required)"
  exit 1
fi

# Auto-derive lexicon method from URL when not explicitly set.
# Strip query/fragment, then take the last path segment.
URL_CLEAN="${URL%%\?*}"
URL_CLEAN="${URL_CLEAN%%\#*}"
URL_CLEAN="${URL_CLEAN%%/}"
if [[ -z "$LXM" ]]; then
  LXM="${URL_CLEAN##*/}"
fi

# Derive HTTP method if not explicitly set.
if [[ -z "$HTTP_METHOD" ]]; then
  if [[ -n "$DATA" ]]; then
    HTTP_METHOD="POST"
  else
    HTTP_METHOD="GET"
  fi
fi

if [[ -z "$USER" || -z "$PASSWORD" ]]; then
  echo "error: DELICURL_USER and DELICURL_PASSWORD must be set" >&2
  exit 1
fi

# Step 1: create session → access JWT (POST, JSON body)
SESSION=$(curl -sS -X POST "$PDS/xrpc/com.atproto.server.createSession" \
  -H "Content-Type: application/json" \
  -d '{"identifier":"'"$USER"'","password":"'"$PASSWORD"'"}')
ACCESS_JWT=$(echo "$SESSION" | python3 -c "import sys,json; print(json.load(sys.stdin)['accessJwt'])")

# Step 2: get service auth token (GET query — aud, exp, lxm are URL params)
EXP=$(( $(date +%s) + 300 ))  # 5 min
SA_TOKEN=$(curl -sS -G "$PDS/xrpc/com.atproto.server.getServiceAuth" \
  -H "Authorization: Bearer $ACCESS_JWT" \
  --data-urlencode "aud=$AUD" \
  --data-urlencode "exp=$EXP" \
  --data-urlencode "lxm=$LXM" \
  | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

# Step 3: make the request
CURL_ARGS=(-sS -w "\n--- status: %{http_code} ---" -X "$HTTP_METHOD" -H "Authorization: Bearer $SA_TOKEN")
if [[ -n "$DATA" ]]; then
  CURL_ARGS+=(-H "Content-Type: application/json" -d "$DATA")
fi
curl "${CURL_ARGS[@]}" "$URL"
