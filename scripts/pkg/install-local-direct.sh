#!/usr/bin/env bash
# Fast local packaged-layout install from this checkout.
#
# This intentionally skips yay/makepkg but installs the same runtime layout
# as deploy/aur/ryeos/PKGBUILD:
#   - binaries -> /usr/bin
#   - bundle sources -> /usr/share/ryeos/{core,standard,web}
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
  /usr/share/ryeos/{core,standard,web}/.ai
  ~/.local/share/ryeos/.ai/bundles/{core,standard,web}  (after init)

Options:
  --skip-populate       Do not run scripts/populate-bundles.sh first
  --no-init             Install files but do not run ryeos init
  --no-daemon-restart   Do not stop/restart an already-running daemon
  --keep-shadows        Do not move /usr/local/bin or ~/.local/bin RyeOS shadows
  --key PATH            Publisher key for populate-bundles.sh
                        (default: .dev-keys/PUBLISHER_DEV.pem)
  --owner LABEL         Owner label for populate-bundles.sh
                        (default: ryeos-dev)
  -h, --help            Show this help

Default behavior is safe and complete: populate bundles, install files,
stop any already-running daemon, move stale PATH shadows aside, run
ryeos init with the installed PUBLISHER_TRUST.toml files, then restart
the daemon if it was running before the install.
EOF
}

die() {
    echo "install-local-direct.sh: $*" >&2
    exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"

run_populate=1
run_init=1
restart_daemon=1
cleanup_shadows=1
key="$repo_root/.dev-keys/PUBLISHER_DEV.pem"
owner="ryeos-dev"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-populate)
            run_populate=0
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

bin_dir="/usr/bin"
share_dir="/usr/share/ryeos"
target_dir="$repo_root/target/release"

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
    "$repo_root/scripts/populate-bundles.sh" --key "$key" --owner "$owner"
fi

daemon_was_running=0
if [[ $restart_daemon -eq 1 ]] && command -v ryeos >/dev/null 2>&1; then
    status_out="$(ryeos status 2>/dev/null || true)"
    if grep -qx "running" <<<"$status_out"; then
        daemon_was_running=1
        echo "[install-local-direct] stopping running daemon before replacing binaries"
        ryeos stop --force >/dev/null || die "failed to stop running daemon before install"
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

[[ -d "$repo_root/bundles/core/.ai" ]] || die "missing bundles/core/.ai"
[[ -d "$repo_root/bundles/standard/.ai" ]] || die "missing bundles/standard/.ai"
[[ -d "$repo_root/bundles/web/.ai" ]] || die "missing bundles/web/.ai"
[[ -f "$repo_root/bundles/core/PUBLISHER_TRUST.toml" ]] || die "missing bundles/core/PUBLISHER_TRUST.toml"
[[ -f "$repo_root/bundles/standard/PUBLISHER_TRUST.toml" ]] || die "missing bundles/standard/PUBLISHER_TRUST.toml"
[[ -f "$repo_root/bundles/web/PUBLISHER_TRUST.toml" ]] || die "missing bundles/web/PUBLISHER_TRUST.toml"

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
for bundle_dir in "$repo_root"/bundles/*/; do
    [[ -d "$bundle_dir/.ai" ]] || continue
    name="$(basename "$bundle_dir")"
    sudo rm -rf "$share_dir/$name"
    sudo mkdir -p "$share_dir/$name"
    sudo cp -a "$bundle_dir/.ai" "$share_dir/$name/.ai"
    if [[ -f "$bundle_dir/PUBLISHER_TRUST.toml" ]]; then
        sudo install -Dm644 "$bundle_dir/PUBLISHER_TRUST.toml" \
            "$share_dir/$name/PUBLISHER_TRUST.toml"
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
    trust_args=()
    for trust_file in "$share_dir"/*/PUBLISHER_TRUST.toml; do
        [[ -f "$trust_file" ]] || continue
        trust_args+=(--trust-file "$trust_file")
    done
    ryeos init "${trust_args[@]}"

    echo "[install-local-direct] verifying initialized bundle state"
    test -d "$HOME/.local/share/ryeos/.ai/bundles/core/.ai" || \
        die "initialized core bundle missing from ~/.local/share/ryeos"
    test -d "$HOME/.local/share/ryeos/.ai/bundles/standard/.ai" || \
        die "initialized standard bundle missing from ~/.local/share/ryeos"
    test -d "$HOME/.local/share/ryeos/.ai/bundles/web/.ai" || \
        die "initialized web bundle missing from ~/.local/share/ryeos"
    grep -q '^execute: client:ryeos/tui$' \
        "$HOME/.local/share/ryeos/.ai/bundles/standard/.ai/node/verbs/tui.yaml" || \
        die "initialized tui verb is stale or not client-backed"
fi

if [[ $daemon_was_running -eq 1 ]]; then
    echo "[install-local-direct] restarting daemon"
    ryeos start >/dev/null
    ryeos status | grep -qx "running" || die "daemon did not restart cleanly"
fi

echo
echo "[install-local-direct] complete"
echo "  ryeos:        $(command -v ryeos)"
echo "  bundle src:   $share_dir/{core,standard,web}"
echo "  local state:  $HOME/.local/share/ryeos"
if [[ $daemon_was_running -eq 1 ]]; then
    echo "  daemon:       restarted"
fi
