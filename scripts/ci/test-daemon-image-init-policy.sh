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
    # Dockerfile.release contains multiple named runtime targets.
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
    if ! grep -Eqi '^run apt-get .* install .* python3-cryptography([[:space:]]|$)' <<<"$instructions"; then
        echo "$(basename "$path") publishes bundles without the signed payload verifier's Python dependency" >&2
        exit 1
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
required_healthcheck='HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 CMD ["python3", "-c", "import os,urllib.request;urllib.request.urlopen('\''http://127.0.0.1:'\''+os.environ.get('\''PORT'\'','\''8000'\'')+'\''/_ryeos/ready'\'',timeout=3).read()"]'
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
    if [[ "$(grep -Fxc "$required_healthcheck" <<<"$final_stage")" -ne 1 ]]; then
        echo "$image must probe exact daemon admission readiness once" >&2
        exit 1
    fi
done

# Named release targets that run the generic daemon entrypoint. Each inherits
# one of the two separately asserted runtime bases, read from its FROM line.
release_daemon_stages=(
    ryeos-standard
    ryeos-central-host
    ryeos-local-inference
    ryeos-hosted-workflow
    ryeos-substrate
    ryeos-core
    ryeos-core-scheduling
    ryeos-core-workflow
    ryeos-core-knowledge
    ryeos-f5r-controller
)
release_runtime_base() {
    local stage="$1" base
    base="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$stage" \
        | awk 'tolower($1) == "from" { print $2; exit }')"
    case "$base" in
        workflow-runtime|hosted-runtime) printf '%s\n' "$base" ;;
        *)
            echo "Dockerfile.release target $stage is not built on an asserted runtime base: ${base:-<none>}" >&2
            return 1
            ;;
    esac
}
for stage in "${release_daemon_stages[@]}"; do
    final_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$stage")"
    if ! grep -Eqi '^run .*apt-get .*install .*tini([[:space:]]|$)|^copy .* /usr/bin/tini([[:space:]]|$)' <<<"$final_stage"; then
        # The named final stages inherit the separately asserted runtime base.
        runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$(release_runtime_base "$stage")")"
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
    if [[ "$(grep -Fxc "$required_healthcheck" <<<"$final_stage")" -ne 1 ]]; then
        echo "Dockerfile.release target $stage must probe exact daemon admission readiness once" >&2
        exit 1
    fi
done
contained_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" ryeos-contained-workflow)"
if [[ "$(grep -Fxc "$required_healthcheck" <<<"$contained_stage")" -ne 1 ]]; then
    echo "Dockerfile.release target ryeos-contained-workflow must probe exact daemon admission readiness once" >&2
    exit 1
fi

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
assert_runtime_bundle_inventory Dockerfile.hosted-workflow hosted-workflow
assert_runtime_bundle_inventory Dockerfile.central-host central-host
assert_runtime_bundle_inventory Dockerfile.release standard ryeos-standard
assert_runtime_bundle_inventory Dockerfile.release central-host ryeos-central-host
assert_runtime_bundle_inventory Dockerfile.release local-inference ryeos-local-inference
assert_runtime_bundle_inventory Dockerfile.release hosted-workflow ryeos-hosted-workflow
assert_runtime_bundle_inventory Dockerfile.release hosted-workflow ryeos-contained-workflow
assert_runtime_bundle_inventory Dockerfile.release bundle-source ryeos-substrate
assert_runtime_bundle_inventory Dockerfile.release core ryeos-core
assert_runtime_bundle_inventory Dockerfile.release core-scheduling ryeos-core-scheduling
assert_runtime_bundle_inventory Dockerfile.release core-workflow ryeos-core-workflow
assert_runtime_bundle_inventory Dockerfile.release core-knowledge ryeos-core-knowledge
assert_runtime_bundle_inventory Dockerfile.release f5r-controller ryeos-f5r-controller

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
assert_runtime_init_profile Dockerfile.hosted-workflow hosted-workflow
assert_runtime_init_profile Dockerfile.central-host central-host
assert_runtime_init_profile Dockerfile.release standard ryeos-standard
assert_runtime_init_profile Dockerfile.release central-host ryeos-central-host
assert_runtime_init_profile Dockerfile.release local-inference ryeos-local-inference
assert_runtime_init_profile Dockerfile.release hosted-workflow ryeos-hosted-workflow
assert_runtime_init_profile Dockerfile.release bundle-source ryeos-substrate
assert_runtime_init_profile Dockerfile.release core ryeos-core
assert_runtime_init_profile Dockerfile.release core-scheduling ryeos-core-scheduling
assert_runtime_init_profile Dockerfile.release core-workflow ryeos-core-workflow
assert_runtime_init_profile Dockerfile.release core-knowledge ryeos-core-knowledge
assert_runtime_init_profile Dockerfile.release f5r-controller ryeos-f5r-controller

