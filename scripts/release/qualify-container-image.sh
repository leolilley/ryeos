#!/usr/bin/env bash
# Qualify the exact RyeOS image digest that will be promoted to a release tag.

set -euo pipefail

usage() {
  echo "usage: $0 <repository@sha256:digest> <standard|central-host> [--pid1-smoke]" >&2
}

[[ $# -ge 2 && $# -le 3 ]] || { usage; exit 2; }
IMAGE_REF="$1"
VARIANT="$2"
PID1_SMOKE=false
if [[ ${3:-} == --pid1-smoke ]]; then
  PID1_SMOKE=true
elif [[ $# == 3 ]]; then
  usage
  exit 2
fi
EXACT_DIGEST=true
if [[ ! "$IMAGE_REF" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]]; then
  EXACT_DIGEST=false
  [[ ${RYEOS_QUALIFY_ALLOW_LOCAL_TAG:-0} == 1 ]] || {
    echo "qualification requires an exact repository@sha256 digest" >&2
    exit 2
  }
fi
[[ "$VARIANT" == standard || "$VARIANT" == central-host ]] || {
  echo "unsupported image variant: $VARIANT" >&2
  exit 2
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
SUFFIX="${GITHUB_RUN_ID:-local}-$$"
CONTAINER="ryeos-qualify-${VARIANT}-${SUFFIX}"
TEST_IMAGE="ryeos-attachment-test:${SUFFIX}"
KEY_FILE="$WORK/client.pem"
PROJECT_DIR="$WORK/project"
DAEMON_PORT=18081
SHUTDOWN_SMOKE_PID=""

# The production image uses the unconfined-host isolation backend, which
# cannot truthfully enforce read-only live filesystem access. Declare the
# required read-write authority explicitly instead of hiding it in the generic
# SSE helper.
LIVE_POLICY='{
  "schema_version": 2,
  "ownership": "daemon_owned",
  "recovery": "restart_recoverable",
  "response": "wait",
  "target": {"kind": "here"},
  "environment": {
    "kind": "project_overlay",
    "include_operator_vault": true,
    "name_policy": {"kind": "declared_required"}
  },
  "project": {
    "kind": "live_direct",
    "access": "read_write",
    "child_policy": {"kind": "inherit"}
  }
}'

cleanup() {
  if [[ -n "$SHUTDOWN_SMOKE_PID" ]]; then
    kill "$SHUTDOWN_SMOKE_PID" >/dev/null 2>&1 || true
    wait "$SHUTDOWN_SMOKE_PID" >/dev/null 2>&1 || true
  fi
  docker exec "$CONTAINER" chmod -R a+rwX /data/projects/container-knowledge/.ai/state \
    >/dev/null 2>&1 || true
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker image rm "$TEST_IMAGE" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cp -a "$ROOT/tests/container/knowledge-project" "$PROJECT_DIR"
openssl genpkey -algorithm ED25519 -out "$KEY_FILE" >/dev/null 2>&1

if [[ "$EXACT_DIGEST" == true ]]; then
  docker pull "$IMAGE_REF" >/dev/null
else
  docker image inspect "$IMAGE_REF" >/dev/null
fi

if [[ "$PID1_SMOKE" == true ]]; then
  docker build \
    --file "$ROOT/Dockerfile.container-attachment-test" \
    --build-arg "BASE_IMAGE=$IMAGE_REF" \
    --tag "$TEST_IMAGE" \
    "$ROOT"
  docker run --rm \
    --entrypoint /usr/local/libexec/lillux-attachment-smoke \
    "$TEST_IMAGE" --require-pid-1
fi

run_env=()
if [[ ${RYEOS_QUALIFY_TRUST_BAKED_PUBLISHERS:-0} == 1 ]]; then
  run_env=(--env RYEOS_TRUST_BAKED_PUBLISHERS=1)
fi

docker run -d \
  --name "$CONTAINER" \
  --env "PORT=$DAEMON_PORT" \
  --publish "127.0.0.1::$DAEMON_PORT" \
  --mount "type=bind,src=$PROJECT_DIR,dst=/data/projects/container-knowledge" \
  --mount "type=bind,src=$ROOT/scripts/release/container-mock-chat-provider.py,dst=/opt/ryeos-test/mock.py,readonly" \
  "${run_env[@]}" \
  "$IMAGE_REF" >/dev/null

READY=false
for _ in $(seq 1 120); do
  if docker exec "$CONTAINER" sh -c '
    for process_dir in /proc/[0-9]*; do
      if [ "$(readlink "$process_dir/exe" 2>/dev/null || true)" = /usr/local/bin/ryeosd ]; then
        exit 0
      fi
    done
    exit 1
  ' && docker exec "$CONTAINER" python3 -c \
    "import urllib.request; urllib.request.urlopen('http://127.0.0.1:$DAEMON_PORT/health', timeout=1).read()" \
    >/dev/null 2>&1; then
    READY=true
    break
  fi
  if [[ "$(docker inspect --format '{{.State.Running}}' "$CONTAINER")" != true ]]; then
    docker logs "$CONTAINER" >&2
    echo "container exited before RyeOS became ready" >&2
    exit 1
  fi
  sleep 1
done
[[ "$READY" == true ]] || {
  docker logs "$CONTAINER" >&2
  echo "RyeOS did not become ready" >&2
  exit 1
}

PID1_COMM="$(docker exec "$CONTAINER" sh -c 'cat /proc/1/comm')"
[[ "$PID1_COMM" == tini ]] || {
  echo "expected tini as PID 1, observed: $PID1_COMM" >&2
  exit 1
}
DAEMON_PID="$(docker exec "$CONTAINER" sh -c '
  for process_dir in /proc/[0-9]*; do
    if [ "$(readlink "$process_dir/exe" 2>/dev/null || true)" = /usr/local/bin/ryeosd ]; then
      basename "$process_dir"
      exit 0
    fi
  done
  exit 1
')" || {
  docker logs "$CONTAINER" >&2
  echo "could not locate the ryeosd process in the container" >&2
  exit 1
}
[[ "$DAEMON_PID" -gt 1 ]]
DAEMON_PPID="$(docker exec "$CONTAINER" sh -c "awk '/^PPid:/ {print \$2}' /proc/$DAEMON_PID/status")"
[[ "$DAEMON_PPID" == 1 ]] || {
  echo "expected ryeosd PPid 1, observed $DAEMON_PPID" >&2
  exit 1
}

# Run the deterministic provider inside the same exact image. It is test
# infrastructure only; RyeOS still performs a normal authenticated launch.
docker exec -d "$CONTAINER" python3 /opt/ryeos-test/mock.py
MOCK_READY=false
for _ in $(seq 1 50); do
  if docker exec "$CONTAINER" python3 -c \
    'import urllib.request; urllib.request.urlopen("http://127.0.0.1:8000/health", timeout=1).read()' \
    >/dev/null 2>&1; then
    MOCK_READY=true
    break
  fi
  sleep 0.1
done
[[ "$MOCK_READY" == true ]] || {
  echo "mock provider did not become ready" >&2
  exit 1
}

docker exec \
  --env RYEOS_APP_ROOT=/data/app \
  --workdir /data/projects/container-knowledge \
  "$CONTAINER" ryeos sign \
  .ai/config/ryeos-runtime/model_routing.yaml \
  .ai/knowledge/container/important_fact.md \
  .ai/directives/container/context.md \
  .ai/directives/container/shutdown.md >/dev/null

RAW_PUBLIC_KEY="$(openssl pkey -in "$KEY_FILE" -pubout -outform DER 2>/dev/null | tail -c 32 | base64 -w0)"
docker exec "$CONTAINER" ryeos-core-tools authorize-client \
  --app-root /data/app \
  --public-key "$RAW_PUBLIC_KEY" \
  --scopes ryeos.execute.directive.container/context,ryeos.execute.directive.container/shutdown,ryeos.read.project.live,ryeos.write.project.live \
  --label release-image-qualification >/dev/null

AUDIENCE="$(docker exec "$CONTAINER" cat /data/app/.ai/node/identity/public-identity.json | jq -er '.principal_id')"
HOST_PORT="$(docker port "$CONTAINER" "$DAEMON_PORT/tcp" | awk -F: 'NR == 1 {print $NF}')"
"$ROOT/scripts/smoke-execute-stream.sh" \
  --url "http://127.0.0.1:$HOST_PORT" \
  --key-pem "$KEY_FILE" \
  --audience "$AUDIENCE" \
  --item-ref directive:container/context \
  --project-path /data/projects/container-knowledge \
  --params-json '{"name":"container"}' \
  --ref-bindings-json '{"model":"directive:container/context"}' \
  --execution-policy-json "$LIVE_POLICY" \
  --timeout 60 \
  --require-terminal thread_completed

# Prove the PID-1 init reaps an orphan while the namespace remains alive.
docker exec -d "$CONTAINER" sh -c 'sh -c "sleep 5" & echo $! >/tmp/ryeos-reap-probe.pid'
for _ in $(seq 1 50); do
  if docker exec "$CONTAINER" test -s /tmp/ryeos-reap-probe.pid; then break; fi
  sleep 0.1
done
REAP_PID="$(docker exec "$CONTAINER" cat /tmp/ryeos-reap-probe.pid)"
REAP_PPID=""
for _ in $(seq 1 20); do
  REAP_PPID="$(docker exec "$CONTAINER" sh -c \
    "test -r /proc/$REAP_PID/status && awk '/^PPid:/ {print \$2}' /proc/$REAP_PID/status" \
    2>/dev/null || true)"
  [[ "$REAP_PPID" == 1 ]] && break
  sleep 0.1
done
[[ "$REAP_PPID" == 1 ]] || {
  echo "reaping probe was not adopted by PID 1 (PPid $REAP_PPID)" >&2
  exit 1
}
REAPED=false
for _ in $(seq 1 80); do
  if ! docker exec "$CONTAINER" test -e "/proc/$REAP_PID"; then
    REAPED=true
    break
  fi
  sleep 0.1
done
[[ "$REAPED" == true ]] || {
  docker exec "$CONTAINER" sh -c "cat /proc/$REAP_PID/status" >&2 || true
  echo "PID 1 did not reap orphan $REAP_PID" >&2
  exit 1
}

# Keep an attachment-aware directive runtime active while the container
# receives SIGTERM. The mock marks receipt before deliberately withholding its
# response, proving the runtime has crossed the provider launch boundary.
SHUTDOWN_STREAM="$WORK/shutdown-stream.log"
"$ROOT/scripts/smoke-execute-stream.sh" \
  --url "http://127.0.0.1:$HOST_PORT" \
  --key-pem "$KEY_FILE" \
  --audience "$AUDIENCE" \
  --item-ref directive:container/shutdown \
  --project-path /data/projects/container-knowledge \
  --ref-bindings-json '{"model":"directive:container/shutdown"}' \
  --execution-policy-json "$LIVE_POLICY" \
  --timeout 300 >"$SHUTDOWN_STREAM" 2>&1 &
SHUTDOWN_SMOKE_PID=$!

SHUTDOWN_ACTIVE=false
for _ in $(seq 1 100); do
  if docker exec "$CONTAINER" test -s /tmp/mock-shutdown-request; then
    RUNTIME_PID="$(docker exec "$CONTAINER" sh -c '
      daemon_pid="$1"
      for status in /proc/[0-9]*/status; do
        [ -r "$status" ] || continue
        ppid="$(grep "^PPid:" "$status" | tr -dc "0-9")"
        if [ "$ppid" = "$daemon_pid" ]; then
          process_dir="$(dirname "$status")"
          command_path="$(tr "\\0" "\\n" <"$process_dir/cmdline" 2>/dev/null | head -n 1)"
          case "$command_path" in
            */ryeos-directive-runtime)
              basename "$process_dir"
              exit 0
              ;;
          esac
        fi
      done
      exit 1
    ' sh "$DAEMON_PID" 2>/dev/null || true)"
    if [[ "$RUNTIME_PID" =~ ^[0-9]+$ ]]; then
      SHUTDOWN_ACTIVE=true
      break
    fi
  fi
  sleep 0.1
done
[[ "$SHUTDOWN_ACTIVE" == true ]] || {
  cat "$SHUTDOWN_STREAM" >&2
  docker exec "$CONTAINER" sh -c '
    daemon_pid="$1"
    for status in /proc/[0-9]*/status; do
      [ -r "$status" ] || continue
      ppid="$(grep "^PPid:" "$status" | tr -dc "0-9")"
      [ "$ppid" = "$daemon_pid" ] || continue
      process_dir="$(dirname "$status")"
      pid="$(basename "$process_dir")"
      executable="$(readlink "$process_dir/exe" 2>/dev/null || true)"
      command="$(tr "\\0" " " <"$process_dir/cmdline" 2>/dev/null || true)"
      echo "direct ryeosd child pid=$pid exe=$executable cmd=$command" >&2
    done
  ' sh "$DAEMON_PID" >&2 || true
  echo "long-running RyeOS runtime did not become active before shutdown" >&2
  exit 1
}

docker exec "$CONTAINER" chmod -R a+rwX /data/projects/container-knowledge/.ai/state
docker stop --time 30 "$CONTAINER" >/dev/null
wait "$SHUTDOWN_SMOKE_PID" >/dev/null 2>&1 || true
SHUTDOWN_SMOKE_PID=""
EXIT_CODE="$(docker inspect --format '{{.State.ExitCode}}' "$CONTAINER")"
[[ "$EXIT_CODE" == 0 ]] || {
  docker logs "$CONTAINER" >&2
  echo "container exited with status $EXIT_CODE after SIGTERM" >&2
  exit 1
}
docker logs "$CONTAINER" 2>&1 | grep -Eq 'daemon exiting.*signal|reason.?=.?signal.*daemon exiting' || {
  docker logs "$CONTAINER" >&2
  echo "RyeOS did not record a signal-driven graceful exit" >&2
  exit 1
}

echo "$VARIANT image qualification passed: $IMAGE_REF"
