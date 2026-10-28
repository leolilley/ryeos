#!/usr/bin/env bash

# Keep third-party dependency compilation persisted across image builds without
# letting it reach workspace artifacts. The dependency stage must precede the
# source copy, mirror every real cargo invocation, and be carried by the bake
# file and workflow registry cache.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
dockerfile="$root/Dockerfile.release"
bake="$root/docker-bake.composition.hcl"
workflow="$root/.github/workflows/qualify-composition-images.yml"
populate="$root/scripts/populate-bundles.sh"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# One logical Dockerfile instruction per line, continuations joined.
instructions="$(awk '
    { line = $0; sub(/\r$/, "", line) }
    cont { sub(/^[[:space:]]+/, "", line) }
    {
        c = (substr(line, length(line), 1) == "\\")
        if (c) line = substr(line, 1, length(line) - 1)
        buf = buf line
        if (!c) { print buf; buf = "" }
        cont = c
    }
' "$dockerfile" | sed 's/[[:space:]]\+/ /g')"

line_of() { grep -n -m1 -E "$1" <<<"$instructions" | cut -d: -f1; }

# 1. The dependency stages precede the source copy and the first real stage.
dep_build="$(line_of '^FROM .* AS dependency-build$')"
dep_recipe="$(line_of '^FROM .* AS dependency-recipe$')"
compiled="$(line_of '^FROM .* AS compiled-publication$')"
[[ -n "$dep_build" && -n "$dep_recipe" && -n "$compiled" ]] || fail "dependency or compiled-publication stage missing"
(( dep_build < compiled && dep_recipe < compiled )) || fail "dependency stages must precede compiled-publication"
first_copy_all="$(awk -v from="$compiled" 'NR > from && /^COPY \. \.$/ { print NR; exit }' <<<"$instructions")"
[[ -n "$first_copy_all" ]] || fail "compiled-publication has no COPY . ."
dep_stage="$(awk -v a="$dep_build" -v b="$compiled" 'NR >= a && NR < b' <<<"$instructions")"
grep -q '^COPY \. \.$' <<<"$dep_stage" && fail "dependency-build must not copy the workspace sources"
grep -q 'cargo chef cook' <<<"$dep_stage" || fail "dependency-build has no cargo chef cook"
grep -q '^RUN cargo install cargo-chef --locked --version [0-9]\+\.[0-9]\+\.[0-9]\+$' <<<"$instructions" \
    || fail "cargo-chef must be installed --locked at an exact version"
grep -q 'cargo chef prepare' <<<"$instructions" || fail "no dependency recipe is prepared"

# The compile stages share the toolchain image digest with the dependency stages.
digests="$(grep -o '^FROM rust:[^ ]* ' <<<"$instructions" | sort -u | wc -l)"
[[ "$digests" == 1 ]] || fail "dependency and compile stages must use one pinned rust image"

# Every cache mount seeds from the dependency stage, so a fresh runner starts warm.
mounts="$(grep -o -- '--mount=type=cache,id=ryeos-release-cargo-[a-z]*,[^ ]*' <<<"$instructions")"
[[ -n "$mounts" ]] || fail "no cargo cache mounts found"
while IFS= read -r mount; do
    [[ "$mount" == *"from=dependency-build,source="* ]] || fail "cache mount does not seed from dependency-build: $mount"
done <<<"$mounts"

# 2. Workspace sources are not compiled by the dependency stage: only a cook of
# named packages, never a bare build of the workspace.
grep -q 'cargo build' <<<"$dep_stage" && fail "dependency-build must cook, never build workspace crates"

# Placeholder workspace artifacts are removed, and checked gone, after the last cook.
last_cook="$(grep -n 'cargo chef cook' <<<"$instructions" | tail -1 | cut -d: -f1)"
cleanup="$(awk -v a="$last_cook" -v b="$compiled" 'NR > a && NR < b' <<<"$instructions")"
grep -q '"metadata", "--no-deps"' <<<"$cleanup" || fail "no workspace member list is derived after the last cook"
grep -q '\.fingerprint' <<<"$cleanup" && grep -q '"deps"' <<<"$cleanup" \
    && grep -q 'x86_64-unknown-linux-gnu/release' <<<"$cleanup" || fail "cleanup must cover fingerprints, deps and both target dirs"
