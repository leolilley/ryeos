#!/usr/bin/env bash
# Browser smoke for an installed RyeOS distribution: initialize an isolated
# node, start it, mint a launch URL through `ryeos web`, and drive the browser
# client headless (smoke.mjs) through launch, session, seat claim, the launcher
# and the drawn Surface ground (theme catalogue, granted picture fetches,
# refusals, and a canvas that differs from the flat Surface). The installed
# ryeos-ui bundle must ship its picture blobs and the populated signed picture
# manifest; the smoke fails when it does not. Needs Node with @playwright/test and its Chromium installed.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$script_dir/../../../scripts/lib/ryeos-terminal.sh"
ryeos_term_init

BUNDLE_SOURCE="${RYEOS_SMOKE_BUNDLE_SOURCE:-/usr/share/ryeos}"
TRUST_FILE="${RYEOS_SMOKE_TRUST_FILE:-}"
READY_TIMEOUT="${RYEOS_SMOKE_READY_TIMEOUT:-60}"
INIT_TIMEOUT="${RYEOS_SMOKE_INIT_TIMEOUT:-120}"
BROWSER_TIMEOUT="${RYEOS_SMOKE_BROWSER_TIMEOUT:-300}"
SHOTS="${RYEOS_SMOKE_SHOTS:-}"
KEEP="${RYEOS_SMOKE_KEEP:-0}"

for command in ryeos node timeout; do
  command -v "$command" >/dev/null 2>&1 || {
    ryeos_term_fail "required command not found: $command"
    exit 2
  }
done

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ryeos-browser-smoke.XXXXXX")"
export RYEOS_APP_ROOT="$WORK_ROOT/app"
SHOTS="${SHOTS:-$WORK_ROOT/shots}"

# A directly spawned daemon has no host-backed process-scope delegation, so the
# standard profile refuses to start it. On a disposable CI VM the operator
# runs the product's own one-time host setup (`ryeos node host setup`), which
# provisions a native runit service with a delegated cgroup subtree; start,
# stop and status then go through that service. Off by default.
HOST_SERVICE="${RYEOS_SMOKE_HOST_SERVICE:-0}"
HOST_NODE_MEMORY_BYTES="${RYEOS_SMOKE_NODE_MEMORY_BYTES:-8589934592}"
HOST_NODE_PROCESSES="${RYEOS_SMOKE_NODE_PROCESSES:-8192}"

host_service_setup() {
  [[ "$HOST_SERVICE" == "1" ]] || return 0
  ryeos_term_info "provisioning the host service association"
  timeout 120 ryeos node host setup --confirm \
    --node-memory-bytes "$HOST_NODE_MEMORY_BYTES" --node-processes "$HOST_NODE_PROCESSES"
}

host_service_retire() {
  [[ "$HOST_SERVICE" == "1" ]] || return 0
  timeout 60 ryeos node host retire --confirm >/dev/null 2>&1 || true
}

host_service_diagnostics() {
  [[ "$HOST_SERVICE" == "1" ]] || return 0
  echo "--- ryeos node doctor" >&2
  timeout 60 ryeos node doctor 2>&1 | head -n 120 >&2 || true
  echo "--- host service logs" >&2
  sudo -n sh -c 'for f in /var/log/lillux-host-service/*/current; do echo "== $f"; tail -n 80 "$f"; done' >&2 2>&1 || true
}

cleanup() {
  timeout 15 ryeos stop --force >/dev/null 2>&1 || true
  host_service_retire
  if [[ "$KEEP" == "1" ]]; then
    ryeos_term_note "retained smoke artifacts at $WORK_ROOT"
  else
    rm -rf "$WORK_ROOT"
  fi
}
smoke_exit() {
  local status="$1"
  [[ "$status" == "0" ]] || host_service_diagnostics
  ryeos_term_handle_exit "$status"
  cleanup
  return "$status"
}
trap 'smoke_exit "$?"' EXIT

ryeos_term_info "initializing isolated node"
init_args=(init --non-interactive --source "$BUNDLE_SOURCE" --node-profile standard --host-class local)
# A fresh node binds to an explicit substrate identity. This isolated node runs
# the installed binaries directly, so name the installed ryeos executable.
substrate_digest="$(sha256sum -- "$(command -v ryeos)")"
init_args+=(--substrate-image-digest "sha256:${substrate_digest%% *}" --substrate-protocol 1)
if [[ -n "$TRUST_FILE" ]]; then
  [[ -f "$TRUST_FILE" ]] || {
    ryeos_term_fail "RYEOS_SMOKE_TRUST_FILE does not exist: $TRUST_FILE"
    exit 2
  }
  init_args+=(--trust-file "$TRUST_FILE")
fi
timeout "$INIT_TIMEOUT" ryeos "${init_args[@]}"
host_service_setup
timeout 60 ryeos start

deadline=$((SECONDS + READY_TIMEOUT))
until timeout 5 ryeos node status --json 2>/dev/null | grep -Fq '"Running"'; do
  if (( SECONDS >= deadline )); then
    ryeos_term_fail "node did not become ready in ${READY_TIMEOUT}s"
    exit 1
  fi
  sleep 1
done

ryeos_term_begin VERIFY "minting a launch URL"
LAUNCH_URL="$(timeout 30 ryeos web --presentation config:ryeos/ui/presentation --print-url --no-open | tail -n 1)"
[[ "$LAUNCH_URL" == http*"/ui/launch/"* ]] || {
  ryeos_term_fail "ryeos web printed no launch URL"
  exit 1
}

ui_ai="$BUNDLE_SOURCE/ryeos-ui/.ai"
[[ -f "$ui_ai/refs/bundles/pictures" ]] || {
  ryeos_term_fail "the installed ryeos-ui bundle has no signed picture manifest at $ui_ai/refs/bundles/pictures"
  exit 1
}
[[ -n "$(ls -A "$ui_ai/pictures/blobs" 2>/dev/null)" ]] || {
  ryeos_term_fail "the installed ryeos-ui bundle ships no picture blobs under $ui_ai/pictures/blobs"
  exit 1
}
export RYEOS_SMOKE_PICTURE_BLOBS="$ui_ai/pictures/blobs"

ryeos_term_info "driving the browser client"
timeout "$BROWSER_TIMEOUT" node "$script_dir/smoke.mjs" "$LAUNCH_URL" "$SHOTS"
ryeos_term_note "browser smoke passed"
