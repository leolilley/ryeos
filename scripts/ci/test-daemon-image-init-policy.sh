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

release_bundle_instructions="$(dockerfile_instructions "$root/Dockerfile.release-bundles")"
if grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)|^entrypoint ' <<<"$release_bundle_instructions"; then
    echo "Dockerfile.release-bundles is an artifact export and must not gain runtime init policy" >&2
    exit 1
fi

echo "daemon image init policy cases passed"
