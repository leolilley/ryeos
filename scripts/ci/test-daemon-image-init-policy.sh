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
    instructions="$(dockerfile_instructions "$path")"
    if grep -Eqi '^copy .*ryeosd|^label .*io\.ryeos\.' <<<"$instructions"; then
        discovered_images+=("$(basename "$path")")
    fi
done

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

# Images that promise an exact source bundle set must copy that same set into
# their final stage. Publishing a bundle in the builder but omitting it from
# /opt/ryeos makes a green image that cannot install the advertised workload.
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
assert_runtime_bundle_inventory() {
    local image="$1" bundle_set="$2" instructions final_stage expected_bundles actual_bundles
    instructions="$(dockerfile_instructions "$root/$image")"
    final_stage="$(awk '
        tolower($1) == "from" { stage = "" }
        { stage = stage $0 ORS }
        END { printf "%s", stage }
    ' <<<"$instructions")"
    expected_bundles="$(ryeos_bundle_set_names "$bundle_set" | sort)"
    actual_bundles="$(sed -nE \
        -e 's#^COPY --from=builder /build/bundles/([^/.][^ /]*) /opt/ryeos/([^ /]+)$#\1 \2#p' \
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

assert_runtime_init_profile() {
    local image="$1" bundle_set="$2" instructions final_stage expected_selector actual_selector
    instructions="$(dockerfile_instructions "$root/$image")"
    final_stage="$(awk '
        tolower($1) == "from" { stage = "" }
        { stage = stage $0 ORS }
        END { printf "%s", stage }
    ' <<<"$instructions")"
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

release_bundle_instructions="$(dockerfile_instructions "$root/Dockerfile.release-bundles")"
if grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)|^entrypoint |^env RYEOS_INIT_NODE_PROFILE=' <<<"$release_bundle_instructions"; then
    echo "Dockerfile.release-bundles is an artifact export and must not gain runtime init policy" >&2
    exit 1
fi

echo "daemon image init policy cases passed"
