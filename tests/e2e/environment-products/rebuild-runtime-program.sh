#!/usr/bin/env bash
# Reproduce the checked fixture executable with the current GNU x86-64 test
# toolchain. This is a source-maintenance helper, not part of node population.
set -euo pipefail

fixture_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
expected=3a55c0c7914fdeccc41ac91789116675ca07befdc1c64d8c4b1b77c4b7296d77

if [[ $# -ne 2 || $1 != /* || $2 != /* ]]; then
  printf 'usage: %s <new-absolute-binary> <new-absolute-base64>\n' "$0" >&2
  exit 64
fi
binary=$1
encoded=$2
if [[ -e $binary || -e $encoded ]]; then
  printf '%s\n' 'runtime fixture outputs must both be new paths' >&2
  exit 73
fi
for dependency in base64 cc sha256sum; do
  command -v -- "$dependency" >/dev/null 2>&1 || {
    printf 'missing runtime fixture build dependency: %s\n' "$dependency" >&2
    exit 77
  }
done

cc -nostdlib -static -fno-stack-protector -fno-pie -no-pie -fno-builtin \
  -Os -s -Wl,--build-id=none \
  -o "$binary" "$fixture_root/runtime-program.c"
observed=$(sha256sum -- "$binary")
observed=${observed%% *}
if [[ $observed != "$expected" ]]; then
  printf 'runtime fixture toolchain produced unexpected bytes: %s\n' "$observed" >&2
  exit 65
fi
base64 -w76 -- "$binary" > "$encoded"
cmp -- "$encoded" "$fixture_root/runtime-program.b64"
printf 'reproduced fixture runtime %s at %s\n' "$expected" "$binary"