grep -q 'shutil.rmtree' <<<"$cleanup" && grep -q 'workspace artifacts remain' <<<"$cleanup" \
    || fail "cleanup must remove workspace artifacts and fail if any remain"

# 3. Every real cargo invocation has a cook with the same flags.
cooks="$(grep -o 'cargo chef cook [^&]*' <<<"$dep_stage")"
[[ "$(grep -c . <<<"$cooks")" -ge 8 ]] || fail "expected a cook for each distinct cargo invocation"
for flag in --release --locked; do
    while IFS= read -r cook; do
        [[ "$cook" == *" $flag "* ]] || fail "cook lacks $flag: $cook"
    done <<<"$cooks"
done
cook_with() { grep -E -- "$1" <<<"$cooks" || true; }
packages_of() { grep -o -- '--package [^ ]*' <<<"$1" | awk '{print $2}' | sort | tr '\n' ' '; }
features_of() { grep -o -- '--features [^ ]*' <<<"$1" | awk '{print $2}' | head -1; }

real_stages="$(awk -v from="$compiled" 'NR >= from' <<<"$instructions")"
real_builds="$(grep -o 'cargo build [^&]*' <<<"$real_stages")"
grep -q -- '-C target-feature=+crt-static' <<<"$real_stages" || fail "static build not found in real stages"

# The populate script's own builds: packages and features per bundle set.
source "$root/scripts/pkg/bundle-sets.sh"
release_pkgs="$(awk '/^  release-artifacts\)$/ { on = 1; next } on && /;;/ { exit } on { print }' "$populate" \
    | sed 's/.*pkgs=(//; s/)//; s/\\//g' | tr -s ' \n' '\n' | grep . | sort)"
static_pkgs="$(sed -n '/^for p in "\${pkgs\[@\]}"; do$/,/^done$/p' "$populate" \
    | grep -o '^    ryeos-[a-z-]*)\|^    modal-guest-launcher)' | tr -d ' )' | sort)"
static_pkgs="$(comm -12 <(printf "%s\n" "$release_pkgs") <(printf "%s\n" "$static_pkgs"))"
[[ -n "$release_pkgs" && -n "$static_pkgs" ]] || fail "could not read the populate package lists"
host_pkgs="$(comm -23 <(printf '%s\n' "$release_pkgs") <(printf '%s\n' "$static_pkgs"))"
release_features=""
for package in ryeosd ryeos-cli; do
    selected="$(ryeos_bundle_set_cargo_features release-artifacts "$package")"
    [[ -z "$selected" ]] || release_features="${release_features:+$release_features,}$selected"
done

host_cook="$(cook_with 'chef cook' | grep -v -- '--target' | grep -- "--package ryeosd" | grep -v -- '--no-default-features' | head -1)"
[[ "$(packages_of "$host_cook")" == "$(printf '%s ' $host_pkgs)" ]] || fail "host cook packages differ from populate-bundles.sh"
[[ "$(features_of "$host_cook")" == "$release_features" ]] || fail "host cook features differ from the release-artifacts selection"
static_cook="$(cook_with '--target x86_64-unknown-linux-gnu' | grep -v -- 'ryeos-workload-client')"
[[ "$(packages_of "$static_cook")" == "$(printf '%s ' $static_pkgs)" ]] || fail "static cook packages differ from populate-bundles.sh"
[[ "$(grep -c . <<<"$static_cook")" == 1 ]] || fail "expected one static worker cook"
grep -q 'RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-C target-feature=+crt-static" cargo chef cook' <<<"$dep_stage" \
    || fail "static cooks must carry the same RUSTFLAGS as the real static builds"

