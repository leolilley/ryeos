#!/usr/bin/env bash

# Keep every image that launches ryeosd behind the same PID-1 init boundary.
# The explicit inventory makes additions and removals deliberate, while the
# discovery pass prevents a new daemon image from silently evading the policy.

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"

daemon_images=(
    Dockerfile
    Dockerfile.central-host
    Dockerfile.dev
    Dockerfile.hosted-node
    Dockerfile.hosted-workflow
    Dockerfile.standard
)

discovered_images=()
dockerfile_instructions() {
    awk '
        BEGIN { escape = "\\" }
        function flush() {
            if (instruction != "") {
                gsub(/[[:space:]]+/, " ", instruction)
                sub(/^ /, "", instruction)
                print instruction
                instruction = ""
            }
        }
        {
            line = $0
            sub(/\r$/, "", line)
            if (instruction == "") {
                sub(/^[[:space:]]+/, "", line)
                if (line ~ /^#[[:space:]]*escape[[:space:]]*=/) {
                    escape = line
                    sub(/^#[[:space:]]*escape[[:space:]]*=[[:space:]]*/, "", escape)
                    sub(/[[:space:]]*$/, "", escape)
                    next
                }
                if (line == "" || line ~ /^#/) {
                    next
                }
            }
            sub(/[[:space:]]*$/, "", line)
            continued = substr(line, length(line), 1) == escape
            if (continued) {
                line = substr(line, 1, length(line) - 1)
            }
            instruction = instruction " " line
            if (!continued) {
                flush()
            }
        }
        END { flush() }
    ' "$1"
}

for path in "$root"/Dockerfile*; do
    [[ -f "$path" ]] || continue
    # Dockerfile.release deliberately contains three named runtime targets.
    # They are asserted individually below rather than misclassified as one
    # final-stage daemon image by this single-image discovery pass.
    [[ "$(basename "$path")" != Dockerfile.release ]] || continue
    instructions="$(dockerfile_instructions "$path")"
    if grep -Eqi '^copy .*ryeosd|^label .*io\.ryeos\.' <<<"$instructions"; then
        discovered_images+=("$(basename "$path")")
    fi
done

dockerfile_stage_instructions() {
    local path="$1" wanted_stage="$2"
    dockerfile_instructions "$path" | awk -v wanted="$wanted_stage" '
        BEGIN { capture = 0; found = 0 }
        tolower($1) == "from" {
            capture = 0
            for (i = 1; i <= NF; i++) {
                if (tolower($i) == "as" && $(i + 1) == wanted) {
                    capture = 1
                    found = 1
                    break
                }
            }
        }
        capture { print }
        END { if (!found) exit 1 }
    '
}

expected="$(printf '%s\n' "${daemon_images[@]}" | sort)"
discovered="$(printf '%s\n' "${discovered_images[@]}" | sort)"
if [[ "$discovered" != "$expected" ]]; then
    echo "daemon Dockerfile inventory does not match discovered RyeOS images" >&2
    echo "expected:" >&2
    printf '%s\n' "$expected" >&2
    echo "discovered:" >&2
    printf '%s\n' "$discovered" >&2
    exit 1
fi

# Bundle publication exchanges a complete sibling generation atomically. A
# Docker COPY may reside in a lower overlay layer whose rename domain differs
# from the writable layer, so every image that runs population must first
# materialize the whole bundle tree in the same RUN instruction. This is an
# artifact-construction requirement, not a test or runtime fallback.
for path in "$root"/Dockerfile*; do
    [[ -f "$path" ]] || continue
    instructions="$(dockerfile_instructions "$path")"
    if ! grep -Fq './scripts/populate-bundles.sh' <<<"$instructions"; then
        continue
    fi
    for required in \
        'cp -a bundles .bundles-writable' \
        'rm -rf bundles' \
        'mv .bundles-writable bundles'; do
        if ! grep -Fq "$required" <<<"$instructions"; then
            echo "$(basename "$path") publishes bundles without same-layer materialization: $required" >&2
            exit 1
        fi
    done
done

required_entrypoint='ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]'
for image in "${daemon_images[@]}"; do
    path="$root/$image"
    instructions="$(dockerfile_instructions "$path")"
    final_stage="$(awk '
        tolower($1) == "from" { stage = "" }
        { stage = stage $0 ORS }
        END { printf "%s", stage }
    ' <<<"$instructions")"

    if ! grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)' <<<"$final_stage"; then
        echo "$image final stage does not install tini" >&2
        exit 1
    fi
    if [[ "$(grep -Eic '^run test -x /usr/bin/tini$' <<<"$final_stage")" -ne 1 ]]; then
        echo "$image final stage must prove /usr/bin/tini is executable" >&2
        exit 1
    fi
    if [[ "$(grep -Fxc "$required_entrypoint" <<<"$final_stage")" -ne 1 ]]; then
        echo "$image must declare the exact tini-wrapped entrypoint once" >&2
        exit 1
    fi
done

