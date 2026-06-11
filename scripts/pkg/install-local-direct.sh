#!/usr/bin/env bash
# Fast local packaged-layout install from this checkout.
#
# This intentionally skips yay/makepkg but installs the same runtime layout
# as deploy/aur/ryeos/PKGBUILD:
#   - binaries -> /usr/bin
#   - bundle sources -> /usr/share/ryeos/{core,standard,studio,web,hosted-node}
#     or, with --bundle-set standard, /usr/share/ryeos/{core,standard};
#     or, with --bundle-set hosted-node, /usr/share/ryeos/{core,hosted-node};
#     with --bundle-set hosted-workflow, /usr/share/ryeos/{core,standard,hosted-node}
#   - ryeos init copies bundle sources into ~/.local/share/ryeos
#
# Use the AUR flow for package-manager ownership. Use this script for fast
# local repair/testing when you explicitly want to bypass the package build.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/pkg/install-local-direct.sh [options]

Fast-install the current checkout using the packaged RyeOS layout:
  /usr/bin/ryeos
  /usr/share/ryeos/{core,standard,studio,web,hosted-node}/.ai
  ~/.local/share/ryeos/.ai/bundles/{core,standard,studio,web,hosted-node}  (after init)

Options:
  --populate            Run scripts/populate-bundles.sh first (expensive; rebuilds
                        bundle-owned release binaries and republishes bundles)
  --no-init             Install files but do not run ryeos init
  --no-daemon-restart   Do not stop/restart an already-running daemon
  --keep-shadows        Do not move /usr/local/bin or ~/.local/bin RyeOS shadows
  --key PATH            Publisher key for populate-bundles.sh
                        (default: .dev-keys/PUBLISHER_DEV.pem)
  --owner LABEL         Owner label for populate-bundles.sh
                        (default: ryeos-dev)
  --bundle-set SET      Bundle set to populate/install: full, standard
                        (core+standard), hosted-node, or hosted-workflow
                        (core+standard+hosted-node)
                        (default: full)
  -h, --help            Show this help

Default behavior is incremental: install already-built binaries and bundle
sources, stop any already-running daemon, move stale PATH shadows aside, run
ryeos init with the installed PUBLISHER_TRUST.toml files, then restart the
daemon if it was running before the install. Pass --populate only when bundle
artifacts actually need to be regenerated.
EOF
}

die() {
    echo "install-local-direct.sh: $*" >&2
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

ryeos_status_quick() {
    run_timeout 10 ryeos node status 2>/dev/null || true
}

pid_from_status() {
    awk '/^pid:/ { print $2; exit }'
}

stop_daemon_for_install() {
    local status_out pid final_status

    status_out="$(ryeos_status_quick)"
    if ! grep -qx "running" <<<"$status_out"; then
        return 1
    fi

    echo "[install-local-direct] stopping running daemon before replacing binaries"
    if ! run_timeout 30 ryeos stop --force >/dev/null; then
        echo "[install-local-direct] ryeos stop timed out or failed; falling back to direct process kill" >&2
        pid="$(pid_from_status <<<"$status_out")"
        if [[ -n "$pid" && "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            for _ in {1..30}; do
                kill -0 "$pid" 2>/dev/null || break
                sleep 0.2
            done
            if kill -0 "$pid" 2>/dev/null; then
                kill -9 "$pid" 2>/dev/null || true
            fi
        fi
    fi

    final_status="$(ryeos_status_quick)"
    if grep -qx "running" <<<"$final_status"; then
        die "failed to stop running daemon before install"
    fi

    return 0
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_populate=0
run_init=1
restart_daemon=1
cleanup_shadows=1
key="$repo_root/.dev-keys/PUBLISHER_DEV.pem"
owner="ryeos-dev"
bundle_set="full"

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
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

cd "$repo_root"

case "$bundle_set" in
    full)
        bundle_names=(core standard studio web hosted-node)
        ;;
    standard)
        bundle_names=(core standard)
        ;;
    hosted-node)
        bundle_names=(core hosted-node)
        ;;
    hosted-workflow)
        bundle_names=(core standard hosted-node)
        ;;
    *)
        die "--bundle-set must be 'full', 'standard', 'hosted-node', or 'hosted-workflow', got: $bundle_set"
        ;;