# Fixed invocations in the real stages.
grep -q -- 'cargo build --release --locked -p lillux -p ryeos-lillux-oci-hook' <<<"$real_builds" || fail "OCI hook build changed; update the cooks"
[[ "$(packages_of "$(cook_with '--package ryeos-lillux-oci-hook')")" == "lillux ryeos-lillux-oci-hook " ]] || fail "no cook for the OCI hook build"
grep -q -- '--target x86_64-unknown-linux-gnu -p ryeos-workload-client' <<<"$real_builds" || fail "workload client build changed; update the cooks"
workload_cook="$(cook_with '--package ryeos-workload-client')"
[[ -n "$workload_cook" && "$workload_cook" == *"--target x86_64-unknown-linux-gnu"* ]] || fail "no static cook for the workload client"
grep -q -- 'cargo build --release --locked -p ryeos-core-tools' <<<"$real_builds" || fail "core tools build changed; update the cooks"
[[ -n "$(cook_with '--package ryeos-core-tools$' | grep -v -- '--target')" ]] || fail "no cook for the core tools build"

# Every node and CLI pair: one cook per distinct (no-default-features, features).
pair_cooks="$(cook_with '--no-default-features')"
for set in core core-scheduling core-workflow core-knowledge standard f5r-controller; do
    features=""
    for package in ryeosd ryeos-cli; do
        selected="$(ryeos_bundle_set_cargo_features "$set" "$package")"
        [[ -z "$selected" ]] || features="${features:+$features,}$selected"
    done
    found=0
    while IFS= read -r cook; do
        [[ "$(packages_of "$cook")" == "ryeos-cli ryeosd " ]] || continue
        [[ "$(features_of "$cook")" == "$features" ]] && found=1
    done <<<"$pair_cooks"
    (( found == 1 )) || fail "no pair cook with the $set features: '$features'"
done
no_default="$(grep -c -- '--no-default-features -p ryeosd -p ryeos-cli' <<<"$real_builds")"
(( no_default >= 6 )) || fail "expected the node and CLI pair builds in the real stages"

# 4. The bake file and workflow carry the registry cache for every target.
targets="$(grep -o '^target "[a-z0-9-]*" {' "$bake" | grep -v '_composition' | cut -d'"' -f2)"
[[ "$(wc -l <<<"$targets")" == 10 ]] || fail "expected ten composition targets"
for target in $targets; do
    block="$(awk -v t="target \"$target\" {" '$0 == t { on = 1 } on { print } on && /^}/ { exit }' "$bake")"
    grep -q "cache-from = cache_from(\"$target\")" <<<"$block" || fail "$target lacks cache-from"
    grep -q "cache-to   = cache_to(\"$target\")" <<<"$block" || fail "$target lacks cache-to"
done
grep -q 'type=registry,ref=${BUILD_CACHE_REF}:${name},mode=max' "$bake" || fail "cache-to must be a mode=max registry cache"
grep -q 'type=registry,ref=${BUILD_CACHE_REF}:${name}"' "$bake" || fail "cache-from must read the registry cache"
grep -q 'compression=zstd,ignore-error=true' "$bake" || fail "cache-to must ignore export errors"
grep -q 'type=provenance,mode=max' "$bake" && grep -q 'type=sbom' "$bake" || fail "attestations must be kept"
grep -q 'BUILD_CACHE_REF=${{ steps.candidate.outputs.cache_ref }}' "$workflow" || fail "workflow does not pass BUILD_CACHE_REF"
grep -q 'ghcr.io/${OWNER,,}/ryeos-buildcache' "$workflow" || fail "workflow cache reference is not on GHCR"
grep -q 'type=gha' "$workflow" && grep -v '^ *#' "$workflow" | grep -q 'type=gha' && fail "workflow must not use the gha cache"

# 5. Binary inputs stay pinned.
for needle in 'cargo build --release --locked' 'SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH"'; do
    grep -q -F -- "$needle" <<<"$real_stages" || fail "real stages lost: $needle"
done
grep -q -F -- 'channel = "1.95.0"' "$root/rust-toolchain.toml" || fail "toolchain pin changed"
grep -q '^COPY rust-toolchain.toml' <<<"$dep_stage" || fail "dependency-build must use the pinned toolchain file"

echo "ok: image build cache policy"
