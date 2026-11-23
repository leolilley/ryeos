#!/usr/bin/env bash
# Restores the bundle state of the newest next CI push run (of the last 50) whose
# artifact is retained and whose commit has no bundles/ difference from this
# branch and no difference on any path the state would write or delete. Newer
# next commits the branch differs from are skipped, so the branch is never
# tested against next's bundle files; the merge base alone is not used. At most
# 5 artifacts are downloaded. When none qualifies (artifacts live 1 day) or the
# API stays unavailable, it exits 1 and the caller refreshes the bundles from
# the branch. The job summary names the next commit used, or that the fallback ran.
set -euo pipefail
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

# Next commits are fetched shallowly into a separate throwaway repository, never
# into the checkout's own .git. It borrows the checkout's objects (alternates) so
# the branch head can be diffed against the fetched commit there.
scratch="$work/scratch.git"
git init -q --bare "$scratch"
printf '%s\n' "$(git rev-parse --absolute-git-dir)/objects" > "$scratch/objects/info/alternates"
head_sha="$(git rev-parse HEAD)"
remote_url="https://github.com/${repo}.git"
auth_header="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 -w0)"
echo "::add-mask::${auth_header#AUTHORIZATION: basic }"

# gh api with backoff: a rate limit or a transient error is retried a few times
# with growing sleeps, never in a tight loop.
gh_retry() {
    local attempt delay=20
    for attempt in 1 2 3 4; do
        if gh api "$@" 2>"$work/api-err"; then
            return 0
        fi
        echo "::warning::gh api $1 failed (attempt $attempt of 4): $(head -c 300 "$work/api-err")" >&2
        if [[ "$attempt" -lt 4 ]]; then
            sleep "$delay"
            delay=$((delay * 2))
        fi
    done
    return 1
}

# Ends the step with failure, which runs the branch refresh in the next step.
fallback() {
    echo "::warning::Bundle refresh fallback: $1"
    {
        echo "### Bundle state"
        echo "No next bundle state was restored: $1. The bundles were refreshed from this branch."
    } >> "$summary"
    exit 1
}

runs="$(gh_retry "repos/$repo/actions/workflows/ci.yml/runs?branch=next&event=push&per_page=50" \
    --jq '.workflow_runs[] | "\(.id) \(.head_sha)"')" \
    || fallback "the list of next CI runs could not be read"

# Newest first. A next commit's state is usable only when this branch has no
# bundles/ difference from that commit and no difference on any path the state
# would write or delete; otherwise the run would test next's files, not the
# branch's, so the search continues with the next older commit. The merge base
# alone is never used as the comparison.
run_id=""
sha=""
considered=0
downloads=0
skipped=""
while read -r id head; do
    [[ -n "$id" ]] || continue
    considered=$((considered + 1))
    live="$(gh_retry "repos/$repo/actions/runs/$id/artifacts?name=bundle-state-$head&per_page=100" \
        --jq '[.artifacts[] | select(.expired | not)] | length')" \
        || fallback "the artifact list of ci.yml run $id could not be read after retries"
    if [[ "$live" -eq 0 ]]; then
        echo "next $head (run $id): no retained bundle state"
        continue
    fi
    if ! git --git-dir="$scratch" -c "http.https://github.com/.extraheader=$auth_header" fetch -q --no-tags --depth=1 "$remote_url" "$head" 2>/dev/null; then
        echo "next $head (run $id): could not fetch the commit, skipped"
        continue
    fi
    git --git-dir="$scratch" diff --name-only "$head" "$head_sha" | sort -u > "$work/differs"
    if grep -q '^bundles/' "$work/differs"; then
        echo "next $head (run $id): branch differs under bundles/, skipped"
        skipped="$skipped $head"
        continue
    fi
    if [[ "$downloads" -ge 5 ]]; then
        echo "next $head (run $id): download cap of 5 reached, stopping the search"
        break
    fi
    downloads=$((downloads + 1))
    rm -rf "$work/dl"
    if ! gh run download "$id" --repo "$repo" -n "bundle-state-$head" -D "$work/dl"; then
        echo "::warning::next $head (run $id): the bundle state artifact could not be downloaded (expired or rate limited), skipped"
        continue
    fi
    state="$work/dl/bundle-state.tar.zst"
    # Paths the state would write or delete.
    tar --zstd -tf "$state" | grep -v -e '^\.ci-deleted-files$' -e '/$' | sort -u > "$work/state-paths" || true
    tar --zstd -xOf "$state" .ci-deleted-files | sort -u > "$work/deleted-paths"
    sort -u "$work/state-paths" "$work/deleted-paths" > "$work/touched"
    comm -12 "$work/touched" "$work/differs" > "$work/conflicts"
    if [[ -s "$work/conflicts" ]]; then
        echo "next $head (run $id): branch differs on paths the state would overwrite or delete, skipped:"
        sed 's/^/  conflict: /' "$work/conflicts"
        skipped="$skipped $head"
        continue
    fi
    run_id="$id"
    sha="$head"
    break
done <<<"$runs"

if [[ -z "$run_id" ]]; then
    fallback "none of the $considered newest ci.yml push runs on next has a retained bundle state compatible with this branch (retention is 1 day)"
fi

echo "Bundle state comes from next $sha (ci.yml run $run_id)"
echo "https://github.com/$repo/actions/runs/$run_id"

if grep -E -q '^(crates/|Cargo\.lock$|Cargo\.toml$)' "$work/differs"; then
    echo "::warning::This branch differs from next $sha in crate sources; the prebuilt bundle bins were built from next and may not reflect those changes."
fi

./scripts/ci/unpack-bundle-state.sh "$state"
{
    echo "### Bundle state"
    echo "Restored from next \`$sha\`, ci.yml run [$run_id](https://github.com/$repo/actions/runs/$run_id); $(wc -l < "$work/touched") paths; no bundles/ difference and no overlap with the branch."
    if [[ -n "$skipped" ]]; then
        echo "Newer next commits skipped because the branch differs from them:$skipped"
    fi
} >> "$summary"
