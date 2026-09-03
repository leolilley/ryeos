#!/usr/bin/env bash
set -euo pipefail

# Backend-bundle authoring helper. RyeOS never invokes this script at runtime;
# the operator runs it before publishing this independently installed bundle.
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$HOST_TRIPLE" ]] || {
    echo "could not determine the Rust host target" >&2
    exit 1
}
TRIPLE="${TRIPLE:-$HOST_TRIPLE}"
[[ "$TRIPLE" == "$HOST_TRIPLE" ]] || {
    echo "cross-compilation is not supported: requested $TRIPLE on $HOST_TRIPLE" >&2
    exit 1
}
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
OUTPUT_DIR="$ROOT/bundles/sandbox-linux-bubblewrap/.ai/bin/$TRIPLE"
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
    for option in --bind-fd --ro-bind-fd --ro-bind-data --argv0 --overlay-src --overlay; do
        grep -Eq "(^|[[:space:]])${option}([[:space:]]|$)" <<<"$help" || return 1
    done
    dynamic="$(readelf -d "$executable" 2>/dev/null)" || return 1
    ! grep -Eq 'Shared library: \[libcap\.so' <<<"$dynamic"
}

fully_static() {
    local executable="$1"
    ! readelf -l "$executable" 2>/dev/null | grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' \
        && ! readelf -d "$executable" 2>/dev/null | grep -Eq 'NEEDED'
}

BUILD_ROOT="$(mktemp -d "${RUNNER_TEMP:-/tmp}/ryeos-bwrap-build.XXXXXX")"
PAYLOAD_DIR="$BUILD_ROOT/payload"
mkdir -p "$PAYLOAD_DIR"
trap 'rm -rf -- "$BUILD_ROOT"' EXIT

RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static" \
    cargo build --release --manifest-path "$ADAPTER_MANIFEST" --target "$TRIPLE" --target-dir "$TARGET"
install -m 0755 "$TARGET/$TRIPLE/release/ryeos-bubblewrap-adapter" "$PAYLOAD_DIR/"
fully_static "$PAYLOAD_DIR/ryeos-bubblewrap-adapter"

libcap_version=2.78
libcap_archive="libcap-${libcap_version}.tar.xz"
libcap_url="https://www.kernel.org/pub/linux/libs/security/linux-privs/libcap2/${libcap_archive}"
libcap_sha256=0d621e562fd932ccf67b9660fb018e468a683d7b827541df27813228c996bb11
libcap_archive_path="$BUILD_ROOT/$libcap_archive"
curl --fail --location --proto '=https' --tlsv1.2 \
    --output "$libcap_archive_path" "$libcap_url"
printf '%s  %s\n' "$libcap_sha256" "$libcap_archive_path" | sha256sum --check --status
tar --extract --file "$libcap_archive_path" --directory "$BUILD_ROOT"
libcap_prefix="$BUILD_ROOT/libcap-prefix"
make -C "$BUILD_ROOT/libcap-${libcap_version}/libcap" \
    SHARED=no PTHREADS=no GOLANG=no prefix="$libcap_prefix" lib=lib install-static

version=0.11.2
archive="bubblewrap-${version}.tar.xz"
source_url="https://github.com/containers/bubblewrap/releases/download/v${version}/${archive}"
expected_sha256=69abc30005d2186baf7737feacd8da35633b93cf5af38838ecff17c5f8e924f6
source_dir="$BUILD_ROOT/bubblewrap-${version}"
build_dir="$BUILD_ROOT/bubblewrap-build"
archive_path="$BUILD_ROOT/$archive"

curl --fail --location --proto '=https' --tlsv1.2 \
    --output "$archive_path" "$source_url"
printf '%s  %s\n' "$expected_sha256" "$archive_path" | sha256sum --check --status
tar --extract --file "$archive_path" --directory "$BUILD_ROOT"
PKG_CONFIG_PATH="$libcap_prefix/lib/pkgconfig" meson setup "$build_dir" "$source_dir" \
    --prefix=/usr \
    -Dprefer_static=true \
    -Dc_link_args=-static \
    --wrap-mode=nodownload \
    -Dbash_completion=disabled \
    -Dzsh_completion=disabled \
    -Dman=disabled \
    -Dselinux=disabled \
    -Dsupport_setuid=false \
    -Dtests=false
meson compile -C "$build_dir"
BWRAP_OUTPUT="$PAYLOAD_DIR/bwrap"
install -m 0755 "$build_dir/bwrap" "$BWRAP_OUTPUT"

bwrap_compatible "$BWRAP_OUTPUT"
fully_static "$BWRAP_OUTPUT"

# Publish only a completely validated pair. A failed adapter/Bubblewrap build
# leaves the prior authoring payload untouched instead of exposing a mixed
# local bundle tree for a later signing command.
OUTPUT_PARENT="$(dirname "$OUTPUT_DIR")"
OUTPUT_BACKUP="$OUTPUT_PARENT/.payload-previous.$$"
mkdir -p "$OUTPUT_PARENT"
if [[ -e "$OUTPUT_BACKUP" ]]; then
    echo "refusing unexpected sandbox payload backup: $OUTPUT_BACKUP" >&2
    exit 1
fi
if [[ -e "$OUTPUT_DIR" ]]; then
    mv "$OUTPUT_DIR" "$OUTPUT_BACKUP"
fi
if mv "$PAYLOAD_DIR" "$OUTPUT_DIR"; then
    rm -rf -- "$OUTPUT_BACKUP"
else
    if [[ -e "$OUTPUT_BACKUP" ]]; then
        mv "$OUTPUT_BACKUP" "$OUTPUT_DIR"
    fi
    exit 1
fi
