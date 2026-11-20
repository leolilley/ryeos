#!/usr/bin/env bash
# Restore the bundle state of a recent `next` CI run into this checkout (a branch
# under test), refusing when that would hide branch changes. Used by
# remote-cargo.yml (bundles=next) and by the UI-only merge gate in ci.yml.
#
# Finds the newest ci.yml push run on next whose "Bundle refresh for tests" job
# succeeded and whose bundle-state artifact has not expired, downloads it, and
# unpacks it with unpack-bundle-state.sh.
# Overwrite-safety rule: the bundle state is a snapshot of next's tree plus the
# refresh output. Let S be the next commit the state came from and HEAD the
# branch under test. The restore is refused (exit 1) when any path the state
# would write (modified tracked or untracked/ignored files) or delete differs
# between S and HEAD, because unpacking would silently replace the branch's
# version of that file with next's. Untracked/ignored generated files
# (populated bin trees, signatures) never differ in git, so they restore freely.
# It is also refused when anything under bundles/ differs between S and HEAD,
# even a tracked file the refresh did not modify: the state's bins and
# signatures were produced from S's bundle sources, so a branch with other
# bundle sources would load a mismatched, partly stale bundle. If it refuses
# because the branch is merely behind next, merge origin/next (the newest next
# run whose bundles job has finished is used, so wait for it if next just moved).
# Branch changes to crate sources can make the prebuilt bundle bins stale
# relative to the branch; that only warns.
#
# Needs GITHUB_REPOSITORY and GH_TOKEN; run from the repository root.

set -euo pipefail

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

run_id=""
sha=""
runs="$(gh api "repos/$repo/actions/workflows/ci.yml/runs?branch=next&event=push&per_page=30" \
    --jq '.workflow_runs[] | "\(.id) \(.head_sha)"')"
while read -r id head; do
    [[ -n "$id" ]] || continue
    ok="$(gh api "repos/$repo/actions/runs/$id/jobs?per_page=100" \
        --jq '[.jobs[] | select(.name == "Bundle refresh for tests" and .conclusion == "success")] | length')"
    [[ "$ok" -gt 0 ]] || continue
    live="$(gh api "repos/$repo/actions/runs/$id/artifacts?per_page=100" \
        --jq "[.artifacts[] | select(.name == \"bundle-state-$head\" and (.expired | not))] | length")"
    if [[ "$live" -gt 0 ]]; then
        run_id="$id"
        sha="$head"
        break
    fi
done <<<"$runs"

if [[ -z "$run_id" ]]; then
    echo "::error::No unexpired bundle state found: none of the last 30 ci.yml runs on next has a successful bundles job with a live artifact (retention is 1 day). Wait for a next CI run to finish its bundles job, or run with bundles=none."
    exit 1
fi
echo "Bundle state comes from next $sha (ci.yml run $run_id)"
echo "https://github.com/$repo/actions/runs/$run_id"

gh run download "$run_id" --repo "$repo" -n "bundle-state-$sha" -D "$work/dl"
state="$work/dl/bundle-state.tar.zst"

# Paths the state would write or delete.
tar --zstd -tf "$state" | grep -v -e '^\.ci-deleted-files$' -e '/$' | sort -u > "$work/state-paths" || true
tar --zstd -xOf "$state" .ci-deleted-files | sort -u > "$work/deleted-paths"
sort -u "$work/state-paths" "$work/deleted-paths" > "$work/touched"

git fetch --no-tags --depth=1 origin "$sha"
git diff --name-only "$sha" HEAD | sort -u > "$work/differs"

{ comm -12 "$work/touched" "$work/differs"; grep '^bundles/' "$work/differs" || true; } | sort -u > "$work/conflicts"
conflicts="$(cat "$work/conflicts")"
if [[ -n "$conflicts" ]]; then
    echo "::error::Refusing to restore next's bundle state: this branch differs from next $sha on paths the state would overwrite or delete, or on bundle sources the state was built from, so the run would test next's version, not the branch's. Merge origin/next into the branch, or keep branch edits out of these paths."
    while IFS= read -r path; do echo "  conflict: $path"; done <<<"$conflicts"
    exit 1
fi

if grep -E -q '^(crates/|Cargo\.lock$|Cargo\.toml$)' "$work/differs"; then
    echo "::warning::This branch differs from next $sha in crate sources; the prebuilt bundle bins were built from next and may not reflect those changes."
fi

./scripts/ci/unpack-bundle-state.sh "$state"
{
    echo "### Bundle state"
    echo "next \`$sha\`, ci.yml run [$run_id](https://github.com/$repo/actions/runs/$run_id); $(wc -l < "$work/touched") paths; no branch overlap."
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
