#!/usr/bin/env bash
# Stage one already-built static verifier into a new unsigned Bundle source.
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
triple=x86_64-unknown-linux-gnu

if [[ $# -ne 2 || $1 != /* || $2 != /* ]]; then
  printf 'usage: %s <absolute-verifier-binary> <new-absolute-bundle-overlay>\n' "$0" >&2
  exit 64
fi
binary=$1
destination=$2
if [[ ! -f $binary || -L $binary || ! -x $binary ]]; then
  printf 'fixture verifier is not an executable regular file: %s\n' "$binary" >&2
  exit 66
fi
if [[ -e $destination ]]; then
  printf 'fixture Bundle destination already exists: %s\n' "$destination" >&2
  exit 73
fi

python3 "$fixture_root/check-static-elf.py" "$binary"
mkdir -p -- "$destination"
cp -a -- "$fixture_root/bundle-overlay/." "$destination/"
mkdir -p -- "$destination/.ai/bin/$triple"
install -m 0555 -- "$binary" \
  "$destination/.ai/bin/$triple/fixture-dynamic-product-verifier"
sha256sum -- "$destination/.ai/bin/$triple/fixture-dynamic-product-verifier"
printf '%s\n' \
  "prepared unsigned unknown-output Bundle overlay at $destination" \
  'normal Bundle population/signing, installation, and execution were not run'
