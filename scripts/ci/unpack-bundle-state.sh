#!/usr/bin/env bash
# Reproduce a bundle refresh's changes from pack-bundle-state.sh output in a
# fresh checkout of the same commit.
#
# Usage: unpack-bundle-state.sh INPUT.tar.zst   (run at the repository root)
set -euo pipefail

in="${1:?usage: unpack-bundle-state.sh INPUT.tar.zst}"
tar --zstd -xf "$in"
if [[ -f .ci-deleted-files ]]; then
    while IFS= read -r path; do
        [[ -z "$path" ]] || rm -f -- "$path"
    done < .ci-deleted-files
    rm -f .ci-deleted-files
fi
