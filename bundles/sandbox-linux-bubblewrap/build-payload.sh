#!/usr/bin/env bash
set -euo pipefail

# Optional example-bundle authoring helper. RyeOS never invokes this script;
# use it only when deliberately building the Bubblewrap demonstration bundle.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TRIPLE="${TRIPLE:-x86_64-unknown-linux-gnu}"
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
OUTPUT_DIR="$ROOT/bundles/sandbox-linux-bubblewrap/.ai/bin/$TRIPLE"
BWRAP_OUTPUT="$OUTPUT_DIR/bwrap"
ADAPTER_MANIFEST="$ROOT/bundles/sandbox-linux-bubblewrap/adapter/Cargo.toml"

bwrap_compatible() {
    local executable="$1"
    local output major minor help dynamic
    output="$("$executable" --version 2>/dev/null)" || return 1
    [[ "$output" =~ ^bubblewrap[[:space:]]([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || return 1
    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    if (( 10#$major == 0 && 10#$minor < 11 )); then
        return 1
    fi
    help="$("$executable" --help 2>&1)" || return 1
    for option in --bind-fd --ro-bind-fd --argv0; do
        grep -Eq "(^|[[:space:]])${option}([[:space:]]|$)" <<<"$help" || return 1
    done
    dynamic="$(readelf -d "$executable" 2>/dev/null)" || return 1
    ! grep -Eq 'Shared library: \[libcap\.so' <<<"$dynamic"
}

mkdir -p "$OUTPUT_DIR"
cargo build --release --manifest-path "$ADAPTER_MANIFEST" --target-dir "$TARGET"
install -m 0755 "$TARGET/release/ryeos-bubblewrap-adapter" "$OUTPUT_DIR/"

if [[ -x "$BWRAP_OUTPUT" ]] && bwrap_compatible "$BWRAP_OUTPUT"; then
    exit 0
fi

version=0.11.2
archive="bubblewrap-${version}.tar.xz"
source_url="https://github.com/containers/bubblewrap/releases/download/v${version}/${archive}"
expected_sha256=69abc30005d2186baf7737feacd8da35633b93cf5af38838ecff17c5f8e924f6
source_dir="${RUNNER_TEMP:-/tmp}/bubblewrap-${version}"
build_dir="${RUNNER_TEMP:-/tmp}/bubblewrap-${version}-build"
archive_path="${RUNNER_TEMP:-/tmp}/${archive}"

curl --fail --location --proto '=https' --tlsv1.2 \
    --output "$archive_path" "$source_url"
printf '%s  %s\n' "$expected_sha256" "$archive_path" | sha256sum --check --status
rm -rf "$source_dir" "$build_dir"
tar --extract --file "$archive_path" --directory "${RUNNER_TEMP:-/tmp}"
meson setup "$build_dir" "$source_dir" \
    --prefix=/usr \
    -Dprefer_static=true \
    -Dbash_completion=disabled \
    -Dzsh_completion=disabled \
    -Dman=disabled \
    -Dselinux=disabled \
    -Dsupport_setuid=false \
    -Dtests=false
meson compile -C "$build_dir"
install -m 0755 "$build_dir/bwrap" "$BWRAP_OUTPUT"

bwrap_compatible "$BWRAP_OUTPUT"
