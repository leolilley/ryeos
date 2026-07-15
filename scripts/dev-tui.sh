#!/usr/bin/env bash
# Dev fast-path for the native TUI: build from this checkout and launch
# the built binary directly.
#
# `ryeos tui` is the PACKAGED path: it dispatches the signed item
# `client:ryeos/tui`, whose `binary_ref: bin/{triple}/ryeos-tui` resolves a
# content-addressed bundle payload. That payload only updates through
# `populate-bundles.sh` + `install-local-direct.sh` — correct for
# acceptance, far too heavy for visual iteration, and hand-copying
# binaries into bundle trees breaks manifest hash verification (by
# design).
#
# This script is the iteration path: source -> target/{profile}/ryeos-tui
# -> normal daemon surface resolution. Everything downstream of launch is
# unchanged: the binary still resolves the surface through the running
# daemon with the same project path.
#
# Packaged acceptance afterwards:
#   scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev --all
#   scripts/pkg/install-local-direct.sh --trust-source-publishers
#   ryeos tui

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/dev-tui.sh [SURFACE_REF] [options]

Build ryeos-client-terminal from this checkout and run it directly,
bypassing the packaged client:ryeos/tui bundle-binary dispatch.

Arguments:
  SURFACE_REF       Surface to open (default: surface:ryeos/ui/lens)

Options:
  --local           Load the surface AND its views from THIS CHECKOUT's
                    bundles/ryeos-ui tree instead of the daemon's installed
                    items — the content-iteration path: edit a view/surface
                    YAML, rerun, see it. No populate, no install, no
                    re-sign needed (the preview reads files directly).
                    Live data (sources, input routing) still needs a
                    running daemon and only knows its installed services.
  --release         Build and run the release profile (default: debug)
  --project PATH    Project root for daemon-backed resolution (default: $PWD)
  --read-only       Open a read-only seat
  --no-build        Skip the cargo build, run the existing binary
  -h, --help        Show this help

Examples:
  scripts/dev-tui.sh
  scripts/dev-tui.sh --local                 # iterate on backdrop/view content
  scripts/dev-tui.sh --local --no-build      # content-only loop, instant
  scripts/dev-tui.sh surface:ryeos/ui/workbench --release
EOF
}

# Default to the single-lens cell-grid home surface: one center lens at a
# time (the cognition feed), swapped via the launcher. Needs a
# `populate-bundles --all` to sign + resolve it; pass `surface:ryeos/ui/base`
# explicitly for the web-style tiled surface.
SURFACE="surface:ryeos/ui/lens"
PROFILE="debug"
PROJECT="$PWD"
READ_ONLY=0
BUILD=1
LOCAL=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --local) LOCAL=1; shift ;;
        --release) PROFILE="release"; shift ;;
        --project) PROJECT="$2"; shift 2 ;;
        --read-only) READ_ONLY=1; shift ;;
        --no-build) BUILD=0; shift ;;
        -h|--help) usage; exit 0 ;;
        surface:*) SURFACE="$1"; shift ;;
        -*) echo "dev-tui.sh: unknown option: $1" >&2; usage >&2; exit 2 ;;
        *) echo "dev-tui.sh: expected a surface: ref, got: $1" >&2; exit 2 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if [[ "$BUILD" -eq 1 ]]; then
    if [[ "$PROFILE" == "release" ]]; then
        cargo build --release -p ryeos-client-terminal
    else
        cargo build -p ryeos-client-terminal
    fi
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BIN="$TARGET_DIR/$PROFILE/ryeos-tui"
if [[ ! -x "$BIN" ]]; then
    echo "dev-tui.sh: built binary not found at $BIN" >&2
    exit 1
fi

if [[ "$LOCAL" -eq 1 ]]; then
    # Map `surface:ryeos/ui/<name>` onto this checkout's surface file
    # and point view resolution at the checkout's view tree. The daemon
    # (if running) still serves whatever the tree doesn't carry, plus all
    # live data.
    SURFACE_FILE="$REPO_ROOT/bundles/ryeos-ui/.ai/surfaces/${SURFACE#surface:}.yaml"
    [[ -f "$SURFACE_FILE" ]] || { echo "dev-tui.sh: no local surface file at $SURFACE_FILE" >&2; exit 1; }
    ARGS=(--surface-file "$SURFACE_FILE" --views-root "$REPO_ROOT/bundles/ryeos-ui/.ai/views" --project "$PROJECT")
else
    ARGS=(--surface "$SURFACE" --project "$PROJECT")
fi
if [[ "$READ_ONLY" -eq 1 ]]; then
    ARGS+=(--read-only)
fi

echo "dev-tui: $BIN ${ARGS[*]}" >&2
exec "$BIN" "${ARGS[@]}"
