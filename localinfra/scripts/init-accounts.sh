#!/bin/sh
# dev bootstrap:
# - create accounts (alice, bob)
# - write OWNER_DID to /shared/owner-did (for knot/spindle)
# - create system label definitions under SYSTEM_DID
set -eu

: "${PDS_URL:?PDS_URL must be set}"
PASSWORD="password"

USERS="alice bob"
OWNER_USER="${OWNER_USER:-alice}"
SYSTEM_USER="${SYSTEM_USER:-alice}"
SHARED_DIR="${SHARED_DIR:-/shared}"

# --- helpers ---

# resolve_handle HANDLE → DID on stdout
resolve_handle() {
  resp=$(curl -sS -w '\n%{http_code}' \
    "${PDS_URL}/xrpc/com.atproto.identity.resolveHandle?handle=$1")
  body=$(printf '%s\n' "$resp" | sed '$d')
  status=$(printf '%s\n' "$resp" | tail -n1)
  case "$status" in
    200) printf '%s\n' "$body" | jq -er '.did' ;;
    400) : ;;  # not found — expected
    *)   printf 'resolveHandle %s: HTTP %s: %s\n' "$1" "$status" "$body" >&2; return 1 ;;
  esac
}

# ensure_account USERNAME → DID on stdout. Creates account if missing.
ensure_account() {
  username="$1"
  handle="${username}.${PDS_HOSTNAME}"
  email="${username}@${PDS_HOSTNAME}"

  did=$(resolve_handle "$handle")
  if [ -n "$did" ]; then
    printf '[skip] %s = %s\n' "$handle" "$did" >&2
    printf '%s\n' "$did"
    return 0
  fi

  invite=$(curl -fsS -u "admin:${PDS_ADMIN_PASSWORD}" \
    -H "Content-Type: application/json" \
    -d '{"useCount":1}' \
    "${PDS_URL}/xrpc/com.atproto.server.createInviteCode" | jq -er '.code')

  result=$(curl -fsS \
    -H "Content-Type: application/json" \
    -d "{\"email\":\"${email}\",\"handle\":\"${handle}\",\"password\":\"${PASSWORD}\",\"inviteCode\":\"${invite}\"}" \
    "${PDS_URL}/xrpc/com.atproto.server.createAccount")

  did=$(printf '%s\n' "$result" | jq -er '.did')
  printf '[create] %s = %s (password: %s)\n' "$handle" "$did" "$PASSWORD" >&2
  printf '%s\n' "$did"
}

# login DID/Handle → access JWT on stdout
login() {
  curl -fsS -H "Content-Type: application/json" \
    -d "{\"identifier\":\"$1\",\"password\":\"${PASSWORD}\"}" \
    "${PDS_URL}/xrpc/com.atproto.server.createSession" \
    | jq -er '.accessJwt'
}

# put_record JWT DID COLLECTION RKEY RECORD_JSON
put_record() {
  jwt="$1"; did="$2"; collection="$3"; rkey="$4"; record="$5"

  payload=$(jq -nc \
    --arg repo "$did" \
    --arg collection "$collection" \
    --arg rkey "$rkey" \
    --argjson record "$record" \
    '{repo:$repo, collection:$collection, rkey:$rkey, record:$record}')

  curl -fsS \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer ${jwt}" \
    -d "$payload" \
    "${PDS_URL}/xrpc/com.atproto.repo.putRecord" >/dev/null

  printf '[record] at://%s/%s/%s\n' "$did" "$collection" "$rkey" >&2
}

# ensure accounts
OWNER_DID=""
SYSTEM_DID=""
for u in $USERS; do
  did=$(ensure_account "$u")
  if [ "$u" = "$OWNER_USER" ]; then
    OWNER_DID="$did"
  fi
  if [ "$u" = "$SYSTEM_USER" ]; then
    SYSTEM_DID="$did"
  fi
done

[ -n "$OWNER_DID" ] || { printf 'OWNER_USER %s not in USERS list\n' "$OWNER_USER" >&2; exit 1; }
[ -n "$SYSTEM_DID" ] || { printf 'SYSTEM_USER %s not in USERS list\n' "$SYSTEM_USER" >&2; exit 1; }