esac
bundle_names_csv=$(IFS=,; echo "${bundle_names[*]}")

if [[ "$bundle_set" != "full" && $run_init -eq 0 ]]; then
    echo "[install-local-direct] warning: --no-init installs lean sources only; existing local initialized state is not rewritten" >&2
fi

bin_dir="/usr/bin"
share_dir="/usr/share/ryeos"
doc_dir="/usr/share/doc/ryeos"
target_dir="$repo_root/target/release"
init_app_root="${RYEOS_APP_ROOT:-}"

# Only user-facing binaries go in /usr/bin/.
# All handler/runtime/tool binaries live inside bundles under
# /usr/share/ryeos/<name>/.ai/bin/<triple>/ and are resolved
# via bin: references at dispatch time.
required_bins=(
    ryeosd
    ryeos
)

# PKGBUILD installs lillux when a full package build has produced it, but
# populate-bundles.sh does not currently build it. Treat it as optional for
# this fast direct-copy helper so the CLI/init path is not blocked.
optional_bins=(lillux)

if [[ $run_populate -eq 1 ]]; then
    [[ -s "$key" ]] || die "publisher key missing or empty: $key"
    echo "[install-local-direct] populating bundles"
    "$repo_root/scripts/populate-bundles.sh" \
        --key "$key" \
        --owner "$owner" \
        --bundle-set "$bundle_set"
fi

daemon_was_running=0
if [[ $restart_daemon -eq 1 ]] && command -v ryeos >/dev/null 2>&1; then
    if stop_daemon_for_install; then
        daemon_was_running=1
    fi
fi

for b in "${required_bins[@]}"; do
    [[ -x "$target_dir/$b" ]] || die "missing required release binary: $target_dir/$b"
done

# Clean up stale bundle binaries from /usr/bin/.
# Previous installs placed handler/runtime/tool binaries there;
# they now live exclusively inside bundles under /usr/share/ryeos/.
stale_bins=(
    ryeos-core-tools
    ryeos-tui
    ryeos-directive-runtime
    ryeos-graph-runtime
    ryeos-knowledge-runtime
    rye-parser-yaml-document
    rye-parser-yaml-header-document
    rye-parser-regex-kv
    rye-composer-extends-chain
    rye-composer-graph-permissions
    rye-composer-identity
)
for b in "${stale_bins[@]}"; do
    if [[ -e "$bin_dir/$b" ]]; then
        echo "[install-local-direct] removing stale bundle binary: $bin_dir/$b"
        sudo rm -f "$bin_dir/$b"
    fi
done

for name in "${bundle_names[@]}"; do
    [[ -d "$repo_root/bundles/$name/.ai" ]] || die "missing bundles/$name/.ai"
done
[[ -d "$repo_root/bundles/.ai" ]] || die "missing source-root seed data: bundles/.ai"
[[ -f "$repo_root/bundles/.ai/PUBLISHER_TRUST.toml" ]] || \
    die "missing source-root trust doc: bundles/.ai/PUBLISHER_TRUST.toml"
[[ -f "$repo_root/bundles/.ai/node/init/command-registration/default.yaml" ]] || \
    die "missing source-root command-registration seed: bundles/.ai/node/init/command-registration/default.yaml"
[[ -f "$repo_root/bundles/.ai/node/init/bundle-registration-grants/default.yaml" ]] || \
    die "missing source-root bundle-registration-grants seed: bundles/.ai/node/init/bundle-registration-grants/default.yaml"

echo "[install-local-direct] installing binaries -> $bin_dir"
for b in "${required_bins[@]}"; do
    sudo install -Dm755 "$target_dir/$b" "$bin_dir/$b"
done
for b in "${optional_bins[@]}"; do
    if [[ -x "$target_dir/$b" ]]; then
        sudo install -Dm755 "$target_dir/$b" "$bin_dir/$b"
    else
        echo "[install-local-direct] optional binary not built, skipping: $b"
    fi
done

echo "[install-local-direct] installing bundle sources -> $share_dir"
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
        echo "[install-local-direct] removing stale bundle source: $path"
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
done
sudo chown -R root:root "$share_dir"

