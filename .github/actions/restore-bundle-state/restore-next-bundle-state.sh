#!/usr/bin/env bash
# Restores the newest retained bundle-state artifact (any workflow event or
# branch) whose commit is on next, has no bundles/ difference from this branch
# and no difference on any path the state would write or delete. Next commits
# the branch differs from are skipped, so the branch is never tested against
# next's bundle files; the merge base alone is not used. At most 5 artifacts are
# downloaded. When none qualifies or the API stays unavailable, it exits 1 and
# the caller refreshes the bundles from the branch. The job summary names the
# next commit used, or that the fallback ran.
set -euo pipefail
export LC_ALL=C

if [[ "${GITHUB_ACTIONS:-}" != true ]]; then
    echo "restore-next-bundle-state.sh only runs inside GitHub Actions (GITHUB_ACTIONS=true): it unpacks a bundle state over the working tree and fetches next commits. Refusing to run." >&2
    exit 2
fi

repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GH_TOKEN:?GH_TOKEN is required}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
summary="${GITHUB_STEP_SUMMARY:-/dev/null}"

# Next commits are fetched shallowly into a separate throwaway repository, never
# into the checkout's own .git. The scratch repository shares no objects with the
# checkout: the branch and the candidate are compared by their listed trees.
scratch="$work/scratch.git"
git init -q --bare "$scratch"
remote_url="https://github.com/${repo}.git"
auth_header="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$GH_TOKEN" | base64 -w0)"
auth_b64="${auth_header#AUTHORIZATION: basic }"
echo "::add-mask::${auth_b64}"
git ls-tree -r -z HEAD | sort -z > "$work/head-tree"

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

# Candidates: every retained bundle-state artifact of this repository, from any
# workflow event and branch (full runs of next are sometimes dispatched from a
# branch pointing at a next commit), newest upload first. A candidate counts only
# when its commit is on next (compare says identical or behind).
: > "$work/artifacts"
for page in 1 2 3; do
    gh_retry "repos/$repo/actions/artifacts?per_page=100&page=$page" \
        --jq '.artifacts[] | select((.name | startswith("bundle-state-")) and (.expired | not) and .name == "bundle-state-\(.workflow_run.head_sha)") | "\(.created_at) \(.workflow_run.id) \(.workflow_run.head_sha) \(.workflow_run.head_branch)"' \
        >> "$work/artifacts" \
        || fallback "the artifact list could not be read"
done
sort -r -u "$work/artifacts" > "$work/runs"
echo "$(wc -l < "$work/runs") retained bundle-state artifacts found"

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
while read -r created id head branch; do
    [[ -n "$id" ]] || continue
    considered=$((considered + 1))
    [[ "$considered" -le 40 ]] || { echo "candidate limit of 40 reached"; break; }
    where="$head (run $id, branch $branch)"
    rel="$(gh_retry "repos/$repo/compare/next...$head" --jq .status)" \
        || fallback "the next ancestry of $head could not be read after retries"
    if [[ "$rel" != behind && "$rel" != identical ]]; then
        echo "$where: not a next commit ($rel), skipped"
        continue
    fi
    if ! git --git-dir="$scratch" -c "http.https://github.com/.extraheader=$auth_header" fetch -q --no-tags --depth=1 "$remote_url" "$head" 2>"$work/fetch-err"; then
        echo "$where: could not fetch the commit, skipped: $(head -c 400 "$work/fetch-err" | sed -e "s/$auth_b64/***/g" -e "s/${GH_TOKEN}/***/g" | tr '\n' ' ')"
        continue
    fi
    git --git-dir="$scratch" ls-tree -r -z "$head" | sort -z > "$work/cand-tree"
    # A path whose entry (mode, type, object) differs, or exists on one side only.
    sort -z "$work/cand-tree" "$work/head-tree" | uniq -z -u | cut -z -f2 | sort -z -u | tr '\0' '\n' > "$work/differs"
    if grep -q '^bundles/' "$work/differs"; then
        echo "$where: branch differs under bundles/, skipped"
        skipped="$skipped $head"
        continue
    fi
    if [[ "$downloads" -ge 5 ]]; then
        echo "$where: download cap of 5 reached, stopping the search"
        break
    fi
    downloads=$((downloads + 1))
    rm -rf "$work/dl"
    if ! gh run download "$id" --repo "$repo" -n "bundle-state-$head" -D "$work/dl"; then
        echo "::warning::$where: the bundle state artifact could not be downloaded (expired or rate limited), skipped"
        continue
    fi
    state="$work/dl/bundle-state.tar.zst"
    # Paths the state would write or delete.
    tar --zstd -tf "$state" | grep -v -e '^\.ci-deleted-files$' -e '/$' | sort -u > "$work/state-paths" || true
    tar --zstd -xOf "$state" .ci-deleted-files | sort -u > "$work/deleted-paths"
    sort -u "$work/state-paths" "$work/deleted-paths" > "$work/touched"
    comm -12 "$work/touched" "$work/differs" > "$work/conflicts"
    if [[ -s "$work/conflicts" ]]; then
        echo "$where: branch differs on paths the state would overwrite or delete, skipped:"
        sed 's/^/  conflict: /' "$work/conflicts"
        skipped="$skipped $head"
        continue
    fi
    run_id="$id"
    sha="$head"
    break
done < "$work/runs"

if [[ -z "$run_id" ]]; then
    fallback "none of the $considered newest retained bundle states is compatible with this branch (retention is 1 day)"
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