mkdir -p "$SHARED_DIR"
printf '%s' "$OWNER_DID" > "${SHARED_DIR}/owner-did"
printf '[owner] %s → %s/owner-did\n' "$OWNER_USER" "$SHARED_DIR" >&2
printf '%s' "$SYSTEM_DID" > "${SHARED_DIR}/system-did"
printf '[system] %s → %s/system-did\n' "$SYSTEM_USER" "$SHARED_DIR" >&2

# label definitions (under SYSTEM_DID)
JWT=$(login "$SYSTEM_DID")

CREATED_AT="2025-09-22T11:14:35+01:00"

put_record "$JWT" "$SYSTEM_DID" "sh.tangled.label.definition" "wontfix" "$(cat <<JSON
{
  "name": "wontfix",
  "color": "#64748b",
  "scope": ["sh.tangled.repo.issue"],
  "multiple": false,
  "createdAt": "${CREATED_AT}",
  "valueType": {"type": "null", "format": "any"}
}
JSON
)"

put_record "$JWT" "$SYSTEM_DID" "sh.tangled.label.definition" "good-first-issue" "$(cat <<JSON
{
  "name": "good-first-issue",
  "color": "#8B5CF6",
  "scope": ["sh.tangled.repo.issue"],
  "multiple": false,
  "createdAt": "${CREATED_AT}",
  "valueType": {"type": "null", "format": "any"}
}
JSON
)"

put_record "$JWT" "$SYSTEM_DID" "sh.tangled.label.definition" "duplicate" "$(cat <<JSON
{
  "name": "duplicate",
  "color": "#ef4444",
  "scope": ["sh.tangled.repo.issue"],
  "multiple": false,
  "createdAt": "${CREATED_AT}",
  "valueType": {"type": "null", "format": "any"}
}
JSON
)"

put_record "$JWT" "$SYSTEM_DID" "sh.tangled.label.definition" "documentation" "$(cat <<JSON
{
  "name": "documentation",
  "color": "#06b6d4",
  "scope": ["sh.tangled.repo.issue"],
  "multiple": false,
  "createdAt": "${CREATED_AT}",
  "valueType": {"type": "null", "format": "any"}
}
JSON
)"

put_record "$JWT" "$SYSTEM_DID" "sh.tangled.label.definition" "assignee" "$(cat <<JSON
{
  "name": "assignee",
  "color": "#10B981",
  "scope": ["sh.tangled.repo.issue", "sh.tangled.repo.pull"],
  "multiple": false,
  "createdAt": "${CREATED_AT}",
  "valueType": {"type": "string", "format": "did"}
}
JSON
)"

# shared env values for appview
LABEL_GFI=at://${SYSTEM_DID}/sh.tangled.label.definition/good-first-issue
LABEL_DEFAULTS=$LABEL_GFI
LABEL_DEFAULTS=$LABEL_DEFAULTS,at://${SYSTEM_DID}/sh.tangled.label.definition/assignee
LABEL_DEFAULTS=$LABEL_DEFAULTS,at://${SYSTEM_DID}/sh.tangled.label.definition/documentation
LABEL_DEFAULTS=$LABEL_DEFAULTS,at://${SYSTEM_DID}/sh.tangled.label.definition/duplicate
LABEL_DEFAULTS=$LABEL_DEFAULTS,at://${SYSTEM_DID}/sh.tangled.label.definition/wontfix

printf '%s' "$LABEL_GFI" > "${SHARED_DIR}/label-gfi"
printf '%s' "$LABEL_DEFAULTS" > "${SHARED_DIR}/label-defaults"
printf '[env] wrote label-defaults, label-gfi\n' >&2

# service definitions (under OWNER_DID)
JWT=$(login "$OWNER_DID")

put_record "$JWT" "$OWNER_DID" "sh.tangled.knot" $KNOT_HOSTNAME "{\"createdAt\": \"${CREATED_AT}\"}"

printf 'done.\n' >&2