for stage in ryeos-standard ryeos-central-host ryeos-hosted-workflow; do
    final_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$stage")"
    if ! grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)' <<<"$final_stage"; then
        # The named final stages inherit the separately asserted runtime base.
        runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" workflow-runtime)"
        [[ "$stage" == ryeos-hosted-workflow ]] && \
            runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" hosted-runtime)"
        grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)' <<<"$runtime_stage" || {
            echo "Dockerfile.release target $stage does not inherit an installed tini" >&2
            exit 1
        }
        [[ "$(grep -Eic '^run test -x /usr/bin/tini$' <<<"$runtime_stage")" -eq 1 ]] || {
            echo "Dockerfile.release target $stage does not inherit a proven tini" >&2
            exit 1
        }
    fi
    if [[ "$(grep -Fxc "$required_entrypoint" <<<"$final_stage")" -ne 1 ]]; then
        echo "Dockerfile.release target $stage must declare the exact tini-wrapped entrypoint once" >&2
        exit 1
    fi
done

# Images that promise an exact source bundle set must copy that same set into
# their final stage. Publishing a bundle in the builder but omitting it from
# /opt/ryeos makes a green image that cannot install the advertised workload.
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
assert_runtime_bundle_inventory() {
    local image="$1" bundle_set="$2" stage="${3:-}" instructions final_stage expected_bundles actual_bundles
    instructions="$(dockerfile_instructions "$root/$image")"
    if [[ -n "$stage" ]]; then
        final_stage="$(dockerfile_stage_instructions "$root/$image" "$stage")"
    else
        final_stage="$(awk '
            tolower($1) == "from" { stage = "" }
            { stage = stage $0 ORS }
            END { printf "%s", stage }
        ' <<<"$instructions")"
    fi
    expected_bundles="$(ryeos_bundle_set_names "$bundle_set" | sort)"
    actual_bundles="$(sed -nE \
        -e 's#^COPY --from=[^ ]+ /build/bundles/([^/.][^ /]*) /opt/ryeos/([^ /]+)$#\1 \2#p' \
        -e 's#^COPY bundles/([^/.][^ /]*) /opt/ryeos/([^ /]+)$#\1 \2#p' \
        <<<"$final_stage" | awk '
        $1 != $2 {
            print "bundle COPY changes the bundle name: " $1 " -> " $2 > "/dev/stderr"
            failed = 1
            next
        }
        { print $1 }
        END { if (failed) exit 1 }
    ' | sort)"
    if [[ "$actual_bundles" != "$expected_bundles" ]]; then
        echo "$image runtime bundle inventory does not match $bundle_set bundle set" >&2
        echo "expected:" >&2
        printf '%s\n' "$expected_bundles" >&2
        echo "actual:" >&2
        printf '%s\n' "$actual_bundles" >&2
        exit 1
    fi
}

assert_runtime_bundle_inventory Dockerfile full
assert_runtime_bundle_inventory Dockerfile.dev full
assert_runtime_bundle_inventory Dockerfile.standard standard
assert_runtime_bundle_inventory Dockerfile.hosted-node hosted-node
assert_runtime_bundle_inventory Dockerfile.hosted-workflow hosted-workflow
assert_runtime_bundle_inventory Dockerfile.central-host central-host
assert_runtime_bundle_inventory Dockerfile.release standard ryeos-standard
assert_runtime_bundle_inventory Dockerfile.release central-host ryeos-central-host
assert_runtime_bundle_inventory Dockerfile.release hosted-workflow ryeos-hosted-workflow

assert_runtime_init_profile() {
    local image="$1" bundle_set="$2" stage="${3:-}" instructions final_stage expected_selector actual_selector
    instructions="$(dockerfile_instructions "$root/$image")"
    if [[ -n "$stage" ]]; then
        final_stage="$(dockerfile_stage_instructions "$root/$image" "$stage")"
    else
        final_stage="$(awk '
            tolower($1) == "from" { stage = "" }
            { stage = stage $0 ORS }
            END { printf "%s", stage }
        ' <<<"$instructions")"
    fi
    expected_selector="$(ryeos_bundle_set_node_init_profile "$bundle_set")"
    actual_selector="$(sed -nE 's/^ENV RYEOS_INIT_NODE_PROFILE=([^[:space:]]+)$/\1/p' <<<"$final_stage")"
    if [[ "$actual_selector" != "$expected_selector" ]]; then
        echo "$image init profile does not match $bundle_set bundle-set profile" >&2
        echo "expected: ${expected_selector:-<none>}" >&2
        echo "actual: ${actual_selector:-<none>}" >&2
        exit 1
    fi
}

assert_runtime_init_profile Dockerfile full
assert_runtime_init_profile Dockerfile.dev full
assert_runtime_init_profile Dockerfile.standard standard
assert_runtime_init_profile Dockerfile.hosted-node hosted-node
assert_runtime_init_profile Dockerfile.hosted-workflow hosted-workflow
assert_runtime_init_profile Dockerfile.central-host central-host
assert_runtime_init_profile Dockerfile.release standard ryeos-standard
assert_runtime_init_profile Dockerfile.release central-host ryeos-central-host
assert_runtime_init_profile Dockerfile.release hosted-workflow ryeos-hosted-workflow

