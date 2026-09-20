#!/usr/bin/env bash
# ryeos:signed:2026-09-19T02:07:39Z:8a757739b570684e93aaf1edc7982d177515a8be957c584423a69ffb33172c2d:tsuSgeZ08lUXDJdRC/Bftl1fhqi0co/CzeiQgc+/6CRhswM5e6H4vwjDW73KTudlFpSUqhU5C4Kb55ulcQDCCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Fast local packaged-layout install from this checkout.
#
# This intentionally skips yay/makepkg but installs the same runtime layout
# as deploy/aur/ryeos/PKGBUILD:
#   - binaries -> /usr/bin
#   - bundle sources -> /usr/share/ryeos/<name> for each bundle in the set.
#     The set membership is the single source of truth in
#     scripts/pkg/bundle-sets.sh (full = core, central-auth, standard, web,
#     browser, ryeos-ui, hosted-node, codex, local-inference; the lean sets are
#     subsets).
#   - ryeos init copies bundle sources into ~/.local/share/ryeos
#
# Use the AUR flow for package-manager ownership. Use this script for fast
# local repair/testing when you explicitly want to bypass the package build.

set -euo pipefail

_install_script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$_install_script_dir/../lib/ryeos-terminal.sh"

usage() {
    cat <<'EOF'
Usage: scripts/pkg/install-local-direct.sh [options]

Fast-install the current checkout using the packaged RyeOS layout:
  /usr/bin/ryeos
  /usr/share/ryeos/<name>/.ai                      (each bundle in the set)
  ~/.local/share/ryeos/.ai/bundles/<name>          (after init)
Set membership is defined in scripts/pkg/bundle-sets.sh (full = core,
central-auth, standard, web, browser, ryeos-ui, hosted-node, codex,
local-inference).

Options:
  --populate            Run scripts/populate-bundles.sh first. Requires either a
                        targeted --crates package list or explicit --all.
  --no-init             Install files but do not run ryeos init
  --no-daemon-restart   Do not stop/restart an already-running daemon
  --keep-shadows        Do not move /usr/local/bin, ~/.local/bin, or the invoking
                        user's Cargo-home bin shadows of installed RyeOS binaries
  --trust-source-publishers
                        Explicitly trust the publisher documents copied from
                        this checkout. Required to initialize dev/custom-signed
                        bundles; never enabled automatically.
  --reset-node-policy-generation
                        Explicitly replace an obsolete complete node-policy
                        generation from this bundle set's signed init profile.
                        Requires init and preserves all non-policy node state.
  --app-root DIR        Select the exact existing node to initialize and manage.
                        This explicit value survives the root-owned package
                        transaction; no privileged re-exec inherits it from the
                        ambient environment.
  --key PATH            Publisher key for populate-bundles.sh
                        (default: .dev-keys/PUBLISHER_DEV.pem)
  --owner LABEL         Owner label for populate-bundles.sh
                        (default: RyeOS Development)
  --bundle-set SET      Bundle set to populate/install: full, central-host,
                        standard, local-inference, hosted-node, hosted-workflow,
                        bundle-source, or release-authority. Each set
                        has an explicit default publisher-authored node init
                        profile; an existing signed generation is preserved.
                        (default: full)
  --node-profile NAME   Select another publisher-authored policy profile whose
                        exact_bundles match --bundle-set. For example,
                        development selects enforced hosted-development policy
                        over the full bundle set without creating another set.
  --jobs N              Cap cargo build parallelism during --populate (cargo -j N).
                        Use a smaller N if a full release build exhausts memory.
  --cargo-target-dir DIR
                        Use this absolute Cargo target directory for population
                        and installation, including across privileged re-exec.
                        Defaults to this checkout's target directory.
  --crates "A B C"      With --populate, rebuild only these Cargo packages (e.g.
                        --crates ryeosd for a daemon-only source correction).
                        Unselected bundle payloads retain their existing exact
                        artifact generation and must already be built.
  --all                 With --populate, rebuild the whole bundle set. Required to
                        do a full rebuild — --populate refuses to build everything
                        implicitly (that full release build is what exhausts memory).
  -h, --help            Show this help

Default behavior installs already-built user binaries and exact, closed source
bundle generations without rebuilding or republishing their payloads. It stops
any already-running daemon, moves stale PATH shadows aside, runs ryeos init
using only the compiled official-publisher trust root, then restarts the daemon
if it was running before the install. Development/custom publisher documents
require --trust-source-publishers. Use --populate --crates ... for the focused
development loop; reserve --populate --all for release/E2E qualification.
EOF
}

die() {
    if declare -F ryeos_term_fail >/dev/null 2>&1; then
        ryeos_term_fail "$*"
    else
        printf 'install-local-direct.sh: %s\n' "$*" >&2
    fi
    exit 1
}

run_timeout() {
    local seconds="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$seconds" "$@"
    else
        "$@"
    fi
}

# The user who invoked the installer. Under sudo, lifecycle commands (status/
# stop/start) MUST run as this user, not root: the daemon and its state live
# under the user's XDG data dir, so a root-context `ryeos` resolves root's
# app-root instead — it sees no daemon, so it never stops the stale one and
# never restarts, leaving the old binary running against the swapped-out files.
# Same drop-to-user reasoning as the populate/init steps below.
invoking_user="${SUDO_USER:-$(id -un)}"
# That user's home, resolved from passwd — NEVER $HOME, which is /root under
# sudo and would silently point app-root fallbacks at root's data dir.
invoking_user_home="$(getent passwd "$invoking_user" | cut -d: -f6)"

# Resolve Cargo's user-owned bin directory from the same login environment used
# for bundle population. Cargo home is configurable and on RyeOS development
# hosts is commonly XDG-aligned rather than ~/.cargo. Hard-coding ~/.cargo would
# leave the actual stale binary ahead of /usr/bin and make an otherwise complete
# install fail only at the final PATH check.
invoking_user_shell="$(getent passwd "$invoking_user" | cut -d: -f7)"
[[ -x "$invoking_user_shell" ]] || invoking_user_shell="/bin/sh"
if [[ "$invoking_user" != "$(id -un)" ]]; then
    # Ask the login environment to execute only the external `env` command.
    # Parameter expansion syntax is shell-specific (fish/nushell are valid
    # passwd shells), so the installer parses the exported value itself.
    invoking_user_environment="$(
        sudo -H -u "$invoking_user" "$invoking_user_shell" -lc env
    )"
    invoking_user_cargo_home="$(
        printf '%s\n' "$invoking_user_environment" |
            sed -n 's/^CARGO_HOME=//p' |
            tail -n 1
    )"
    [[ -n "$invoking_user_cargo_home" ]] || \
        invoking_user_cargo_home="$invoking_user_home/.cargo"
else
    invoking_user_cargo_home="${CARGO_HOME:-$invoking_user_home/.cargo}"
