#!/usr/bin/env bash
# Golden-path smoke for an installed RyeOS distribution. Exercises durable
# thread recovery across a graceful daemon restart using a deterministic,
# long-running subprocess fixture.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$script_dir/lib/ryeos-terminal.sh"
ryeos_term_init

APP_ROOT="${RYEOS_SMOKE_APP_ROOT:-}"
BUNDLE_SOURCE="${RYEOS_SMOKE_BUNDLE_SOURCE:-/usr/share/ryeos}"
READY_TIMEOUT="${RYEOS_SMOKE_READY_TIMEOUT:-60}"
STATE_TIMEOUT="${RYEOS_SMOKE_STATE_TIMEOUT:-30}"
COMMAND_TIMEOUT="${RYEOS_SMOKE_COMMAND_TIMEOUT:-45}"
KEEP="${RYEOS_SMOKE_KEEP:-0}"
TRUST_FILE="${RYEOS_SMOKE_TRUST_FILE:-}"

for command in ryeos python3 timeout; do
  command -v "$command" >/dev/null 2>&1 || {
    ryeos_term_fail "required command not found: $command"
    exit 2
  }
done

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/ryeos-resume-smoke.XXXXXX")"
PROJECT_ROOT="$WORK_ROOT/project"
if [[ -z "$APP_ROOT" ]]; then
  APP_ROOT="$WORK_ROOT/app"
fi
export RYEOS_APP_ROOT="$APP_ROOT"

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

bounded() {
  timeout "$COMMAND_TIMEOUT" "$@"
}

mkdir -p "$PROJECT_ROOT/.ai/tools/smoke" "$PROJECT_ROOT/.ai/graphs/smoke"
cat >"$PROJECT_ROOT/.ai/tools/smoke/resume.yaml" <<'YAML'
category: smoke
version: "1.0.0"
executor_id: "@subprocess"
description: Deterministic long-running fixture for installed resume smoke tests
config:
  command: /bin/sh
  args: ["-c", "sleep 120; printf ryeos-resume-smoke-ok"]
  timeout_secs: 180
YAML
cat >"$PROJECT_ROOT/.ai/graphs/smoke/resume.yaml" <<'YAML'
version: "1.0.0"
category: smoke
description: Native-resume graph wrapping the deterministic long-running fixture
config:
  start: wait
  nodes:
    wait:
      action: {item_id: "tool:smoke/resume"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
YAML

json_value() {
  local expression="$1"
  python3 -c '
import json, sys
needle, mode = sys.argv[1].split(":", 1)
value = json.load(sys.stdin)
def walk(node):
    if isinstance(node, dict):
        if needle in node and isinstance(node[needle], str):
            yield node[needle]
        for child in node.values():
            yield from walk(child)
    elif isinstance(node, list):
        for child in node:
            yield from walk(child)
values = list(walk(value))
if mode == "first" and values:
    print(values[0])
elif mode == "all":
    print("\n".join(values))
' "$expression"
}

wait_ready() {
  local deadline=$((SECONDS + READY_TIMEOUT))
  local status
  while true; do
    status="$(timeout 5 ryeos node status --json 2>/dev/null || true)"
    if grep -Fq '"Running"' <<<"$status"; then
      return 0
    fi
    if (( SECONDS >= deadline )); then
      ryeos_term_fail "node did not become ready in ${READY_TIMEOUT}s"
      printf '%s\n' "$status" >&2
      return 1
    fi
    sleep 1
  done
}

thread_json() {
  bounded ryeos thread get --thread-id "$1" --json
}

wait_thread_active() {
  local thread_id="$1"
  local deadline=$((SECONDS + STATE_TIMEOUT))
  local detail states
  while (( SECONDS < deadline )); do
    if detail="$(thread_json "$thread_id" 2>/dev/null)"; then
      states="$(printf '%s' "$detail" | json_value status:all)"
      if grep -Eq '^(created|launching|running|resuming)$' <<<"$states"; then
        printf '%s' "$detail"
        return 0
      fi
    fi
    sleep 1
  done
  ryeos_term_fail "thread $thread_id did not reach an active state"
  thread_json "$thread_id" >&2 || true
  return 1
}

ryeos_term_info "initializing isolated node"
init_args=(init --source "$BUNDLE_SOURCE")
if [[ -n "$TRUST_FILE" ]]; then
  [[ -f "$TRUST_FILE" ]] || {
    ryeos_term_fail "RYEOS_SMOKE_TRUST_FILE does not exist: $TRUST_FILE"
    exit 2
  }
  init_args+=(--trust-file "$TRUST_FILE")
fi
bounded ryeos "${init_args[@]}"
bounded ryeos start
wait_ready

ryeos_term_begin VERIFY "launching resumable fixture"
LAUNCH_JSON="$(bounded ryeos --project "$PROJECT_ROOT" execute --async graph:smoke/resume --json)"
THREAD_ID="$(printf '%s' "$LAUNCH_JSON" | json_value thread_id:first)"
if [[ -z "$THREAD_ID" ]]; then
  ryeos_term_fail "launch response contained no thread ID"
  printf '%s\n' "$LAUNCH_JSON" >&2
  exit 1
fi

BEFORE_JSON="$(wait_thread_active "$THREAD_ID")"
CHAIN_ROOT="$(printf '%s' "$BEFORE_JSON" | json_value chain_root_id:first)"
[[ -n "$CHAIN_ROOT" ]] || {
  ryeos_term_fail "thread detail contained no chain root ID"
  exit 1
}

ryeos_term_update "restarting active thread" "$THREAD_ID"
ryeos_term_suspend
bounded ryeos stop
bounded ryeos start
wait_ready
ryeos_term_end success VERIFY "daemon restarted"
ryeos_term_begin VERIFY "checking resumed thread"

AFTER_JSON="$(wait_thread_active "$THREAD_ID")"
AFTER_THREAD="$(printf '%s' "$AFTER_JSON" | json_value thread_id:first)"
AFTER_CHAIN="$(printf '%s' "$AFTER_JSON" | json_value chain_root_id:first)"
[[ "$AFTER_THREAD" == "$THREAD_ID" ]] || {
  ryeos_term_fail "durable thread identity changed after restart"
  exit 1
}
[[ "$AFTER_CHAIN" == "$CHAIN_ROOT" ]] || {
  ryeos_term_fail "durable chain identity changed after restart"
  exit 1
}

PROOF_JSON="$(bounded ryeos thread chain --thread-id "$THREAD_ID" --json)"
if ! grep -Fq "$THREAD_ID" <<<"$PROOF_JSON" || ! grep -Fq "$CHAIN_ROOT" <<<"$PROOF_JSON"; then
  ryeos_term_fail "chain proof does not contain durable identities"
  printf '%s\n' "$PROOF_JSON" >&2
  exit 1
fi

ryeos_term_end success "VERIFY COMPLETE" "thread $THREAD_ID resumed in chain $CHAIN_ROOT"
printf '%s\n' "$PROOF_JSON"