# The shared entrypoint consumes only a generic selector and must not infer
# provider/workload policy from the bundles present in an image.
! grep -Fq 'hosted-workflow' "$root/deploy/entrypoint.sh"
! grep -Fiq 'codex' "$root/deploy/entrypoint.sh"
(
    # shellcheck source=deploy/entrypoint.sh
    source "$root/deploy/entrypoint.sh"
    policy_test_root="$(mktemp -d)"
    trap 'rm -rf "$policy_test_root"' EXIT

    # Image metadata is mandatory even when a persisted generation exists.
    unset RYEOS_INIT_NODE_PROFILE
    unset RYEOS_RESET_NODE_POLICY_GENERATION
    mkdir -p "$policy_test_root/.ai/node/policies"
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" >/dev/null 2>&1; then
        echo "entrypoint accepted an absent node init profile" >&2
        exit 1
    fi

    # A fresh root receives the exact mapped first-publication seed.
    RYEOS_INIT_NODE_PROFILE=hosted-workflow
    rm -rf "$policy_test_root/.ai"
    build_ryeos_init_args /opt/ryeos "$policy_test_root"
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --node-profile hosted-workflow" ]]

    # A present generation is preserved. Even a malformed occupant takes this
    # path so real RyeOS rejects it instead of silently falling back to seed.
    mkdir -p "$policy_test_root/.ai/node/policies"
    build_ryeos_init_args /opt/ryeos "$policy_test_root" >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos" ]]

    # Replacement is an explicit one-boot opt-in and remains part of the same
    # locked init transaction as exact bundle reconciliation.
    RYEOS_RESET_NODE_POLICY_GENERATION=1
    build_ryeos_init_args /opt/ryeos "$policy_test_root" >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --node-profile hosted-workflow --replace-node-policy-generation --confirm-node-policy-generation-replacement" ]]
    RYEOS_RESET_NODE_POLICY_GENERATION=invalid
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" >/dev/null 2>&1; then
        echo "entrypoint accepted an invalid policy replacement opt-in" >&2
        exit 1
    fi
    unset RYEOS_RESET_NODE_POLICY_GENERATION
    rm -rf "$policy_test_root/.ai/node/policies"
    : > "$policy_test_root/.ai/node/policies"
    build_ryeos_init_args /opt/ryeos "$policy_test_root" >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos" ]]
)

# central-auth owns Python-authored support and every runtime set that includes
# it must mechanically prove the interpreter is present in the final image.
for image in "${daemon_images[@]}"; do
    instructions="$(dockerfile_instructions "$root/$image")"
    final_stage="$(awk '
        tolower($1) == "from" { stage = "" }
        { stage = stage $0 ORS }
        END { printf "%s", stage }
    ' <<<"$instructions")"
    if [[ "$(grep -Eic '^run test -x /usr/bin/python3$' <<<"$final_stage")" -ne 1 ]]; then
        echo "$image final stage must prove /usr/bin/python3 is executable" >&2
        exit 1
    fi
done
for stage in ryeos-standard ryeos-central-host ryeos-hosted-workflow; do
    final_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$stage")"
    runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" workflow-runtime)"
    [[ "$stage" == ryeos-hosted-workflow ]] && \
        runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" hosted-runtime)"
    [[ "$(grep -Eic '^run test -x /usr/bin/python3$' <<<"$runtime_stage")" -eq 1 ]] || {
        echo "Dockerfile.release target $stage does not inherit a proven Python runtime" >&2
        exit 1
    }
done

release_instructions="$(dockerfile_instructions "$root/Dockerfile.release")"
[[ "$(grep -Fc './scripts/populate-bundles.sh' <<<"$release_instructions")" -eq 1 ]]
grep -Fq -- '--bundle-set release-artifacts' <<<"$release_instructions"
if grep -Eq 'cargo[[:space:]]+test|test_contract\.py|scripts/(ci|gate)' \
    "$root/Dockerfile.release" "$root/.github/workflows/publish-ryeosd.yml"; then
    echo "release construction must not run repository or runtime tests" >&2
    exit 1
fi

release_workflow="$root/.github/workflows/publish-ryeosd.yml"
release_bake="$root/docker-bake.release.hcl"
[[ "$(grep -Fc 'uses: docker/bake-action@' "$release_workflow")" -eq 1 ]] || {
    echo "release workflow must use exactly one unified Bake action" >&2
    exit 1
}
if grep -Fq 'uses: docker/build-push-action@' "$release_workflow" \
    || grep -Fq 'docker buildx build' "$release_workflow"; then
    echo "release workflow contains an independent image or archive build" >&2
    exit 1
fi
for target in bundle-artifact standard central-host hosted-workflow; do
    grep -Fq "target \"$target\"" "$release_bake" || {
        echo "release Bake contract is missing target $target" >&2
        exit 1
    }
done
[[ "$(grep -Fc 'cache-to' "$release_bake")" -eq 1 ]] || {
    echo "release Bake contract must export its shared build cache exactly once" >&2
    exit 1
}

echo "daemon image init policy cases passed"