# The host class selects which of a profile's signed capability floors the node
# holds. It describes where an image is deployed, so no image may preset it.
for path in "$root"/Dockerfile*; do
    [[ -f "$path" ]] || continue
    if dockerfile_instructions "$path" | grep -Eq '^(ENV|ARG) RYEOS_INIT_HOST_CLASS([= ]|$)'; then
        echo "$(basename "$path") presets RYEOS_INIT_HOST_CLASS; the deployment must declare it" >&2
        exit 1
    fi
done

# Composition targets run a node pair compiled with exactly the native
# features of their own bundle set, from the selectable composition
# publication, never the full-featured release pair.
assert_composition_pair() {
    local stage="$1" pair="$2" bundle_set="$3" final_stage pair_stage binary
    final_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$stage")"
    pair_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$pair")"
    for binary in ryeosd ryeos; do
        [[ "$(grep -Ec "^COPY --from=$pair /build/target/$bundle_set/$binary /usr/local/bin/$binary$" <<<"$final_stage")" -eq 1 ]] || {
            echo "Dockerfile.release target $stage must take $binary from $pair" >&2
            exit 1
        }
    done
    if grep -E '^COPY --from=' <<<"$final_stage" | grep -Ev "^COPY --from=($pair|composition-publication) " >/dev/null; then
        echo "Dockerfile.release target $stage copies from outside its pair and the composition publication" >&2
        exit 1
    fi
    grep -Eq '^FROM composition-publication AS ' <<<"$pair_stage" || {
        echo "Dockerfile.release stage $pair is not built from the composition publication" >&2
        exit 1
    }
    grep -Fq "ryeos_bundle_set_cargo_features $bundle_set " <<<"$pair_stage" || {
        echo "Dockerfile.release stage $pair does not take its features from the $bundle_set bundle set" >&2
        exit 1
    }
    grep -Fq -- '--no-default-features -p ryeosd -p ryeos-cli' <<<"$pair_stage" || {
        echo "Dockerfile.release stage $pair must build the node pair without default features" >&2
        exit 1
    }
}
assert_composition_pair ryeos-core core-pair core
assert_composition_pair ryeos-core-scheduling core-scheduling-pair core-scheduling
assert_composition_pair ryeos-core-workflow core-workflow-pair core-workflow
assert_composition_pair ryeos-core-knowledge core-knowledge-pair core-knowledge
assert_composition_pair ryeos-f5r-controller controller-pair f5r-controller
assert_composition_pair ryeos-standard standard-pair standard
grep -Fxq 'ARG PUBLICATION_STAGE=published' <<<"$(dockerfile_instructions "$root/Dockerfile.release")" || {
    echo "Dockerfile.release must default the composition publication to the published stage" >&2
    exit 1
}
grep -Fxq 'FROM ${PUBLICATION_STAGE} AS composition-publication' <<<"$(dockerfile_instructions "$root/Dockerfile.release")" || {
    echo "Dockerfile.release composition publication must follow PUBLICATION_STAGE" >&2
    exit 1
}
# Only the distributable archives may depend on the official-only `published`
# stage; a runtime image reaching it would refuse a development publication.
published_copies="$(dockerfile_instructions "$root/Dockerfile.release" | grep -E '^(COPY --from=published |FROM published )' || true)"
[[ "$published_copies" == $'COPY --from=published /out/ /\nCOPY --from=published /workload-out/ /' ]] || {
    echo "Dockerfile.release: only the bundle and workload-client artifacts may depend on the published stage" >&2
    exit 1
}
# The selected features are themselves bundle-set data: no scheduler where the
# set has no scheduling bundle, no provider setup where it has no directive.
[[ -z "$(ryeos_bundle_set_cargo_features core ryeosd)" ]]
[[ "$(ryeos_bundle_set_cargo_features core-scheduling ryeosd)" == ryeosd/scheduling ]]
[[ -z "$(ryeos_bundle_set_cargo_features core-workflow ryeosd)" ]]
[[ -z "$(ryeos_bundle_set_cargo_features core-knowledge ryeosd)" ]]
[[ -z "$(ryeos_bundle_set_cargo_features f5r-controller ryeosd)" ]]
[[ "$(ryeos_bundle_set_cargo_features standard ryeosd)" == ryeosd/model-provider-setup,ryeosd/scheduling ]]

