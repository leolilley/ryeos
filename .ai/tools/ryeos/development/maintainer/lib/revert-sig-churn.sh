#!/usr/bin/env bash
# ryeos:signed:2026-11-13T00:09:46Z:40219622e2f7c9c6aa45a21a94bf86ebe59f9700306e0c27e17124de5e15436e:NgEhAniorOy9xb5APW//nw9Enk0HchYXNe12jD2IJvBhQequ/9XoNXTQYg/baHRAPIyw4IENiUvU0EzoIkDpAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Revert files whose ONLY git-tracked change is the `ryeos:signed:` line(s) —
# the timestamp/signature churn that `populate-bundles.sh` / a local install
# leaves on otherwise-unchanged bundle items. Files with real content changes
# are left untouched.
#
# Usage: .ai/tools/ryeos/development/maintainer/lib/revert-sig-churn.sh [<pathspec> ...]   (default: bundles/)
# Flags: --dry-run   list what would be reverted without touching anything
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../../../../.." && pwd)"
cd "$ROOT"

dry_run=0
paths=()
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    *) paths+=("$arg") ;;
  esac
done
[[ ${#paths[@]} -gt 0 ]] || paths=("bundles/")

reverted=0
kept=0
while IFS= read -r file; do
  [[ -n "$file" ]] || continue
  # Added/removed content lines, excluding the diff file headers.
  changed="$(git diff -- "$file" | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' || true)"
  [[ -n "$changed" ]] || continue
  # Any changed line that is NOT a signature line means real content changed.
  nonsig="$(printf '%s\n' "$changed" | grep -vE 'ryeos:signed:' || true)"
  if [[ -z "$nonsig" ]]; then
    if [[ "$dry_run" == 1 ]]; then
      echo "would revert (sig-only): $file"
    else
      git checkout -- "$file"
      echo "reverted (sig-only): $file"
    fi
    reverted=$((reverted + 1))
  else
    kept=$((kept + 1))
  fi
done < <(git diff --name-only -- "${paths[@]}")

echo "sig-only: $reverted; real-change (kept): $kept"
