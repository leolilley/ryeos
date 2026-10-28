#!/usr/bin/env bash
# Reproduce a bundle refresh's changes from pack-bundle-state.sh output in a
# fresh checkout of the same commit.
#
# Usage: unpack-bundle-state.sh INPUT.tar.zst   (run at the repository root)
set -euo pipefail

in="${1:?usage: unpack-bundle-state.sh INPUT.tar.zst}"
# The E2E support judges prepared bundles stale when any bundle file, crate
# source or manifest is newer than the published manifest ref. A fresh checkout
# stamps every tracked file with the checkout time, later than the refresh that
# ran in another job, so give tracked files an old time first. Files the
# refresh changed come back below with the mtimes they had when it wrote them,
# which keeps the ref newest exactly as it was after the refresh.
git ls-files -z | xargs -0 touch -h -d '2000-01-01 00:00:00 UTC'
tar --zstd -xf "$in"
if [[ -f .ci-deleted-files ]]; then
    while IFS= read -r path; do
        [[ -z "$path" ]] || rm -f -- "$path"
    done < .ci-deleted-files
    rm -f .ci-deleted-files
fi