if [[ $cleanup_shadows -eq 1 ]]; then
    echo "[install-local-direct] moving PATH shadows aside"
    stamp="$(date +%Y%m%d%H%M%S)"
    user_backup_dir="$HOME/.local/bin/ryeos-shadow-backups-$stamp"
    made_user_backup=0
    for b in "${required_bins[@]}" "${optional_bins[@]}"; do
        if [[ -e "/usr/local/bin/$b" || -L "/usr/local/bin/$b" ]]; then
            sudo mv "/usr/local/bin/$b" "/usr/local/bin/$b.bak.$stamp"
        fi
        if [[ -e "$HOME/.local/bin/$b" || -L "$HOME/.local/bin/$b" ]]; then
            if [[ $made_user_backup -eq 0 ]]; then
                mkdir -p "$user_backup_dir"
                made_user_backup=1
            fi
            mv "$HOME/.local/bin/$b" "$user_backup_dir/$b"
        fi
    done
fi

hash -r 2>/dev/null || true

resolved="$(command -v ryeos || true)"
if [[ "$resolved" != "$bin_dir/ryeos" ]]; then
    type -a ryeos 2>/dev/null || true
    die "expected ryeos on PATH to resolve to $bin_dir/ryeos, got: ${resolved:-not found}"
fi

if [[ $run_init -eq 1 ]]; then
    echo "[install-local-direct] running ryeos init from PATH"
    state_root="${init_app_root:-$HOME/.local/share/ryeos}"
    for path in "$state_root/.ai/bundles"/*; do
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
            echo "[install-local-direct] removing stale initialized bundle: $path"
            rm -rf "$path"
        fi
    done
    for path in "$state_root/.ai/node/bundles"/*.yaml; do
        [[ -f "$path" ]] || continue
        name="$(basename "$path" .yaml)"
        keep=0
        for bundle_name in "${bundle_names[@]}"; do
            if [[ "$name" == "$bundle_name" ]]; then
                keep=1
                break
            fi
        done
        if [[ $keep -eq 0 ]]; then
            echo "[install-local-direct] removing stale initialized bundle registration: $path"
            rm -f "$path"
        fi
    done
    trust_args=()
    for trust_file in "$share_dir/.ai/PUBLISHER_TRUST.toml" "$share_dir"/*/PUBLISHER_TRUST.toml; do
        [[ -f "$trust_file" ]] || continue
        trust_args+=(--trust-file "$trust_file")
    done
    init_args=(init --source "$share_dir")
    if [[ -n "$init_app_root" ]]; then
        init_args+=(--app-root "$init_app_root")
    fi
    ryeos "${init_args[@]}" "${trust_args[@]}"

    echo "[install-local-direct] verifying initialized bundle state"
    state_root="${init_app_root:-$HOME/.local/share/ryeos}"
    for name in "${bundle_names[@]}"; do
        test -d "$state_root/.ai/bundles/$name/.ai" || \
            die "initialized $name bundle missing from $state_root"
    done
    if [[ "$bundle_set" == "hosted-node" ]]; then
        for name in standard studio web; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized hosted-node state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized hosted-node state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "standard" ]]; then
        for name in hosted-node studio web; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized standard state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized standard state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "full" ]]; then
        grep -q '^  execute: client:ryeos/tui$' \
            "$state_root/.ai/bundles/studio/.ai/node/commands/tui.yaml" || \
            die "initialized tui command is stale or not client-backed"
    fi
fi

if [[ $daemon_was_running -eq 1 ]]; then
    echo "[install-local-direct] restarting daemon"
    # An incompatible projection schema epoch bump can make the first restart
    # rebuild projection.sqlite3 from CAS/refs before readiness. Give that
    # healthy one-time rebuild enough time to finish. Keep this slightly above
    # ryeos start's internal wait so the CLI can print its own diagnostic.
    run_timeout 930 ryeos start >/dev/null || die "daemon did not restart cleanly"
    ryeos_status_quick | grep -qx "running" || die "daemon did not restart cleanly"
fi

echo
echo "[install-local-direct] complete"
echo "  ryeos:        $(command -v ryeos)"
echo "  bundle set:   $bundle_set"
echo "  bundle src:   $share_dir/{$bundle_names_csv}"
echo "  app root:     ${init_app_root:-$HOME/.local/share/ryeos}"
if [[ $daemon_was_running -eq 1 ]]; then
    echo "  daemon:       restarted"
fi
