#!/usr/bin/env bash
# Capture what a bundle refresh changed in the checkout so other jobs can
# reproduce it without rebuilding: every untracked or ignored file and every
# modified tracked file (outside target/), plus the list of deleted tracked
# files, as one zstd-compressed tarball.
#
# Usage: pack-bundle-state.sh OUTPUT.tar.zst   (run at the repository root)
set -euo pipefail

out="${1:?usage: pack-bundle-state.sh OUTPUT.tar.zst}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Without --exclude-standard, --others also lists ignored files.
git ls-files -z --others --modified -- . ':(exclude)target' | sort -zu > "$work/present"
# A path that is modified and deleted is listed by both commands; keep only
# what still exists in the tree.
: > "$work/files"
while IFS= read -r -d '' path; do
    if [[ -e "$path" || -L "$path" ]]; then
        printf '%s\0' "$path" >> "$work/files"
    fi
done < "$work/present"

git ls-files --deleted > "$work/.ci-deleted-files"
tar --null --files-from="$work/files" -cf "$work/state.tar"
tar -rf "$work/state.tar" -C "$work" .ci-deleted-files
zstd -q -T0 -3 -o "$out" "$work/state.tar"
printf 'packed %s files, %s deletions, %s bytes\n' \
    "$(tr -cd '\0' < "$work/files" | wc -c)" "$(wc -l < "$work/.ci-deleted-files")" "$(stat -c %s "$out")"
