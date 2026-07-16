#!/usr/bin/env bash
# smoke-execute-stream.sh — real signed /execute/stream + SSE smoke.
#
# Signs a POST to the daemon's /execute/stream endpoint using the canonical
# ryeos-request-v1 protocol and asserts SSE frames arrive.
#
# Usage:
#   ./scripts/smoke-execute-stream.sh \
#     --url http://localhost:8000 \
#     --key-pem /tmp/client.pem \
#     --audience "fp:abc123..." \
#     --item-ref directive:hello \
#     --project-path /data/projects/my-app

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$script_dir/lib/ryeos-terminal.sh"
ryeos_term_init

URL=""
KEY_PEM=""
AUDIENCE=""
ITEM_REF="directive:hello"
PROJECT_PATH="."
PARAMS_JSON='{}'
TIMEOUT=30

while [[ $# -gt 0 ]]; do
  case "$1" in
    --url)          URL="$2"; shift 2 ;;
    --key-pem)      KEY_PEM="$2"; shift 2 ;;
    --audience)     AUDIENCE="$2"; shift 2 ;;
    --item-ref)     ITEM_REF="$2"; shift 2 ;;
    --project-path) PROJECT_PATH="$2"; shift 2 ;;
    --params-json)  PARAMS_JSON="$2"; shift 2 ;;
    --timeout)      TIMEOUT="$2"; shift 2 ;;
    -h|--help)
      printf 'Usage: %s --url <url> --key-pem <pem> --audience <fp:...> [--item-ref <ref>] [--project-path <path>]\n' "$0"
      exit 0 ;;
    *) ryeos_term_fail "unknown argument: $1"; exit 2 ;;
  esac
done

if [[ -z "$URL" || -z "$KEY_PEM" || -z "$AUDIENCE" ]]; then
  ryeos_term_fail "--url, --key-pem, and --audience are required"
  exit 2
fi

# 1. Build body JSON.
BODY=$(jq -nc \
  --arg ref "$ITEM_REF" \
  --arg pp "$PROJECT_PATH" \
  --argjson p "$PARAMS_JSON" \
  '{item_ref:$ref, project_path:$pp, parameters:$p}')

# 2. Compute signing inputs.
TS=$(date +%s)
NONCE=$(openssl rand -hex 16)
BODY_SHA=$(printf '%s' "$BODY" | openssl dgst -sha256 -hex | awk '{print $2}')
PATH_FOR_SIG="/execute/stream"
CANON=$(printf 'ryeos-request-v1\nPOST\n%s\n%s\n%s\n%s\n%s' \
  "$PATH_FOR_SIG" "$BODY_SHA" "$TS" "$NONCE" "$AUDIENCE")
CANON_HASH=$(printf '%s' "$CANON" | openssl dgst -sha256 -hex | awk '{print $2}')

# 3. Sign sha256_hex(canonical) ASCII bytes with the Ed25519 key.
SIG_FILE=$(mktemp)
printf '%s' "$CANON_HASH" > "$SIG_FILE"
SIG_RAW=$(openssl pkeyutl -sign -inkey "$KEY_PEM" -rawin -in "$SIG_FILE" 2>/dev/null | base64 -w0)
rm -f "$SIG_FILE"
SIG="$SIG_RAW"

# 4. Derive fingerprint of the public key for x-ryeos-key-id.
FP=$(openssl pkey -in "$KEY_PEM" -pubout -outform der 2>/dev/null \
  | tail -c 32 | openssl dgst -sha256 -hex | awk '{print $2}')

# 5. POST /execute/stream and capture SSE.
ryeos_term_section "execute stream smoke"
ryeos_term_row "endpoint" "$URL/execute/stream"
ryeos_term_row "key" "fp:$FP"
ryeos_term_row "item" "$ITEM_REF"
ryeos_term_row "project" "$PROJECT_PATH"

TMP=$(mktemp)
cleanup_stream_smoke() {
  local status="$1"
  ryeos_term_handle_exit "$status"
  rm -f "$TMP"
  return "$status"
}
trap 'cleanup_stream_smoke "$?"' EXIT

ryeos_term_begin VERIFY "waiting for SSE stream"

HTTP_CODE=$(timeout "$TIMEOUT" curl -sS -o "$TMP" -w '%{http_code}' -N -X POST "$URL/execute/stream" \
  -H "x-ryeos-key-id: fp:$FP" \
  -H "x-ryeos-timestamp: $TS" \
  -H "x-ryeos-nonce: $NONCE" \
  -H "x-ryeos-signature: $SIG" \
  -H "content-type: application/json" \
  -d "$BODY" || true)

if [[ "$HTTP_CODE" != "200" ]]; then
  ryeos_term_fail "execute stream returned HTTP $HTTP_CODE"
  cat "$TMP" >&2
  exit 1
fi

# 6. Assert we got at least one SSE frame.
if ! grep -qE '^event:' "$TMP"; then
  ryeos_term_fail "no SSE event frames in response"
  cat "$TMP" >&2
  exit 1
fi

ryeos_term_end success "VERIFY COMPLETE" "received SSE stream"
