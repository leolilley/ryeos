#!/usr/bin/env bash
# Rehearse a relay session's startup locally, with no provider and no spend.
#
# The controller is this tree's real start loop over a composed node; the
# guest is the real occurrence owner, supervisor and launcher, started in a
# local root enclosure by a rehearsal host; every relay file effect is a file
# move in that host's mailbox. See the module comment of
# crates/engine/ryeos-executor/src/execution/persistent_session/
# guest_relay_loopback_tests.rs for what each case asserts.
#
# Cases (all in the one test module, all run by the `run` step):
#   baseline Ready and candidate launch; corrupted attach response; controller
#   clock +-30 s (attaches) and -1 h (refused, the refusal's detail reaches the
#   start failure); early supervisor exit (the owner's report reaches the start
#   failure); and a report of the enforced isolation and descendant containment
#   this machine qualifies. The qualification (Q) session itself is not covered:
#   it needs an accepted qualification root that only the full operator flow
#   produces.
#
# Prerequisites
#   - Linux with unprivileged user, PID and mount namespaces (the cases skip
#     with a stated reason otherwise; RYEOS_GUEST_RELAY_LOOPBACK=1, set here,
#     turns a skip into a failure).
#   - Populate payloads (bin/, refs/, objects/ under bundles/{core,graph,
#     directive}/.ai) in this tree: the composed node loads them. Copy them
#     from a tree that has run scripts/populate-bundles.sh; never commit them.
#   - Guest executables MUST be static. The enclosure carries no loader or
#     libc, so a dynamically linked supervisor fails at exec with ENOENT.
#
# Usage
#   scripts/dev/rehearse-guest-relay-loopback.sh [guests|host|run|all] [test-filter]
# Steps: guests builds the three static guest executables; host builds the
# static rehearsal host; run runs the cases (default filter
# guest_relay_loopback_tests); all does the three in order. Environment:
#   CARGO               cargo command (default "cargo"; word-split, so it may
#                       name a wrapper such as a serializing guard script)
#   CARGO_TARGET_DIR    target directory (default "target" in this tree)
#   RYEOS_GUEST_RELAY_LOOPBACK_WORK
#                       private parent for session work (default /var/tmp)
#   REHEARSAL_OUT       where the host executable is copied (default
#                       $CARGO_TARGET_DIR/rehearsal-host)
set -euo pipefail

cd "$(dirname "$0")/../.."
step=${1:-all}
filter=${2:-guest_relay_loopback_tests}
CARGO=${CARGO:-cargo}
target_dir=${CARGO_TARGET_DIR:-target}
export CARGO_TARGET_DIR="$target_dir"
triple=x86_64-unknown-linux-gnu
static_flags="-C target-feature=+crt-static"
out=${REHEARSAL_OUT:-$target_dir/rehearsal-host}
guest_bin="$target_dir/$triple/release"
work=${RYEOS_GUEST_RELAY_LOOPBACK_WORK:-/var/tmp}

build_guests() {
    # The recipe the release image and scripts/populate-bundles.sh use for
    # every guest executable. An explicit --target keeps the static flags off
    # host-built proc macros and build scripts.
    RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$static_flags" \
        $CARGO build --release --locked --target "$triple" \
        -p ryeos-external-guest-occurrence-owner \
        -p ryeos-external-candidate-supervisor \
        -p ryeos-external-candidate-launcher
}

build_host() {
    # The host re-executes this very test binary inside the enclosure, so it
    # is built static as well. Its path is printed by cargo; keep a copy,
    # because a serializing wrapper may prune test executables afterwards.
    local line path
    line=$(RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }$static_flags" \
        $CARGO test --locked --target "$triple" --no-run --message-format=json \
        -p ryeos-external-execution --test guest_start_rehearsal_native \
        | python3 -I -c '
import json, sys
for text in sys.stdin:
    try:
        message = json.loads(text)
    except ValueError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable") \
            and message["target"]["name"] == "guest_start_rehearsal_native":
        print(message["executable"])
')
    path=$(printf '%s\n' "$line" | tail -n1)
    [ -x "$path" ] || { echo "no rehearsal host executable was produced" >&2; exit 1; }
    mkdir -p "$(dirname "$out")"
    cp "$path" "$out"
    echo "rehearsal host: $out"
}

run_cases() {
    for name in ryeos-external-guest-occurrence-owner ryeos-external-candidate-supervisor \
            ryeos-external-candidate-launcher; do
        [ -x "$guest_bin/$name" ] || { echo "$guest_bin/$name is not built (run: $0 guests)" >&2; exit 1; }
    done
    [ -x "$out" ] || { echo "$out is not built (run: $0 host)" >&2; exit 1; }
    RYEOS_GUEST_RELAY_LOOPBACK=1 \
        RYEOS_GUEST_RELAY_LOOPBACK_HOST="$(realpath "$out")" \
        RYEOS_GUEST_RELAY_LOOPBACK_BIN_DIR="$(realpath "$guest_bin")" \
        RYEOS_GUEST_RELAY_LOOPBACK_WORK="$work" \
        $CARGO test --locked -p ryeos-executor --lib "$filter" -- --test-threads=1 --nocapture
}

case "$step" in
    guests) build_guests ;;
    host) build_host ;;
    run) run_cases ;;
    all) build_guests; build_host; run_cases ;;
    *) echo "usage: $0 [guests|host|run|all] [test-filter]" >&2; exit 2 ;;
esac
