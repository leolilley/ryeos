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

# The hosted-workflow image promises one exact source bundle set to init.
# Keep its runtime COPY inventory mechanically identical to the shared
# authoring/install bundle-set definition: publishing a bundle in the builder
# but omitting it from /opt/ryeos makes a green image that cannot install the
# advertised workload.
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
hosted_workflow_instructions="$(dockerfile_instructions "$root/Dockerfile.hosted-workflow")"
hosted_workflow_final_stage="$(awk '
    tolower($1) == "from" { stage = "" }
    { stage = stage $0 ORS }
    END { printf "%s", stage }
' <<<"$hosted_workflow_instructions")"

expected_hosted_workflow_bundles="$(ryeos_bundle_set_names hosted-workflow | sort)"
actual_hosted_workflow_bundles="$(sed -nE \
    's#^COPY --from=builder /build/bundles/([^/.][^ /]*) /opt/ryeos/([^ /]+)$#\1 \2#p' \
    <<<"$hosted_workflow_final_stage" | awk '
        $1 != $2 {
            print "hosted-workflow bundle COPY changes the bundle name: " $1 " -> " $2 > "/dev/stderr"
            failed = 1
            next
        }
        { print $1 }
        END { if (failed) exit 1 }
    ' | sort)"
if [[ "$actual_hosted_workflow_bundles" != "$expected_hosted_workflow_bundles" ]]; then
    echo "Dockerfile.hosted-workflow runtime bundle inventory does not match bundle set" >&2
    echo "expected:" >&2
    printf '%s\n' "$expected_hosted_workflow_bundles" >&2
    echo "actual:" >&2
    printf '%s\n' "$actual_hosted_workflow_bundles" >&2
    exit 1
fi

release_bundle_instructions="$(dockerfile_instructions "$root/Dockerfile.release-bundles")"
if grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)|^entrypoint ' <<<"$release_bundle_instructions"; then
    echo "Dockerfile.release-bundles is an artifact export and must not gain runtime init policy" >&2
    exit 1
fi

echo "daemon image init policy cases passed"