fi
[[ "$invoking_user_cargo_home" == /* ]] || \
    die "invoking user's Cargo home is not absolute: $invoking_user_cargo_home"

# Run `ryeos <args>` with a timeout, as the invoking user when under sudo so it
# targets that user's app-root. `timeout` wraps the external command (sudo or
# ryeos), never a shell function.
ryeos_user() {
    local secs="$1"
    shift
    # Initialization already passes the selected app root explicitly. Carry
    # the same selection across sudo/login for lifecycle operations too;
    # otherwise an install targeting a second node can stop the primary node.
    local selected_app_root="${init_app_root:-${RYEOS_APP_ROOT:-}}"
    if [[ "$invoking_user" != "$(id -un)" ]]; then
        local user_shell cmd a
        user_shell="$(getent passwd "$invoking_user" | cut -d: -f7)"
        [[ -x "$user_shell" ]] || user_shell="/bin/sh"
        if [[ -n "$selected_app_root" ]]; then
            printf -v cmd 'exec env %q ryeos' "RYEOS_APP_ROOT=$selected_app_root"
        else
            printf -v cmd 'exec ryeos'
        fi
        for a in "$@"; do printf -v cmd '%s %q' "$cmd" "$a"; done
        run_timeout "$secs" sudo -H -u "$invoking_user" "$user_shell" -lc "$cmd"
    else
        if [[ -n "$selected_app_root" ]]; then
            RYEOS_APP_ROOT="$selected_app_root" run_timeout "$secs" ryeos "$@"
        else
            run_timeout "$secs" ryeos "$@"
        fi
    fi
}

ryeos_status_quick() {
    RYEOS_TTY=never ryeos_user 10 node status
}

# Build only the init-profile portion of init. The distribution mapping is
# first-publication authority; any existing occupant is preserved so RyeOS can
# validate it as one complete signed generation. In particular, an empty file,
# link, or partial directory never triggers a profile fallback.
build_install_init_profile_args() {
    local policy_generation_path="$1"
    local mapped_profile="$2"
    local replace_generation="$3"

    [[ -n "$mapped_profile" ]] || return 1
    INSTALL_INIT_PROFILE_ARGS=()
    INSTALL_PUBLISH_INITIAL_POLICY=0
    if [[ "$replace_generation" == "1" ]]; then
        INSTALL_INIT_PROFILE_ARGS=(
            --node-profile "$mapped_profile"
            --replace-node-policy-generation
            --confirm-node-policy-generation-replacement
        )
        # Replacement publishes the selected profile just as first init does.
        # Keep the same exact post-init inventory check; merely observing a
        # nonempty generation would allow a partial or wrong replacement to
        # pass this installer boundary.
        INSTALL_PUBLISH_INITIAL_POLICY=1
    elif [[ ! -e "$policy_generation_path" && ! -L "$policy_generation_path" ]]; then
        INSTALL_INIT_PROFILE_ARGS=(--node-profile "$mapped_profile")
        INSTALL_PUBLISH_INITIAL_POLICY=1
    fi
}

# Compile the existing signed policy generation with the candidate daemon
# before acquiring administrator authority, stopping a live node, or changing
# the shared package namespace. A clean schema cut is never inferred: only the
# explicit reset flag changes this probe to meaning-blind predecessor
# verification. This keeps policy-kind/version knowledge in the registered
# Rust compilers rather than duplicating it in the installer.
preflight_existing_node_policy() {
    local policy_generation_path="$1"
    local reset_generation="$2"
    local -a command=(
        "$target_dir/ryeosd"
        init-policy-preflight
        --app-root "$state_root"
    )
    if [[ ! -e "$policy_generation_path" && ! -L "$policy_generation_path" ]]; then
        return 0
    fi
    [[ -e "$policy_generation_path" && ! -L "$policy_generation_path" ]] || {
        ryeos_term_fail "existing node policy generation path is unsafe: $policy_generation_path"
        return 1
    }
    if [[ "$reset_generation" == "1" ]]; then
        command+=(--schema-cut)
    fi
    if [[ "$invoking_user" != "$(id -un)" ]]; then
        sudo -H -u "$invoking_user" "${command[@]}"
    else
        "${command[@]}"
    fi
}

# Refuse a non-official key in a source publisher document before stopping the
# daemon or changing the installed layout unless the operator acknowledged it.
# The document is only a publisher pointer; it is never authority by location.
validate_source_publisher_trust() {
    local trust_file="$1"
    local allow_source_publishers="$2"
    local official_fingerprint="$3"
    local decoded_len
    local computed_fingerprint
    local -a declared_fingerprints=()
    local -a encoded_public_keys=()

    VALIDATED_SOURCE_PUBLISHER_FINGERPRINT=""

    if [[ ! -f "$trust_file" ]]; then
        ryeos_term_fail "source publisher trust document not found: $trust_file"
        return 1
    fi

    mapfile -t declared_fingerprints < <(
        sed -n 's/^[[:space:]]*fingerprint[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$trust_file"
    )
    mapfile -t encoded_public_keys < <(
        sed -n 's/^[[:space:]]*public_key[[:space:]]*=[[:space:]]*"ed25519:\([^"]*\)"[[:space:]]*$/\1/p' "$trust_file"
    )
    if [[ ${#declared_fingerprints[@]} -ne 1 ]]; then
        ryeos_term_fail "source publisher trust document must contain exactly one fingerprint: $trust_file"
        return 1
    fi
    if [[ ! "${declared_fingerprints[0]}" =~ ^[0-9a-f]{64}$ ]]; then
        ryeos_term_fail "source publisher trust document has an invalid fingerprint: $trust_file"
        return 1
    fi
    if [[ ${#encoded_public_keys[@]} -ne 1 ]]; then
        ryeos_term_fail "source publisher trust document must contain exactly one ed25519 public_key: $trust_file"
        return 1
    fi
    if ! decoded_len="$(printf '%s' "${encoded_public_keys[0]}" | base64 --decode 2>/dev/null | wc -c)"; then
        ryeos_term_fail "source publisher trust document has invalid base64 public_key: $trust_file"
        return 1
    fi
    if [[ "$decoded_len" -ne 32 ]]; then
        ryeos_term_fail "source publisher trust document public_key is not 32-byte Ed25519 material: $trust_file"
        return 1
    fi
    if ! computed_fingerprint="$(printf '%s' "${encoded_public_keys[0]}" | base64 --decode 2>/dev/null | sha256sum | cut -d' ' -f1)"; then
        ryeos_term_fail "could not fingerprint source publisher public_key: $trust_file"
        return 1
    fi
    if [[ "$computed_fingerprint" != "${declared_fingerprints[0]}" ]]; then
        ryeos_term_fail "source publisher trust document fingerprint does not match its public_key: $trust_file"
        return 1
    fi
    VALIDATED_SOURCE_PUBLISHER_FINGERPRINT="$computed_fingerprint"
    if [[ "$computed_fingerprint" == "$official_fingerprint" ]]; then
        return 0
    fi
    if [[ "$allow_source_publishers" == "1" ]]; then
        return 0
    fi

    ryeos_term_fail "source bundles are signed by non-official publisher $computed_fingerprint"
    ryeos_term_fail "refusing to pin trust from $trust_file automatically"
    ryeos_term_info "rerun with --trust-source-publishers to make this development/custom trust decision explicit, or use --no-init to install files only"
    return 1
}

# Validate every selected trust document, then report the operator's explicit
# trust decision once per distinct non-official publisher. A full source tree
# normally repeats one publisher document at the root and in every bundle; the
# documents remain independent validation boundaries even when their signer is
# the same.
validate_selected_source_publisher_trust() {
    local allow_source_publishers="$1"
    local official_fingerprint="$2"
    shift 2
    local trust_file fingerprint count
    local -a publisher_order=()
    local -A publisher_counts=()

    for trust_file in "$@"; do
        validate_source_publisher_trust \
            "$trust_file" "$allow_source_publishers" "$official_fingerprint" || return 1
        fingerprint="$VALIDATED_SOURCE_PUBLISHER_FINGERPRINT"
        if [[ "$fingerprint" == "$official_fingerprint" ]]; then
            continue
        fi
        if [[ -z "${publisher_counts[$fingerprint]+present}" ]]; then
            publisher_order+=("$fingerprint")
            publisher_counts["$fingerprint"]=0
        fi
        publisher_counts["$fingerprint"]=$((publisher_counts["$fingerprint"] + 1))
    done

    for fingerprint in "${publisher_order[@]}"; do
        count="${publisher_counts[$fingerprint]}"
        if (( count == 1 )); then
            ryeos_term_info "explicitly trusting source publisher $fingerprint"
        else
            ryeos_term_info "explicitly trusting source publisher $fingerprint · $count selected documents"
        fi
    done
}

require_closed_source_bundle_payloads() {
    local repo_root="$1"
    shift
    local name bundle_root
    local -a incomplete=()

    for name in "$@"; do
        bundle_root="$repo_root/bundles/$name"
        if [[ ! -s "$bundle_root/.ai/refs/bundles/manifest" \
            || ! -d "$bundle_root/.ai/objects" ]]; then
            incomplete+=("$name")
        fi
    done

    if (( ${#incomplete[@]} > 0 )); then
        ryeos_term_fail "source bundle payload is not published for: ${incomplete[*]}"
        ryeos_term_info "refusing to replace installed bundles with source trees that lack their closed manifest/object payload"
        ryeos_term_info "rerun with --populate and an explicit --crates or --all scope"
        return 1
    fi
}

# This is an external host-install boundary, not node or workload policy.
# Complete it before entering lifecycle shutdown: discovering a missing binary
# or prompting for sudo after stop strands a previously working node. The
# lifecycle owner, not this shell helper, must later establish supervisor
# inhibition and descendant recovery obligations for replacement.
preflight_host_install() {
    local release_dir="$1" binary
    shift
    for binary in "$@"; do
        [[ -f "$release_dir/$binary" && -x "$release_dir/$binary" ]] || {
            ryeos_term_fail "missing required release binary: $release_dir/$binary"
            return 1
        }
    done
    if [[ $(id -u) -ne 0 ]]; then
        # Never hide the only authorization prompt behind a progress renderer.
        ryeos_term_suspend
        sudo -v || {
            ryeos_term_fail "sudo authorization is required before stopping the node"
            return 1
        }
    fi
}

# Build init trust arguments from the exact source boundary the installer
# selected and validated. The result intentionally excludes every other
# document that might already exist below the packaged share directory.
collect_selected_source_trust_args() {
    local installed_share_dir="$1"
    shift
    local name trust_file
    local root_trust_file="$installed_share_dir/.ai/PUBLISHER_TRUST.toml"

    if [[ ! -f "$root_trust_file" ]]; then
        ryeos_term_fail "installed source-root trust document not found: $root_trust_file"
        return 1
    fi

    SELECTED_SOURCE_TRUST_ARGS=(--trust-file "$root_trust_file")
    for name in "$@"; do
        trust_file="$installed_share_dir/$name/PUBLISHER_TRUST.toml"
        if [[ -f "$trust_file" ]]; then
            SELECTED_SOURCE_TRUST_ARGS+=(--trust-file "$trust_file")
        fi
    done
    # Per-bundle docs are optional under the shared-root trust model; the
    # trailing test above must not decide this function's exit status.
    return 0
}

status_has_live_daemon() {
    grep -Eq '^(running|starting — daemon|live daemon control is unusable|failed — daemon)'
}

stop_daemon_for_install() {
    local status_out final_status

    status_out="$(ryeos_status_quick)" || \
        die "cannot establish node lifecycle state; refusing binary replacement"
    if ! status_has_live_daemon <<<"$status_out"; then
        # Only an affirmative offline result permits an initially stopped
        # install. Failed probes, stale ownership and unfamiliar output must
        # never be interpreted as absence of a node.
        if grep -Eq '^(initialized, stopped|not initialized)' <<<"$status_out"; then
            return 1
        fi
        die "node lifecycle ownership is uncertain; refusing binary replacement"
    fi

    ryeos_term_info "stopping live daemon before replacing binaries"
    ryeos_term_suspend
    # Exact process control and escalation belong to the installed lifecycle
    # owner/Lillux. A shell PID/name check cannot pin a process incarnation and
    # cannot inhibit supervisor restart or settle a retained worker scope.
    # There is deliberately no numeric-PID kill or predecessor-client fallback.
    ryeos_user 30 stop --force || \
        die "node shutdown failed; refusing binary replacement (no direct-kill fallback)"

    final_status="$(ryeos_status_quick)" || \
        die "cannot verify stopped node; refusing binary replacement"
    grep -Eq '^initialized, stopped' <<<"$final_status" || \
        die "node did not remain stopped; refusing binary replacement"

    return 0
}

# Shell owns external installation sequencing only. Protected association,
# durable inhibition and whole-tree/image proofs remain in ryeos-node/Lillux.
# Use the selected package's entrypoint so the pre-stop checks are the same
# implementation being installed; no predecessor-command fallback.
host_upgrade_command() {
    [[ -n "${RYEOS_INSTALL_TRANSACTION_FD:-}" ]] || \
        die "host upgrade requires the retained package installation transaction"
    "$target_dir/ryeosd" host-install --package-root "$share_dir" validate \
        --transaction-fd "$RYEOS_INSTALL_TRANSACTION_FD" || \
        die "host upgrade lost its exact package installation transaction"
    if [[ $(id -u) -eq 0 ]]; then
        "$target_dir/ryeosd" host-upgrade --app-root "$state_root" \
            --expected-daemon-path "$bin_dir/ryeosd" "$@"
    else
        sudo "$target_dir/ryeosd" host-upgrade --app-root "$state_root" \
            --expected-daemon-path "$bin_dir/ryeosd" "$@"
    fi
}

prepare_host_upgrade() {
    host_upgrade_mode="$(host_upgrade_command --inspect)" || \
        die "cannot inspect host service association; node lifecycle was not changed"
    case "$host_upgrade_mode" in
        direct) return 0 ;;
        supervised) ;;
        *) die "invalid host installation mode; refusing replacement" ;;
    esac
    [[ $restart_daemon -eq 1 && $run_init -eq 1 ]] || \
        die "supervised installation requires lifecycle management and installed-state verification; omit --no-daemon-restart and --no-init"
    command -v ryeos >/dev/null 2>&1 || die "supervised installation requires the installed lifecycle client"
    command -v sha256sum >/dev/null 2>&1 || die "cannot measure staged daemon image before shutdown"
    host_upgrade_digest="$(sha256sum -- "$target_dir/ryeosd")" || die "cannot hash staged daemon"
    host_upgrade_digest="${host_upgrade_digest%% *}"
    host_upgrade_desired="$(host_upgrade_command --expected-daemon-sha256 "$host_upgrade_digest" begin)" || \
        die "cannot establish durable host upgrade inhibition; refusing replacement"
    case "$host_upgrade_desired" in
        up|down) ;;
        *) die "invalid retained host intent; upgrade remains inhibited" ;;
    esac
}

# Keep the policy helpers sourceable by their lightweight regression script.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
# Retain the exact caller argument vector. A root-owned daemon process later
# re-execs this same installer while carrying the package transaction lock; it
# must not reconstruct options from shell state or silently change scope.
installer_original_args=("$@")

ryeos_term_init
install_started="$(_ryeos_term_now)"
verification_skipped=0

# Shared bundle-set definition (one source of truth with populate-bundles.sh).
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$script_dir/bundle-sets.sh"

run_populate=0
run_init=1
restart_daemon=1
cleanup_shadows=1
trust_source_publishers=0
reset_node_policy_generation=0
key="$repo_root/.dev-keys/PUBLISHER_DEV.pem"
owner="RyeOS Development"
bundle_set="full"
node_profile_override=""
jobs=""            # forwarded to populate as cargo -j N
cargo_target_root="$repo_root/target"
crates=""          # forwarded to populate to rebuild only these Cargo packages
populate_all=0     # explicit opt-in to rebuild the whole bundle set
init_app_root="${RYEOS_APP_ROOT:-}"
app_root_argument_present=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --populate)
            run_populate=1
            shift
            ;;
        --no-init)
            run_init=0
            shift
            ;;
        --no-daemon-restart)
            restart_daemon=0
            shift
            ;;
        --keep-shadows)
            cleanup_shadows=0
            shift
            ;;
        --trust-source-publishers)
            trust_source_publishers=1
            shift
            ;;
        --reset-node-policy-generation)
            reset_node_policy_generation=1
            shift
            ;;
        --app-root)
            [[ $# -ge 2 && -n "$2" ]] || die "--app-root requires a path"
            [[ "$2" == /* ]] || die "--app-root requires an absolute path"
            if [[ -n "$init_app_root" && "$init_app_root" != "$2" ]]; then
                die "--app-root contradicts RYEOS_APP_ROOT"
            fi
            init_app_root="$2"
            app_root_argument_present=1
            shift 2
            ;;
        --key)
            [[ $# -ge 2 ]] || die "--key requires a path"
            key="$2"
            shift 2
            ;;
        --owner)
            [[ $# -ge 2 ]] || die "--owner requires a label"
            owner="$2"
            shift 2
            ;;
        --bundle-set)
            [[ $# -ge 2 ]] || die "--bundle-set requires a value"
            bundle_set="$2"
            shift 2
            ;;
        --node-profile)
            [[ $# -ge 2 ]] || die "--node-profile requires a value"
            node_profile_override="$2"
            shift 2
            ;;
        --jobs)
            [[ $# -ge 2 ]] || die "--jobs requires a number"
            jobs="$2"
            shift 2
            ;;
        --cargo-target-dir)
            [[ $# -ge 2 && -n "$2" ]] || die "--cargo-target-dir requires a path"
            [[ "$2" == /* ]] || die "--cargo-target-dir requires an absolute path"
            cargo_target_root="${2%/}"
            shift 2
            ;;
        --crates)
            [[ $# -ge 2 ]] || die "--crates requires a space-separated Cargo package list"
            crates="$2"
            shift 2
            ;;
        --all)
            populate_all=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

if [[ -n "$init_app_root" && "$init_app_root" != /* ]]; then
    die "selected app root must be an absolute path"
fi
# Environment selection is accepted at the unprivileged entrypoint for normal
# RyeOS CLI consistency, but is converted into an explicit argument before the
# sanitized administrator-owned re-exec. The privileged process never relies
# on inheriting RYEOS_APP_ROOT.
if [[ -n "$init_app_root" && $app_root_argument_present -eq 0 ]]; then
    installer_original_args+=(--app-root "$init_app_root")
fi

cd "$repo_root"

if [[ -n "$crates" && $populate_all -eq 1 ]]; then
    die "--crates and --all are mutually exclusive build scopes"
fi
if [[ $run_populate -eq 0 && ( -n "$crates" || $populate_all -eq 1 ) ]]; then
    die "--crates and --all require --populate"
fi
if [[ $reset_node_policy_generation -eq 1 && $run_init -eq 0 ]]; then
    die "--reset-node-policy-generation cannot be combined with --no-init"
fi
if [[ -n "$node_profile_override" && $run_init -eq 0 ]]; then
    die "--node-profile cannot be combined with --no-init"
fi

bundle_names=()
while IFS= read -r _bundle_name; do
    bundle_names+=("$_bundle_name")
done < <(ryeos_bundle_set_names "$bundle_set") || true
if [[ ${#bundle_names[@]} -eq 0 ]]; then
    die "unknown or non-installable --bundle-set: $bundle_set"
fi
if [[ -n "$node_profile_override" ]]; then
    node_init_profile="$node_profile_override"
else
    if ! node_init_profile="$(ryeos_bundle_set_node_init_profile "$bundle_set")"; then
        die "could not resolve default node init profile for bundle set: $bundle_set"
    fi
fi
[[ -n "$node_init_profile" ]] || die "bundle set has no explicit node init profile: $bundle_set"
if ! node_profile_bundle_set="$(ryeos_node_init_profile_bundle_set "$node_init_profile")"; then
    die "unsupported --node-profile: $node_init_profile"
fi
if [[ "$node_profile_bundle_set" != "$bundle_set" ]]; then
    die "node init profile '$node_init_profile' requires bundle set '$node_profile_bundle_set', not '$bundle_set'"
fi
bundle_names_csv=$(IFS=,; printf '%s\n' "${bundle_names[*]}")

# Every selected source bundle must already carry its exact closed manifest and
# object graph. Source-only optional bundles are not exempt from install
# integrity merely because they stage no Rust binary.
closed_payload_bundle_names=("${bundle_names[@]}")

if [[ "$bundle_set" != "full" && $run_init -eq 0 ]]; then
    ryeos_term_warn "--no-init installs lean sources only; existing local initialized state is not rewritten"
fi

bin_dir="/usr/bin"
share_dir="/usr/share/ryeos"
doc_dir="/usr/share/doc/ryeos"
target_dir="$cargo_target_root/release"
install_transaction_active=0

# Only a root-owned Lillux lock on the exact shared package namespace permits
# a re-exec'd installer to skip duplicate population. Environment text alone
# is never trusted: validate the inherited lock descriptor before using the
# prepared marker. This covers the whole replacement transaction, while the
# node's short service gate remains a separate lifecycle authority.
if [[ "${RYEOS_INSTALL_PREPARED:-}" == 1 ]]; then
    [[ -n "${RYEOS_INSTALL_TRANSACTION_FD:-}" ]] || \
        die "prepared installation is missing its inherited package transaction"
    "$target_dir/ryeosd" host-install --package-root "$share_dir" validate \
        --transaction-fd "$RYEOS_INSTALL_TRANSACTION_FD" || \
        die "prepared installation has no valid package transaction"
    install_transaction_active=1
fi

# Only user-facing binaries go in /usr/bin/.
# All handler/runtime/tool binaries live inside bundles under
# /usr/share/ryeos/<name>/.ai/bin/<triple>/ and are resolved
# via bin: references at dispatch time.
required_bins=(
    ryeosd
    ryeos
)

# A complete `--populate --all` now builds lillux with the other user-facing
# binaries. Focused population may legitimately retain no prior lillux build,
# so this direct-copy development helper still treats it as optional rather
# than broadening a targeted repair into another package build.
optional_bins=(lillux)
installed_user_bins=("${required_bins[@]}")

if [[ $run_populate -eq 1 && $install_transaction_active -eq 0 ]]; then
    [[ -s "$key" ]] || die "publisher key missing or empty: $key"
    # Be explicit about scope — never trigger a full workspace rebuild implicitly.
    if [[ -z "$crates" && $populate_all -eq 0 ]]; then
        die "--populate needs an explicit scope: pass --crates \"<Cargo package ...>\" for a focused rebuild (e.g. --crates ryeosd), or --all to rebuild the whole '$bundle_set' set"
    fi
    ryeos_term_begin INSTALL "populating bundles"
    populate_args=(--key "$key" --owner "$owner" --bundle-set "$bundle_set")
    [[ -n "$jobs" ]] && populate_args+=(--jobs "$jobs")
    [[ -n "$crates" ]] && populate_args+=(--crates "$crates")
    [[ $populate_all -eq 1 ]] && populate_args+=(--all)

    # populate-bundles.sh runs `cargo build` and stages binaries into the
    # CHECKOUT (bundles/*/.ai/bin, target/). Those belong to the invoking
    # user, and the build must use that user's toolchain — not root's. When
    # this installer is run under sudo, drop the populate step back to
    # $SUDO_USER through their login shell so their rustup env
    # (CARGO_HOME/RUSTUP_HOME/PATH, sourced from ~/.zshenv etc.) is restored.
    # Otherwise the build runs as root with the wrong toolchain and leaves
    # root-owned artifacts in the checkout that break later user-run
    # cargo/tests. Same reasoning as the `ryeos init` drop below.
    populate_user="${SUDO_USER:-$(id -un)}"
    populate_status=0
    if [[ "$populate_user" != "$(id -un)" ]]; then
        populate_shell="$(getent passwd "$populate_user" | cut -d: -f7)"
        [[ -x "$populate_shell" ]] || populate_shell="/bin/sh"
        if [[ -n "${CARGO:-}" ]]; then
            printf -v populate_cmd 'cd %q && exec env CARGO_TARGET_DIR=%q CARGO=%q %q' \
                "$repo_root" "$cargo_target_root" "$CARGO" "$repo_root/scripts/populate-bundles.sh"
        else
            printf -v populate_cmd 'cd %q && exec env CARGO_TARGET_DIR=%q %q' \
                "$repo_root" "$cargo_target_root" "$repo_root/scripts/populate-bundles.sh"
        fi
        for a in "${populate_args[@]}"; do printf -v populate_cmd '%s %q' "$populate_cmd" "$a"; done
        ryeos_term_note "running bundle population as $populate_user"
        ryeos_term_suspend
        sudo -H -u "$populate_user" "$populate_shell" -lc "$populate_cmd" || populate_status=$?
    else
        ryeos_term_suspend
        env CARGO_TARGET_DIR="$cargo_target_root" \
            "$repo_root/scripts/populate-bundles.sh" "${populate_args[@]}" || populate_status=$?
    fi
    if (( populate_status != 0 )); then
        ryeos_term_end failure "INSTALL FAILED" "populating bundles · exit status $populate_status"
        exit "$populate_status"
    fi
    ryeos_term_end success "INSTALL" "bundles populated"
elif [[ $run_populate -eq 1 ]]; then
    ryeos_term_info "reusing source closure prepared inside the retained package transaction"
fi

source_root_trust_doc="$repo_root/bundles/.ai/PUBLISHER_TRUST.toml"
if [[ $run_init -eq 1 ]]; then
    official_publisher_fp="$(bash "$repo_root/scripts/release/official-publisher-fingerprint.sh")" \
        || die "could not resolve the official publisher fingerprint"
    source_trust_docs=("$source_root_trust_doc")
    for name in "${bundle_names[@]}"; do
        source_trust_doc="$repo_root/bundles/$name/PUBLISHER_TRUST.toml"
        [[ -f "$source_trust_doc" ]] && source_trust_docs+=("$source_trust_doc")
    done
    validate_selected_source_publisher_trust \
        "$trust_source_publishers" "$official_publisher_fp" "${source_trust_docs[@]}" \
        || die "source publisher trust policy rejected initialization"
fi

# A failed/interrupted population can leave authored source plus binaries but
# no derived manifest/object closure. Installing that tree would destroy a
# previously bootable installed payload and can never pass prospective init.
# Refuse before stopping the daemon or replacing anything; only the publisher
# build may recreate this closed evidence.
require_closed_source_bundle_payloads "$repo_root" "${closed_payload_bundle_names[@]}" \
    || die "selected source bundle set is incomplete"

# Validate the complete shared source-root profile closure before stopping a
# live daemon or replacing installed files. Every distribution carries the
# closed catalog. Its default profile is same-named; an explicit profile may
# intentionally select different policy over the exact same bundle set.
for name in "${bundle_names[@]}"; do
    [[ -d "$repo_root/bundles/$name/.ai" ]] || die "missing bundles/$name/.ai"
done
[[ -d "$repo_root/bundles/.ai" && ! -L "$repo_root/bundles/.ai" ]] || \
    die "missing or unsafe source-root seed data: bundles/.ai"
[[ -f "$repo_root/bundles/.ai/PUBLISHER_TRUST.toml" && ! -L "$repo_root/bundles/.ai/PUBLISHER_TRUST.toml" ]] || \
    die "missing or unsafe source-root trust doc: bundles/.ai/PUBLISHER_TRUST.toml"
ryeos_validate_node_init_root "$repo_root/bundles/.ai/node/init" || \
    die "source-root node init namespace is not closed"
node_init_profile_dir="$repo_root/bundles/.ai/node/init/profiles"
[[ -d "$node_init_profile_dir" && ! -L "$node_init_profile_dir" ]] || \
    die "missing or unsafe source-root node init-profile directory: $node_init_profile_dir"
unsupported_node_init_profile="$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
[[ -z "$unsupported_node_init_profile" ]] || \
    die "source-root node init-profile inventory contains an unsafe entry: $unsupported_node_init_profile"
hardlinked_node_init_profile="$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 -type f -links +1 -print -quit)"
[[ -z "$hardlinked_node_init_profile" ]] || \
    die "source-root node init-profile inventory contains a multiply-linked file: $hardlinked_node_init_profile"
expected_node_init_profiles="$(ryeos_node_init_profile_file_names | sort)"
actual_node_init_profiles="$(find "$node_init_profile_dir" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)"
[[ "$actual_node_init_profiles" == "$expected_node_init_profiles" ]] || \
    die "source-root node init-profile inventory is incomplete or unsupported"
while IFS= read -r node_init_profile_name; do
    ryeos_validate_node_init_profile \
        "$node_init_profile_name" "$node_init_profile_dir/$node_init_profile_name.yaml" || \
        die "source-root node init-profile contract is invalid: $node_init_profile_name"
done < <(ryeos_node_init_profile_names)

# Reject an invalid profile-selection request before shutdown or package writes.
# This selects arguments only; init's existing locked prospective-generation
# compiler remains the authority for signed policy validation/publication.
state_root="${init_app_root:-$invoking_user_home/.local/share/ryeos}"
if [[ $run_init -eq 1 ]]; then
    policy_generation_path="$state_root/.ai/node/policies"
    if [[ $reset_node_policy_generation -eq 1 ]]; then
        [[ -e "$policy_generation_path" && ! -L "$policy_generation_path" ]] || \
            die "--reset-node-policy-generation requires an existing safe policy generation"
    elif [[ -n "$node_profile_override" \
        && ( -e "$policy_generation_path" || -L "$policy_generation_path" ) ]]; then
        die "--node-profile selects first publication only; use --reset-node-policy-generation to replace an existing generation explicitly"
    fi
    build_install_init_profile_args \
        "$policy_generation_path" "$node_init_profile" "$reset_node_policy_generation" || \
        die "could not resolve init-profile arguments"
    if ! preflight_existing_node_policy \
        "$policy_generation_path" "$reset_node_policy_generation"; then
        if [[ $reset_node_policy_generation -eq 1 ]]; then
            die "existing node policy generation is not a complete signed schema-cut occupant; node lifecycle was not changed"
        fi
        die "existing node policy generation is incompatible with this RyeOS build; rerun with --reset-node-policy-generation to explicitly replace it before lifecycle shutdown"
    fi
fi

preflight_host_install "$target_dir" "${required_bins[@]}" || \
    die "host install preflight failed; node lifecycle was not changed"

# Serialise the entire shared `/usr/share/ryeos` replacement, rather than only
# the brief per-node stop/start window. The first pass completes all expensive
# unprivileged build/closure checks before this root-owned entry acquires the
# namespace lock. It then execs this script with an inherited descriptor; the
# second pass validates it before any package mutation or skipped population.
if [[ $install_transaction_active -eq 0 ]]; then
    ryeos_term_info "acquiring exclusive shared package installation transaction"
    installer_digest="$(sha256sum -- "$script_dir/install-local-direct.sh")" || \
        die "cannot measure the selected installer script"
    installer_digest="${installer_digest%% *}"
    if [[ $(id -u) -eq 0 ]]; then
        exec "$target_dir/ryeosd" host-install --package-root "$share_dir" acquire \
            --installer "$script_dir/install-local-direct.sh" \
            --installer-digest "$installer_digest" --prepared -- \
            "${installer_original_args[@]}"
    else
        exec sudo "$target_dir/ryeosd" host-install --package-root "$share_dir" acquire \
            --installer "$script_dir/install-local-direct.sh" \
            --installer-digest "$installer_digest" --prepared -- \
            "${installer_original_args[@]}"
    fi
fi

daemon_was_running=0
prepare_host_upgrade
if [[ "$host_upgrade_mode" == supervised ]]; then
    ryeos_term_info "stopping supervised node under durable installation inhibition"
    ryeos_term_suspend
    ryeos_user 30 stop --force || die "supervised shutdown failed; upgrade remains inhibited"
    host_upgrade_command --expected-daemon-sha256 "$host_upgrade_digest" replacement-safe || \
        die "host process tree is not settled; refusing replacement and retaining upgrade inhibition"
    [[ "$host_upgrade_desired" != up ]] || daemon_was_running=1
elif [[ $restart_daemon -eq 1 ]] && command -v ryeos >/dev/null 2>&1; then
    if stop_daemon_for_install; then
        daemon_was_running=1
    fi
fi

# Clean up stale bundle binaries from /usr/bin/.
# Previous installs placed handler/runtime/tool binaries there;
# they now live exclusively inside bundles under /usr/share/ryeos/.
stale_bins=(
    ryeos-core-tools
    ryeos-session-exec
    ryeos-tui
    ryeos-directive-runtime
    ryeos-directive-launch-preparer
    ryeos-direct-execution-evidence
    ryeos-graph-launch-preparer
    ryeos-graph-execution-evidence
    ryeos-graph-runtime
    ryeos-knowledge-runtime
    rye-parser-yaml-document
    rye-parser-yaml-header-document
    rye-parser-regex-kv
    rye-composer-extends-chain
    rye-composer-graph-permissions
    ryeos-graph-effective-validator
    rye-composer-identity
)
for b in "${stale_bins[@]}"; do
    if [[ -e "$bin_dir/$b" ]]; then
        ryeos_term_note "removing stale bundle binary: $bin_dir/$b"
        sudo rm -f "$bin_dir/$b"
    fi
done

ryeos_term_begin INSTALL "installing binaries"
for b in "${required_bins[@]}"; do
    sudo install -Dm755 "$target_dir/$b" "$bin_dir/$b"
done
for b in "${optional_bins[@]}"; do
    if [[ -x "$target_dir/$b" ]]; then
        sudo install -Dm755 "$target_dir/$b" "$bin_dir/$b"
        installed_user_bins+=("$b")
    else
        ryeos_term_note "optional binary not built, skipping: $b"
    fi
done

ryeos_term_update "installing bundle sources" "$share_dir"
sudo mkdir -p "$share_dir"
sudo rm -rf "$share_dir/.ai"
sudo cp -a "$repo_root/bundles/.ai" "$share_dir/.ai"
for path in "$share_dir"/*; do
    [[ -d "$path/.ai" ]] || continue
    name="$(basename "$path")"
    keep=0
    for bundle_name in "${bundle_names[@]}"; do
        if [[ "$name" == "$bundle_name" ]]; then
            keep=1
            break
        fi
    done
    if [[ $keep -eq 0 ]]; then
        ryeos_term_note "removing stale bundle source: $path"
        sudo rm -rf "$path"
    fi
done
for name in "${bundle_names[@]}"; do
    bundle_dir="$repo_root/bundles/$name"
    [[ -d "$bundle_dir/.ai" ]] || continue
    sudo rm -rf "$share_dir/$name"
    sudo mkdir -p "$share_dir/$name"
    sudo cp -a "$bundle_dir/.ai" "$share_dir/$name/.ai"
    if [[ -f "$bundle_dir/PUBLISHER_TRUST.toml" ]]; then
        sudo install -Dm644 "$bundle_dir/PUBLISHER_TRUST.toml" \
            "$share_dir/$name/PUBLISHER_TRUST.toml"
    fi
    if [[ -f "$bundle_dir/README.md" ]]; then
        sudo install -Dm644 "$bundle_dir/README.md" \
            "$doc_dir/$name/README.md"
    fi
    for pinned_contract in "$bundle_dir"/PINNED-*.md; do
        [[ -f "$pinned_contract" ]] || continue
        sudo install -Dm644 "$pinned_contract" \
            "$doc_dir/$name/$(basename "$pinned_contract")"
        [[ -s "$doc_dir/$name/$(basename "$pinned_contract")" ]] || \
            die "failed to install $name pinned workload contract"
    done
done
sudo chown -R root:root "$share_dir"

if [[ $cleanup_shadows -eq 1 ]]; then
    ryeos_term_update "moving PATH shadows" "preserving stale entries"
    stamp="$(date +%Y%m%d%H%M%S)"
    user_backup_dir="$invoking_user_home/.local/bin/ryeos-shadow-backups-$stamp"
    made_user_backup=0
    for b in "${installed_user_bins[@]}"; do
        if [[ -e "/usr/local/bin/$b" || -L "/usr/local/bin/$b" ]]; then
            sudo mv "/usr/local/bin/$b" "/usr/local/bin/$b.bak.$stamp"
        fi
        if [[ -e "$invoking_user_home/.local/bin/$b" || -L "$invoking_user_home/.local/bin/$b" ]]; then
            if [[ $made_user_backup -eq 0 ]]; then
                sudo -H -u "$invoking_user" mkdir -p "$user_backup_dir"
                made_user_backup=1
            fi
            sudo -H -u "$invoking_user" mkdir -p "$user_backup_dir/local-bin"
            sudo -H -u "$invoking_user" mv \
                "$invoking_user_home/.local/bin/$b" "$user_backup_dir/local-bin/$b"
        fi
        if [[ -e "$invoking_user_cargo_home/bin/$b" || -L "$invoking_user_cargo_home/bin/$b" ]]; then
            if [[ $made_user_backup -eq 0 ]]; then
                sudo -H -u "$invoking_user" mkdir -p "$user_backup_dir"
                made_user_backup=1
            fi
            sudo -H -u "$invoking_user" mkdir -p "$user_backup_dir/cargo-bin"
            sudo -H -u "$invoking_user" mv \
                "$invoking_user_cargo_home/bin/$b" "$user_backup_dir/cargo-bin/$b"
        fi
    done
fi

hash -r 2>/dev/null || true

for b in "${installed_user_bins[@]}"; do
    resolved="$(command -v "$b" || true)"
    if [[ "$resolved" != "$bin_dir/$b" ]]; then
        type -a "$b" 2>/dev/null || true
        die "expected $b on PATH to resolve to $bin_dir/$b, got: ${resolved:-not found}"
    fi
done

if [[ $run_init -eq 1 ]]; then
    # The node lives in the INVOKING USER's XDG data dir, not root's. Run init as that
    # user so ryeos's own app-root resolution (RYEOS_APP_ROOT > BaseDirs data dir) picks
    # the right node and writes user-owned state. Never init under sudo: $HOME would be
    # /root and XDG would be scrubbed — that is what silently sent the node to /root.
    init_as=()
    [[ "$invoking_user" != "$(id -un)" ]] && init_as=(sudo -H -u "$invoking_user")
    ryeos_term_update "initializing node state" "user $invoking_user"
    if [[ $reset_node_policy_generation -eq 1 ]]; then
        ryeos_term_note "replacing obsolete signed node policy generation during initialization"
    elif [[ $INSTALL_PUBLISH_INITIAL_POLICY -eq 1 ]]; then
        ryeos_term_note "publishing initial signed node policy generation"
    else
        ryeos_term_note "preserving existing signed node policy generation"
    fi
    trust_args=()
    if [[ $trust_source_publishers -eq 1 ]]; then
        # Pin only the source-root and selected bundle documents validated
        # above. A broad share-dir glob could import an unrelated residual
        # document that was never part of this install's trust decision.
        collect_selected_source_trust_args "$share_dir" "${bundle_names[@]}" || \
            die "could not collect selected source publisher documents"
        trust_args=("${SELECTED_SOURCE_TRUST_ARGS[@]}")
    fi
    init_args=(init --non-interactive --source "$share_dir")
    if [[ -n "$init_app_root" ]]; then
        init_args+=(--app-root "$init_app_root")
    fi
    init_args+=("${INSTALL_INIT_PROFILE_ARGS[@]}")
    init_status=0
    ryeos_term_suspend
    "${init_as[@]}" ryeos "${init_args[@]}" "${trust_args[@]}" || init_status=$?
    if (( init_status != 0 )); then
        ryeos_term_end failure "INSTALL FAILED" "initializing node state · exit status $init_status"
        exit "$init_status"
    fi

    ryeos_term_end success INSTALL "node state initialized"
    ryeos_term_begin VERIFY "initialized bundle state"
    for name in "${bundle_names[@]}"; do
        test -d "$state_root/.ai/bundles/$name/.ai" || \
            die "initialized $name bundle missing from $state_root"
    done
    if [[ $INSTALL_PUBLISH_INITIAL_POLICY -eq 1 ]]; then
        selected_node_init_profile="$share_dir/.ai/node/init/profiles/$node_init_profile.yaml"
        expected_node_policies="$(
            ryeos_node_init_profile_policy_names "$selected_node_init_profile" \
                | sed 's/$/.yaml/' \
                | sort
        )"
        node_policy_dir="$state_root/.ai/node/policies"
        [[ -n "$expected_node_policies" && -d "$node_policy_dir" && ! -L "$node_policy_dir" ]] || \
            die "selected node init profile was not materialized under $node_policy_dir"
        unsafe_node_policy="$(find "$node_policy_dir" -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
        [[ -z "$unsafe_node_policy" ]] || \
            die "materialized node-policy generation contains an unsafe entry: $unsafe_node_policy"
        actual_node_policies="$(find "$node_policy_dir" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)"
        [[ "$actual_node_policies" == "$expected_node_policies" ]] || \
            die "materialized node-policy generation does not match $node_init_profile"
    else
        node_policy_dir="$policy_generation_path"
        [[ -d "$node_policy_dir" && ! -L "$node_policy_dir" ]] || \
            die "existing node-policy generation is not a safe directory: $node_policy_dir"
        unsafe_node_policy="$(find "$node_policy_dir" -mindepth 1 -maxdepth 1 ! -type f -print -quit)"
        [[ -z "$unsafe_node_policy" ]] || \
            die "existing node-policy generation contains an unsafe entry: $unsafe_node_policy"
        actual_node_policies="$(find "$node_policy_dir" -mindepth 1 -maxdepth 1 -type f -printf '%f\n' | sort)"
        [[ -n "$actual_node_policies" ]] || \
            die "existing node-policy generation is empty: $node_policy_dir"
    fi
    if [[ "$bundle_set" == "hosted-node" ]]; then
        for name in standard ryeos-ui web browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized hosted-node state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized hosted-node state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "standard" ]]; then
        for name in hosted-node ryeos-ui web browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized standard state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized standard state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "central-host" ]]; then
        # central-host is standard + web; it must NOT drag in the ryeos-ui/browser
        # UI bundles or the hosted-node control plane.
        for name in hosted-node ryeos-ui browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized central-host state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized central-host state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "full" ]]; then
        grep -q '^  execute: client:ryeos/tui$' \
            "$state_root/.ai/bundles/ryeos-ui/.ai/node/commands/tui.yaml" || \
            die "initialized tui command is stale or not client-backed"
    fi

    # ── Verify installed bundle signatures (offline doctor --strict) ──
    # Closes the "edited YAML, forgot to re-sign, discover at runtime" loop:
    # run the same preflight verification `ryeos doctor` wraps against every
    # installed bundle and fail the install on any red check. Offline, no daemon.
    core_tools_bin="$share_dir/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools"
    if [[ -x "$core_tools_bin" ]]; then
        ryeos_term_update "installed bundle signatures" "doctor --strict"
        verify_status=0
        doctor_output="$(mktemp)"
        for name in "${bundle_names[@]}"; do
            if [[ "$invoking_user" != "$(id -un)" ]]; then
                doctor_status=0
                # The installer owns the capture file; only the doctor process
                # drops to the invoking user. The root shell must perform this
                # redirection so the unprivileged child never needs write
                # authority over the installer-created temporary file.
                # shellcheck disable=SC2024
                sudo -H -u "$invoking_user" env RYEOS_APP_ROOT="$state_root" \
                    "$core_tools_bin" doctor "$share_dir/$name" --strict >"$doctor_output" || doctor_status=$?
                if (( doctor_status != 0 )); then
                    ryeos_term_fail "doctor failed for bundle: $name"
                    sed 's/^/   /' "$doctor_output" >&2
                    (( verify_status == 0 )) && verify_status="$doctor_status"
                fi
            else
                doctor_status=0
                RYEOS_APP_ROOT="$state_root" \
                    "$core_tools_bin" doctor "$share_dir/$name" --strict >"$doctor_output" || doctor_status=$?
                if (( doctor_status != 0 )); then
                    ryeos_term_fail "doctor failed for bundle: $name"
                    sed 's/^/   /' "$doctor_output" >&2
                    (( verify_status == 0 )) && verify_status="$doctor_status"
                fi
            fi
        done
        rm -f "$doctor_output"
        if (( verify_status != 0 )); then
            ryeos_term_end failure "INSTALL FAILED" "bundle verification · exit status $verify_status"
            exit "$verify_status"
        fi
    else
        ryeos_term_warn "skipping bundle verification: core-tools binary not found at $core_tools_bin"
        verification_skipped=1
    fi
fi

if [[ "$host_upgrade_mode" == supervised ]]; then
    [[ $verification_skipped -eq 0 ]] || die "cannot restore supervised node without installed-state verification"
    host_upgrade_command --expected-daemon-sha256 "$host_upgrade_digest" restore-ready || \
        die "host upgrade restoration refused; journal retained"
fi

if [[ $daemon_was_running -eq 1 ]]; then
    if [[ $run_init -eq 1 ]]; then
        ryeos_term_end success VERIFY "installed bundle state"
    fi
    ryeos_term_info "restarting daemon"
    # `ryeos start` consumes the daemon's lifecycle stream directly, including
    # current-generation rebuild and journal-replay progress. Keep its output
    # visible and retain a small wrapper margin above the CLI's own wait.
    ryeos_term_suspend
    restart_status=0
    ryeos_user 930 start || restart_status=$?
    if (( restart_status != 0 )); then
        ryeos_term_end failure "INSTALL FAILED" "daemon restart · exit status $restart_status"
        exit "$restart_status"
    fi
    daemon_verify_status=0
    ryeos_status_quick | grep -qx "running" || daemon_verify_status=$?
    if (( daemon_verify_status != 0 )); then
        ryeos_term_end failure "INSTALL FAILED" "daemon verification · exit status $daemon_verify_status"
        exit "$daemon_verify_status"
    fi
fi

if [[ "$host_upgrade_mode" == supervised ]]; then
    host_upgrade_command --expected-daemon-sha256 "$host_upgrade_digest" finish || \
        die "restored host generation is unproved; upgrade journal retained"
fi

if [[ $run_init -eq 1 && $daemon_was_running -eq 0 ]]; then
    ryeos_term_end success VERIFY "installed bundle state"
fi
if (( verification_skipped == 1 )); then
    ryeos_term_end warning "INSTALL COMPLETE" "local package layout ready · verification skipped" "$install_started"
else
    ryeos_term_end success "INSTALL COMPLETE" "local package layout ready" "$install_started"
fi
ryeos_term_section "installation"
ryeos_term_row "ryeos" "$(command -v ryeos)"
ryeos_term_row "bundle set" "$bundle_set"
ryeos_term_row "bundle src" "$share_dir/{$bundle_names_csv}"
ryeos_term_row "app root" "${init_app_root:-$invoking_user_home/.local/share/ryeos}"
if [[ $daemon_was_running -eq 1 ]]; then
    ryeos_term_row "daemon" "restarted"
fi