# Contained startup receives its profile through the admitted worker binding,
# rather than the generic image entrypoint. Keep the declared requirement exact
# and prove it maps to the copied hosted-workflow set; an ENV seed cannot replace
# that binding's authority.
contained_profile="$(sed -nE 's/^LABEL io\.ryeos\.required-node-profile="([^" ]+)"$/\1/p' <<<"$contained_stage")"
[[ "$contained_profile" == contained-workflow ]]
[[ "$(ryeos_node_init_profile_bundle_set "$contained_profile")" == hosted-workflow ]]
if grep -Eq '^ENV RYEOS_INIT_NODE_PROFILE=' <<<"$contained_stage"; then
    echo "contained workflow image must obtain its profile from the admitted worker binding" >&2
    exit 1
fi

# The shared entrypoint consumes only a generic selector and must not infer
# provider/workload policy from the bundles present in an image.
! grep -Fq 'hosted-workflow' "$root/deploy/entrypoint.sh"
! grep -Fiq 'codex' "$root/deploy/entrypoint.sh"
! grep -Fiq 'opencode' "$root/deploy/entrypoint.sh"
(
    # shellcheck source=deploy/entrypoint.sh
    source "$root/deploy/entrypoint.sh"
    policy_test_root="$(mktemp -d)"
    trap 'rm -rf "$policy_test_root"' EXIT

    unset PORT
    [[ "$(container_bind)" == '[::]:8000' ]]
    PORT=18081
    [[ "$(container_bind)" == '[::]:18081' ]]
    unset PORT

    # Image metadata is mandatory even when a persisted generation exists.
    unset RYEOS_INIT_NODE_PROFILE
    unset RYEOS_RESET_NODE_POLICY_GENERATION
    unset RYEOS_EXECUTION_HISTORY_SCHEMA_CUT
    mkdir -p "$policy_test_root/.ai/node/policies"
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null 2>&1; then
        echo "entrypoint accepted an absent node init profile" >&2
        exit 1
    fi

    # A fresh root receives the exact mapped first-publication seed.
    RYEOS_INIT_NODE_PROFILE=hosted-workflow
    unset RYEOS_SUBSTRATE_IMAGE RYEOS_SUBSTRATE_PROTOCOL
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null 2>&1; then
        echo "entrypoint accepted absent substrate authority" >&2
        exit 1
    fi
    RYEOS_SUBSTRATE_IMAGE=ghcr.io/example/ryeos-substrate:latest
    RYEOS_SUBSTRATE_PROTOCOL=1
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null 2>&1; then
        echo "entrypoint accepted mutable substrate authority" >&2
        exit 1
    fi
    RYEOS_SUBSTRATE_IMAGE="ghcr.io/example/ryeos-substrate@sha256:$(printf '1%.0s' {1..64})"
    RYEOS_SUBSTRATE_PROTOCOL=0
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null 2>&1; then
        echo "entrypoint accepted zero substrate protocol" >&2
        exit 1
    fi
    RYEOS_SUBSTRATE_PROTOCOL=1
    substrate_args="--substrate-image-digest sha256:$(printf '1%.0s' {1..64}) --substrate-protocol 1"
    rm -rf "$policy_test_root/.ai"
    # The profile is never passed without a declared host class.
    unset RYEOS_INIT_HOST_CLASS
    if build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null 2>&1; then
        echo "entrypoint initialized a profile without a declared host class" >&2
        exit 1
    fi
    RYEOS_INIT_HOST_CLASS=hosted
    build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081'
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --bind [::]:18081 $substrate_args --node-profile hosted-workflow --host-class hosted" ]]

    # A present generation is preserved. Even a malformed occupant takes this
    # path so real RyeOS rejects it instead of silently falling back to seed.
    mkdir -p "$policy_test_root/.ai/node/policies"
    build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --bind [::]:18081 $substrate_args" ]]

    # A declared class that differs from the recorded one is warned about
    # plainly; the same class, or no record, is silent.
    printf '{\n  "schema": "ryeos/substrate-identity/v2",\n  "host_class": "local"\n}\n' \
        > "$policy_test_root/.ai/node/substrate-identity.json"
    changed="$(build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' 2>&1 >/dev/null)"
    grep -Fq 'warning: RYEOS_INIT_HOST_CLASS is hosted, but this node was initialized under host class local' <<<"$changed"
    RYEOS_INIT_HOST_CLASS=local
    unchanged="$(build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' 2>&1 >/dev/null)"
    if grep -Fq 'warning:' <<<"$unchanged"; then
        echo "entrypoint warned about an unchanged or unrecorded host class" >&2
        exit 1
    fi
    RYEOS_INIT_HOST_CLASS=hosted
    rm "$policy_test_root/.ai/node/substrate-identity.json"
    unrecorded="$(build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' 2>&1 >/dev/null)"
    if grep -Fq 'warning:' <<<"$unrecorded"; then
        echo "entrypoint warned about an unchanged or unrecorded host class" >&2
        exit 1
    fi

    # The environment never replaces a policy generation: the retired
    # variable refuses the start, whatever its value, and init arguments carry
    # no replacement flags.
    RYEOS_RESET_NODE_POLICY_GENERATION=1
    if refuse_retired_environment >/dev/null 2>&1; then
        echo "entrypoint accepted the retired policy-generation reset variable" >&2
        exit 1
    fi
    RYEOS_RESET_NODE_POLICY_GENERATION=0
    if refuse_retired_environment >/dev/null 2>&1; then
        echo "entrypoint accepted the retired policy-generation reset variable set to 0" >&2
        exit 1
    fi
    unset RYEOS_RESET_NODE_POLICY_GENERATION
    refuse_retired_environment
    build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null
    if grep -Fq 'replace-node-policy-generation' <<<"${INIT_ARGS[*]}"; then
        echo "entrypoint passed a policy replacement flag" >&2
        exit 1
    fi
    unset RYEOS_INIT_HOST_CLASS
    # Preserving a present generation selects nothing, so it needs no class.
    build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --bind [::]:18081 $substrate_args" ]]
    RYEOS_INIT_HOST_CLASS=hosted
    rm -rf "$policy_test_root/.ai/node/policies"
    : > "$policy_test_root/.ai/node/policies"
    build_ryeos_init_args /opt/ryeos "$policy_test_root" '[::]:18081' >/dev/null
    [[ "${INIT_ARGS[*]}" == "init --non-interactive --app-root $policy_test_root --source /opt/ryeos --bind [::]:18081 $substrate_args" ]]

    # A mount nested under the app root is refused before anything changes.
    # No mount can be created unprivileged, so the app root's device number is
    # overridden: every entry then reads as another filesystem.
    nested_root="$policy_test_root/nested-mount"
    mkdir -p "$nested_root/inner"
    : > "$nested_root/inner/file"
    [[ -z "$(first_entry_on_other_device "$nested_root" "$(stat -c %d "$nested_root")")" ]]
    [[ -n "$(first_entry_on_other_device "$nested_root" 0)" ]]
    before="$(stat -c '%u:%g' "$nested_root/inner" "$nested_root/inner/file")"
    if hand_app_root_to_node_account "$nested_root" 0 >/dev/null 2>&1; then
        echo "entrypoint re-owned a tree containing a nested mount" >&2
        exit 1
    fi
    [[ "$(stat -c '%u:%g' "$nested_root/inner" "$nested_root/inner/file")" == "$before" ]]
    rm -rf "$nested_root"

    # The app root is handed over as an owner-private directory: exactly
    # 0700, with an inherited set-group-id bit cleared, owned by the caller.
    handed_root="$policy_test_root/handed-root"
    mkdir -p "$handed_root"
    chmod 2775 "$handed_root"
    # The node account is the invoking user here, since no other owner can be
    # assigned unprivileged.
    NODE_UID="$(id -u)" NODE_GID="$(id -g)" hand_app_root_to_node_account "$handed_root"
    [[ "$(stat -c '%a' "$handed_root")" == 700 ]]
    [[ "$(stat -c '%u' "$handed_root")" == "$(id -u)" ]]
    rm -rf "$handed_root"

    # The execution-history schema cut is a signed offline operation, not an
    # environment route.
    RYEOS_EXECUTION_HISTORY_SCHEMA_CUT=35:39
    if refuse_retired_environment >/dev/null 2>&1; then
        echo "entrypoint accepted the retired execution-history schema cut variable" >&2
        exit 1
    fi
    unset RYEOS_EXECUTION_HISTORY_SCHEMA_CUT

    # An initialized volume (a completed init or a policy generation) is never
    # re-seeded because its bundle directory is missing; an identity alone is
    # what an init that died early leaves, and is seeded again.
    fresh_root="$policy_test_root/fresh-root"
    mkdir -p "$fresh_root/.ai/node/identity"
    : > "$fresh_root/.ai/node/identity/private_key.pem"
    require_fresh_app_root "$fresh_root"
    mkdir -p "$fresh_root/.ai/node/policies"
    if require_fresh_app_root "$fresh_root" >/dev/null 2>&1; then
        echo "entrypoint re-seeded an app root holding a policy generation" >&2
        exit 1
    fi
    rm -rf "$fresh_root/.ai/node/policies"
    mkdir -p "$fresh_root/.ai/config/onboarding"
    : > "$fresh_root/.ai/config/onboarding/init-completion.json"
    if require_fresh_app_root "$fresh_root" >/dev/null 2>&1; then
        echo "entrypoint re-seeded an app root holding a completed init" >&2
        exit 1
    fi
    rm -rf "$fresh_root"

    # The platform's offline-operation request is copied into the inbox of an
    # initialized node, once, by the digest of its bytes, and decided nowhere.
    offline_root="$policy_test_root/offline-root"
    mkdir -p "$offline_root/.ai/node/identity" "$offline_root/.ai/state"
    : > "$offline_root/.ai/node/identity/private_key.pem"
    offline_inbox="$offline_root/.ai/state/offline-operations/inbox"
    printf '{"body":{},"signature":"x"}' > "$policy_test_root/request.json"
    request_digest="$(sha256sum "$policy_test_root/request.json" | cut -d' ' -f1)"
    mkdir -p "$policy_test_root/inputs"
    printf 'input bytes' > "$policy_test_root/inputs/anything"
    input_digest="$(sha256sum "$policy_test_root/inputs/anything" | cut -d' ' -f1)"
    unset RYEOS_OFFLINE_OPERATION_DELIVERED
    RYEOS_OFFLINE_OPERATION_REQUEST="$policy_test_root/request.json" \
        RYEOS_OFFLINE_OPERATION_INPUTS="$policy_test_root/inputs" \
        deliver_offline_operation_request "$offline_root" >/dev/null
    [[ -f "$offline_inbox/$request_digest.json" ]]
    [[ -f "$offline_inbox/$input_digest.input" ]]
    [[ "$(stat -c %a "$offline_inbox")" == 700 ]]
    [[ "$(stat -c %a "$offline_inbox/$request_digest.json")" == 600 ]]
    # Delivered once: after the node applies and removes it, the same file is
    # not copied again.
    rm "$offline_inbox/$request_digest.json" "$offline_inbox/$input_digest.input"
    RYEOS_OFFLINE_OPERATION_REQUEST="$policy_test_root/request.json" \
        RYEOS_OFFLINE_OPERATION_INPUTS="$policy_test_root/inputs" \
        deliver_offline_operation_request "$offline_root" >/dev/null
    [[ -z "$(ls -A "$offline_inbox")" ]]
    # A base64 value is accepted, a link and an oversized file are not, and a
    # failure never stops the start.
    rm -rf "$offline_root/.ai/state/offline-operations"
    RYEOS_OFFLINE_OPERATION_REQUEST="$(base64 -w0 < "$policy_test_root/request.json")" \
        deliver_offline_operation_request "$offline_root" >/dev/null
    [[ -f "$offline_inbox/$request_digest.json" ]]
    rm -rf "$offline_root/.ai/state/offline-operations"
    ln -s "$policy_test_root/request.json" "$policy_test_root/request-link.json"
    RYEOS_OFFLINE_OPERATION_REQUEST="$policy_test_root/request-link.json" \
        deliver_offline_operation_request "$offline_root" >/dev/null 2>&1
    [[ -z "$(ls -A "$offline_inbox")" ]]
    head -c $((OFFLINE_MAX_REQUEST_BYTES + 1)) /dev/zero > "$policy_test_root/huge.json"
    RYEOS_OFFLINE_OPERATION_REQUEST="$policy_test_root/huge.json" \
        deliver_offline_operation_request "$offline_root" >/dev/null 2>&1
    [[ -z "$(ls -A "$offline_inbox")" ]]
    # A node that is not initialized receives nothing.
    rm -rf "$offline_root/.ai/node" "$offline_root/.ai/state/offline-operations"
    RYEOS_OFFLINE_OPERATION_REQUEST="$policy_test_root/request.json" \
        deliver_offline_operation_request "$offline_root" >/dev/null 2>&1
    [[ ! -e "$offline_root/.ai/state/offline-operations" ]]
    rm -rf "$offline_root"

    # The daemon consumes the create-once endpoint from config.yaml; ordinary
    # container startup must not present a competing runtime override.
    unset RYEOS_HOST_RUNTIME_BINDING
    # The image seed reaches the daemon as flags when the image pins its
    # substrate by digest, and never otherwise.
    build_ryeos_daemon_args /data/app
    [[ "${DAEMON_ARGS[*]}" == "--app-root /data/app --image-seed-root /opt/ryeos --substrate-image-digest sha256:$(printf '1%.0s' {1..64})" ]]
    saved_substrate_image="$RYEOS_SUBSTRATE_IMAGE"
    unset RYEOS_SUBSTRATE_IMAGE
    build_ryeos_daemon_args /data/app
    [[ "${DAEMON_ARGS[*]}" == "--app-root /data/app" ]]
    RYEOS_SUBSTRATE_IMAGE="$saved_substrate_image"
    RYEOS_HOST_RUNTIME_BINDING=/run/ryeos/host-runtime.json
    build_ryeos_daemon_args /data/app
    [[ "${DAEMON_ARGS[*]}" == "host-runtime --binding /run/ryeos/host-runtime.json" ]]
    RYEOS_HOST_RUNTIME_BINDING=relative.json
    if build_ryeos_daemon_args /data/app >/dev/null 2>&1; then
      echo "relative external host-runtime binding was accepted" >&2
      exit 1
    fi
    unset RYEOS_HOST_RUNTIME_BINDING
    # A recorded restart inhibition paces the start ryeosd will refuse.
    [[ "$(restart_inhibition_hold_seconds "$policy_test_root")" == 0 ]]
    mkdir -p "$policy_test_root/.ai/state"
    : > "$policy_test_root/.ai/state/restart-inhibited.json"
    [[ "$(restart_inhibition_hold_seconds "$policy_test_root")" == "$RESTART_INHIBITED_HOLD_SECONDS" ]]

    # The operator hold changes nothing unless the value is exactly 1, and
    # refuses any value other than unset, 0 or 1.
    unset RYEOS_OPERATOR_HOLD
    ! operator_hold_requested
    RYEOS_OPERATOR_HOLD=0
    ! operator_hold_requested
    hold_for_operator "$policy_test_root"
    hold_status=0
    RYEOS_OPERATOR_HOLD=true operator_hold_requested 2>/dev/null || hold_status=$?
    [[ "$hold_status" -eq 2 ]]
    hold_status=0
    RYEOS_OPERATOR_HOLD=yes operator_hold_requested 2>/dev/null || hold_status=$?
    [[ "$hold_status" -eq 2 ]]
    ! RYEOS_OPERATOR_HOLD='' operator_hold_requested
    RYEOS_OPERATOR_HOLD=1
    operator_hold_requested
    OPERATOR_HOLD_POLL_SECONDS=1
    hold_marker="$policy_test_root/.ai/state/operator-hold-released"
    rm -f "$hold_marker"
    (sleep 3; : > "$hold_marker") &
    hold_release_pid=$!
    hold_start=$SECONDS
    hold_message="$(hold_for_operator "$policy_test_root" 2>&1)"
    wait "$hold_release_pid"
    [[ $((SECONDS - hold_start)) -ge 2 ]]
    grep -Fq 'held for operator commands' <<<"$hold_message"
    [[ ! -e "$hold_marker" ]]
    # The hold refuses on the host-runtime route instead of being ignored.
    grep -Fq 'RYEOS_OPERATOR_HOLD is not supported with RYEOS_HOST_RUNTIME_BINDING' "$root/deploy/entrypoint.sh"
    unset RYEOS_OPERATOR_HOLD
    [[ "$RESTART_INHIBITED_HOLD_SECONDS" -gt 0 && "$RESTART_INHIBITED_HOLD_SECONDS" -le 60 ]]
    grep -Fq 'exec ryeosd "${DAEMON_ARGS[@]}"' "$root/deploy/entrypoint.sh"
    ! sed -n '/exec ryeosd/,$p' "$root/deploy/entrypoint.sh" | grep -Fq -- '--bind'
)

# These current images require Python for readiness probes and admitted tools;
# optional central-auth is one additional consumer, not the interpreter owner.
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
for stage in "${release_daemon_stages[@]}"; do
    runtime_stage="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$(release_runtime_base "$stage")")"
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
for target in bundle-artifact standard central-host local-inference hosted-workflow; do
    grep -Fq "target \"$target\"" "$release_bake" || {
        echo "release Bake contract is missing target $target" >&2
        exit 1
    }
done
grep -Fq 'target "contained-workflow-qualification"' "$release_bake" || {
    echo "release Bake contract is missing the contained qualification candidate" >&2
    exit 1
}
if grep -Fq 'contained-workflow-qualification' "$release_workflow"; then
    echo "contained qualification candidate must not enter official release promotion" >&2
    exit 1
fi
composition_bake="$root/docker-bake.composition.hcl"
for target in core core-scheduling core-workflow core-knowledge standard central-host local-inference hosted-workflow bundle-source f5r-controller; do
    grep -Fq "target \"$target\"" "$composition_bake" || {
        echo "composition Bake contract is missing target $target" >&2
        exit 1
    }
done
composition_workflow="$root/.github/workflows/qualify-composition-images.yml"
smoke_step="$(sed -n '/^      - name: First-boot init smoke$/,$p' "$composition_workflow")"
[[ -n "$smoke_step" ]] || {
    echo "composition workflow must carry a First-boot init smoke step" >&2
    exit 1
}
# The smoke boots exactly the images the bake pushed, read from its
# metadata, and an undirected dispatch bakes the whole composition group.
grep -Fq 'built="$(jq -r '"'"'to_entries[] | select((.value | type) == "object" and (.value | has("containerimage.digest"))) | .key'"'"' <<<"$BAKE_METADATA" | sort)"' <<<"$smoke_step" || {
    echo "composition first-boot init smoke must cover every image the bake pushed" >&2
    exit 1
}
grep -Eq '^ +for target in \$built; do$' <<<"$smoke_step" || {
    echo "composition first-boot init smoke must iterate the pushed images" >&2
    exit 1
}
grep -Fq 'default: "composition"' "$composition_workflow" || {
    echo "composition workflow must bake the whole group unless a dispatch names targets" >&2
    exit 1
}
for required in '@${DIGEST}' 'RYEOS_TRUST_BAKED_PUBLISHERS=1' 'host_classes:' 'RYEOS_INIT_HOST_CLASS="$(' 'RYEOS_SUBSTRATE_IMAGE="$IMAGE"'; do
    grep -Fq -- "$required" <<<"$smoke_step" || {
        echo "composition first-boot init smoke must contain $required" >&2
        exit 1
    }
done
if grep -Eq -- '-e RYEOS_INIT_HOST_CLASS=' <<<"$smoke_step"; then
    echo "composition first-boot init smoke must take the host class from each image's profile" >&2
    exit 1
fi
if grep -Fq 'RYEOS_INIT_NODE_PROFILE' <<<"$smoke_step" && grep -Eq -- '-e RYEOS_INIT_NODE_PROFILE=' <<<"$smoke_step"; then
    echo "composition first-boot init smoke must use each image's baked profile" >&2
    exit 1
fi
# Every bundle set is either built as a composition image or explicitly listed
# as having none, so a new set cannot ship without a deliberate choice. The
# stage of each imaged set is read from the Bake target of the same name.
image_bundle_sets=(
    standard
    central-host
    local-inference
    hosted-workflow
    bundle-source
    core
    core-scheduling
    core-workflow
    core-knowledge
    f5r-controller
)
no_image_bundle_sets=(
    full
    standard-central-auth
    release-authority
)
while IFS= read -r set_id; do
    listed=0
    for listed_id in "${image_bundle_sets[@]}" "${no_image_bundle_sets[@]}"; do
        [[ "$listed_id" != "$set_id" ]] || listed=$((listed + 1))
    done
    [[ "$listed" -eq 1 ]] || {
        echo "bundle set $set_id must be on exactly one of the image and no-image lists (found $listed)" >&2
        exit 1
    }
done < <(ryeos_bundle_set_ids)
for listed_id in "${image_bundle_sets[@]}" "${no_image_bundle_sets[@]}"; do
    ryeos_bundle_set_ids | grep -Fxq -- "$listed_id" || {
        echo "image inventory names $listed_id, which is not a bundle set" >&2
        exit 1
    }
done

# Bake targets and release stages correspond one to one.
composition_group="$(sed -nE '/^group "composition" \{/,/^\}/p' "$composition_bake" | grep -Eo '"[^"]+"' | tr -d '"' | grep -vx composition || true)"
bake_stages=()
while IFS= read -r target_name; do
    [[ "$target_name" != _composition ]] || continue
    stage="$(awk -v name="$target_name" '
        $0 == "target \"" name "\" {" { in_target = 1; next }
        in_target && /^\}/ { exit }
        in_target && $1 == "target" { gsub(/"/, "", $3); print $3; exit }
    ' "$composition_bake")"
    [[ -n "$stage" ]] || {
        echo "composition Bake target $target_name names no Dockerfile.release stage" >&2
        exit 1
    }
    printf '%s\n' "${release_daemon_stages[@]}" | grep -Fxq -- "$stage" || {
        echo "composition Bake target $target_name names $stage, which is not a release daemon stage" >&2
        exit 1
    }
    grep -Fxq -- "$target_name" <<<"$composition_group" || {
        echo "composition Bake target $target_name is missing from the composition group" >&2
        exit 1
    }
    bake_stages+=("$stage")
done < <(sed -nE 's/^target "([^"]+)" \{$/\1/p' "$composition_bake")
for stage in "${release_daemon_stages[@]}"; do
    printf '%s\n' "${bake_stages[@]}" | grep -Fxq -- "$stage" || {
        echo "release daemon stage $stage has no composition Bake target" >&2
        exit 1
    }
done
[[ "${#bake_stages[@]}" -eq "${#image_bundle_sets[@]}" ]] || {
    echo "composition Bake targets do not match the image bundle-set inventory" >&2
    exit 1
}
grep -Fq 'PUBLICATION_STAGE = PUBLICATION_STAGE' "$composition_bake" || {
    echo "composition Bake contract must pass the selected publication stage" >&2
    exit 1
}
if grep -Fq 'docker-bake.composition.hcl' "$release_workflow"; then
    echo "composition candidates must not enter official release promotion" >&2
    exit 1
fi
[[ "$(grep -Fc 'cache-to' "$release_bake")" -eq 1 ]] || {
    echo "release Bake contract must export its shared build cache exactly once" >&2
    exit 1
}

# The released-bundle controller is a separate Bake contract: it builds the
# controller daemon from the tag source but takes its bundles from the
# released archive, so it must not be able to sign and must not be a
# composition candidate.
controller_release_bake="$root/docker-bake.controller-release.hcl"
controller_release_workflow="$root/.github/workflows/build-controller-from-release.yml"
controller_release_stage=ryeos-f5r-controller-released
[[ "$(sed -nE 's/^target "([^"]+)" \{$/\1/p' "$controller_release_bake")" == f5r-controller-released ]] || {
    echo "controller-release Bake contract must define exactly the f5r-controller-released target" >&2
    exit 1
}
grep -Eq "^[[:space:]]*target[[:space:]]*=[[:space:]]*\"$controller_release_stage\"" "$controller_release_bake" || {
    echo "controller-release Bake target must build the $controller_release_stage stage" >&2
    exit 1
}
if grep -Eq 'secret|PUBLISHER_KEY|PUBLICATION_STAGE|inherits' "$controller_release_bake"; then
    echo "controller-release Bake contract must not take a publisher key or inherit a composition target" >&2
    exit 1
fi
grep -Fq 'released-bundles = RELEASED_BUNDLES_DIR' "$controller_release_bake" || {
    echo "controller-release Bake target must take its bundles from the materialized release directory" >&2
    exit 1
}
if grep -Fq "$controller_release_stage" "$composition_bake" "$release_bake"; then
    echo "the released-bundle controller stage must not be a composition or release candidate" >&2
    exit 1
fi
if grep -Fq 'docker-bake.controller-release.hcl' "$release_workflow"; then
    echo "the released-bundle controller must not enter official release promotion" >&2
    exit 1
fi
grep -Fq 'files: ./docker-bake.controller-release.hcl' "$controller_release_workflow" || {
    echo "the controller-from-release workflow must bake the controller-release contract" >&2
    exit 1
}
# The workflow may read the public development key to refuse its fingerprint,
# but it must never mount a publisher signing key into the build.
if grep -Eq 'RYEOS_PUBLISHER_KEY|id=publisher-key|secrets\.RYEOS' "$controller_release_workflow"; then
    echo "the controller-from-release workflow must not carry a signing key" >&2
    exit 1
fi
controller_release_instructions="$(dockerfile_stage_instructions "$root/Dockerfile.release" "$controller_release_stage")"
[[ "$(grep -Fxc "$required_entrypoint" <<<"$controller_release_instructions")" -eq 1 ]] || {
    echo "Dockerfile.release target $controller_release_stage must declare the exact tini-wrapped entrypoint once" >&2
    exit 1
}
grep -Eqi '^copy --from=released-bundles / /opt/ryeos/$' <<<"$controller_release_instructions" || {
    echo "Dockerfile.release target $controller_release_stage must take /opt/ryeos from the released bundles" >&2
    exit 1
}
if grep -Eqi 'populate-bundles|RYEOS_PUBLISHER_KEY|mount=type=secret' <<<"$controller_release_instructions"; then
    echo "Dockerfile.release target $controller_release_stage must not publish or sign bundles" >&2
    exit 1
fi
[[ "$(release_runtime_base "$controller_release_stage")" == workflow-runtime ]] || {
    echo "Dockerfile.release target $controller_release_stage must be built on the workflow runtime" >&2
    exit 1
}

# A draft descriptor must never ship inside a signed bundle tree; drafts live
# under authoring/ until they can be admitted.
unsigned_drafts="$(grep -rEl --include='*.yaml' --include='*.yml' \
    "^[[:space:]]*description:[[:space:]]*['\"]?UNSIGNED" "$root"/bundles/*/.ai || true)"
[[ -z "$unsigned_drafts" ]] || {
    echo "bundle trees must not ship unsigned draft descriptors:" >&2
    echo "$unsigned_drafts" >&2
    exit 1
}

echo "daemon image init policy cases passed"
