#!/usr/bin/env bash
# Build the disposable fixture verifier without changing a Bundle or node.
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
program=$fixture_root/verifier-program
triple=x86_64-unknown-linux-gnu

if [[ $# -ne 1 || $1 != /* ]]; then
  printf 'usage: %s <new-absolute-output-binary>\n' "$0" >&2
  exit 64
fi
output=$1
if [[ -e $output ]]; then
  printf 'fixture verifier output already exists: %s\n' "$output" >&2
  exit 73
fi
if [[ ! -f $program/Cargo.lock ]]; then
  printf '%s\n' 'fixture verifier Cargo.lock is absent; generate and review it offline before building' >&2
  exit 66
fi
for dependency in cargo python3; do
  command -v -- "$dependency" >/dev/null 2>&1 || {
    printf 'missing fixture verifier build dependency: %s\n' "$dependency" >&2
    exit 77
  }
done

target_dir=$(mktemp -d)
trap 'rm -rf -- "$target_dir"' EXIT
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static --remap-path-prefix=$fixture_root=/fixture" \
  CARGO_TARGET_DIR=$target_dir \
  cargo build \
    --manifest-path "$program/Cargo.toml" \
    --locked --frozen --offline --jobs 2 --release --target "$triple"

built=$target_dir/$triple/release/fixture-dynamic-product-verifier
[[ -f $built && ! -L $built ]] || {
  printf '%s\n' 'fixture verifier build produced no regular binary' >&2
  exit 70
}
install -m 0555 -- "$built" "$output"
python3 "$fixture_root/check-static-elf.py" "$output"
printf 'built offline static fixture verifier %s\n' "$output"
