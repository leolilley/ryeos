#!/usr/bin/env bash
# Browser smoke for an installed RyeOS distribution: initialize an isolated
# node, start it, mint a launch URL through `ryeos web`, and drive the browser
# client headless (smoke.mjs) through launch, session, seat claim and the
# launcher. Needs Node with @playwright/test and its Chromium installed.

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

cleanup() {
  timeout 15 ryeos stop --force >/dev/null 2>&1 || true
  if [[ "$KEEP" == "1" ]]; then
    ryeos_term_note "retained smoke artifacts at $WORK_ROOT"
  else
    rm -rf "$WORK_ROOT"
  fi
}
smoke_exit() {
  local status="$1"
  ryeos_term_handle_exit "$status"
  cleanup
  return "$status"
}
trap 'smoke_exit "$?"' EXIT

ryeos_term_info "initializing isolated node"
init_args=(init --non-interactive --source "$BUNDLE_SOURCE" --node-profile full --host-class local)
if [[ -n "$TRUST_FILE" ]]; then
  [[ -f "$TRUST_FILE" ]] || {
    ryeos_term_fail "RYEOS_SMOKE_TRUST_FILE does not exist: $TRUST_FILE"
    exit 2
  }
  init_args+=(--trust-file "$TRUST_FILE")
fi
timeout "$INIT_TIMEOUT" ryeos "${init_args[@]}"
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

ryeos_term_info "driving the browser client"
timeout "$BROWSER_TIMEOUT" node "$script_dir/smoke.mjs" "$LAUNCH_URL" "$SHOTS"
ryeos_term_note "browser smoke passed"
