#!/usr/bin/env bash
# Fast local packaged-layout install from this checkout.
#
# This intentionally skips yay/makepkg but installs the same runtime layout
# as deploy/aur/ryeos/PKGBUILD:
#   - binaries -> /usr/bin
#   - bundle sources -> /usr/share/ryeos/<name> for each bundle in the set.
#     The set membership is the single source of truth in
#     scripts/pkg/bundle-sets.sh (full = core, central-auth, standard, web,
#     browser, studio, hosted-node; the lean sets are subsets).
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
  /usr/share/ryeos/<name>/.ai                      (each bundle in the set)
  ~/.local/share/ryeos/.ai/bundles/<name>          (after init)
Set membership is defined in scripts/pkg/bundle-sets.sh (full = core,
central-auth, standard, web, browser, studio, hosted-node).

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
  --jobs N              Cap cargo build parallelism during --populate (cargo -j N).
                        Use a smaller N if a full release build exhausts memory.
  --crates "A B C"      With --populate, rebuild only these crates (e.g.
                        --crates ryeos-core-tools to refresh just core-tools). Other
                        bundle binaries must already exist in target/release.
  --all                 With --populate, rebuild the whole bundle set. Required to
                        do a full rebuild — --populate refuses to build everything
                        implicitly (that full release build is what exhausts memory).
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

# The user who invoked the installer. Under sudo, lifecycle commands (status/
# stop/start) MUST run as this user, not root: the daemon and its state live
# under the user's XDG data dir, so a root-context `ryeos` resolves root's
# app-root instead — it sees no daemon, so it never stops the stale one and
# never restarts, leaving the old binary running against the swapped-out files.
# Same drop-to-user reasoning as the populate/init steps below.
invoking_user="${SUDO_USER:-$(id -un)}"

# Run `ryeos <args>` with a timeout, as the invoking user when under sudo so it
# targets that user's app-root. `timeout` wraps the external command (sudo or
# ryeos), never a shell function.
ryeos_user() {
    local secs="$1"
    shift
    if [[ "$invoking_user" != "$(id -un)" ]]; then
        local user_shell cmd a
        user_shell="$(getent passwd "$invoking_user" | cut -d: -f7)"
        [[ -x "$user_shell" ]] || user_shell="/bin/sh"
        printf -v cmd 'exec ryeos'
        for a in "$@"; do printf -v cmd '%s %q' "$cmd" "$a"; done
        run_timeout "$secs" sudo -H -u "$invoking_user" "$user_shell" -lc "$cmd"
    else
        run_timeout "$secs" ryeos "$@"
    fi
}

ryeos_status_quick() {
    ryeos_user 10 node status 2>/dev/null || true
}

# Print projection-rebuild progress while the daemon restarts — ONLY when a
# rebuild is actually happening.
#
# A projection schema-epoch bump makes the first restart rebuild
# projection.sqlite3 from the event log before the daemon is ready — minutes of
# silence on a large store that otherwise reads as a hang. The daemon marks a
# real rebuild by renaming the outgoing database to
# `projection.sqlite3.reset.<from>-to-<to>.<ts>.<pid>`; a fresh reset file is
# the detection signal, and its own row count is the progress denominator (the
# rebuild reprojects the same corpus plus the new tail, hence the `~`). A
# restart with no epoch bump produces no reset file and stays silent.
# Strictly best-effort: no `sqlite3` or no readable databases just means no
# progress lines — errexit and pipefail are disabled here so a probe failure
# never touches the install.
report_projection_rebuild() {
    set +e
    local state_dir="$1"
    local db="$state_dir/projection.sqlite3"
    command -v sqlite3 >/dev/null 2>&1 || return 0
    local started_at reset_file="" total="" last="" n pct
    started_at="$(date +%s)"
    while :; do
        sleep 10
        if [[ -z "$reset_file" ]]; then
            reset_file="$(find "$state_dir" -maxdepth 1 \
                -name 'projection.sqlite3.reset.*' -newermt "@$started_at" 2>/dev/null | head -1)"
            [[ -z "$reset_file" ]] && continue
            echo "[install-local-direct]   projection schema epoch changed — one-time" \
                 "rebuild from the event log (minutes on a large store)"
            total="$(sqlite3 -readonly "$reset_file" 'SELECT count(*) FROM events;' 2>/dev/null)"
        fi
        n="$(sqlite3 -readonly "$db" 'SELECT count(*) FROM events;' 2>/dev/null)"
        [[ -z "$n" ]] && continue
        [[ "$n" == "$last" ]] && continue   # no forward motion — stay quiet
        last="$n"
        if [[ -n "$total" && "$total" -gt 0 ]]; then
            pct=$(( n * 100 / total ))
            [[ "$pct" -gt 100 ]] && pct=100
            echo "[install-local-direct]   projection rebuild: $n/~$total events (${pct}%)"
        else
            echo "[install-local-direct]   projection rebuild: $n events"
        fi
    done
}

bundle_payload_bins() {
    case "$1" in
        core)
            printf '%s\n' \
                rye-parser-yaml-document \
                rye-parser-yaml-header-document \
                rye-parser-regex-kv \
                rye-composer-identity \
                ryeos-core-tools
            ;;
        standard)
            printf '%s\n' \
                ryeos-directive-runtime \
                ryeos-graph-runtime \
                ryeos-knowledge-runtime \
                rye-composer-extends-chain \
                rye-composer-graph-permissions
            ;;
        studio)
            printf '%s\n' ryeos-tui web
            ;;
        web)
            printf '%s\n' ryeos-web-tools
            ;;
        browser)
            printf '%s\n' ryeos-browser-tools
            ;;
    esac
}

publisher_fingerprint_from_trust_doc() {
    local trust_file="$1"
    sed -n 's/^fingerprint *= *"\([^"]*\)".*/\1/p' "$trust_file" | head -n1
}

operator_fingerprint() {
    local key_path="${init_app_root:-$HOME/.local/share/ryeos}/.ai/config/keys/signing/private_key.pem"
    [[ -s "$key_path" ]] || return 1
    openssl pkey -in "$key_path" -pubout -outform DER 2>/dev/null \
        | tail -c 32 \
        | sha256sum \
        | cut -d' ' -f1
}

refresh_installed_bundle_payload() {
    local name="$1"
    local dest="$share_dir/$name"
    local bin_dest="$dest/.ai/bin/x86_64-unknown-linux-gnu"
    local bins=()
    local b
    local trust_fp operator_fp

    while IFS= read -r b; do
        [[ -n "$b" ]] && bins+=("$b")
    done < <(bundle_payload_bins "$name")
    [[ ${#bins[@]} -gt 0 ]] || return 0

    [[ -x "$target_dir/ryeos-core-tools" ]] || \
        die "bundle payload refresh requires built binary: $target_dir/ryeos-core-tools"
    [[ -f "$dest/PUBLISHER_TRUST.toml" ]] || \
        die "bundle payload refresh requires trust doc: $dest/PUBLISHER_TRUST.toml"
    trust_fp="$(publisher_fingerprint_from_trust_doc "$dest/PUBLISHER_TRUST.toml")"
    operator_fp="$(operator_fingerprint || true)"
    if [[ -z "$operator_fp" || "$trust_fp" != "$operator_fp" ]]; then
        echo "[install-local-direct] skipping $name bundle payload refresh: installed bundle trusts $trust_fp, operator key is ${operator_fp:-unavailable}; run with --populate to refresh publisher-signed payloads"
        return 0
    fi

    echo "[install-local-direct] refreshing $name bundle payload"
    sudo mkdir -p "$bin_dest"
    for b in "${bins[@]}"; do
        [[ -x "$target_dir/$b" ]] || die "bundle payload binary missing: $target_dir/$b"
        sudo install -Dm755 "$target_dir/$b" "$bin_dest/$b"
    done

    case "$name" in
        core)
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$dest" \
                --registry-root "$share_dir/core" \
                --owner "$owner" >/dev/null
            ;;
        standard)
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$dest" \
                --registry-root "$share_dir/core" \
                --owner "$owner" >/dev/null
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$share_dir/core" \
                --registry-root "$share_dir/core" \
                --registry-root "$share_dir/standard" \
                --owner "$owner" >/dev/null
            ;;
        studio)
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$dest" \
                --registry-root "$share_dir/core" \
                --registry-root "$share_dir/standard" \
                --owner "$owner" >/dev/null
            ;;
        web)
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$dest" \
                --registry-root "$share_dir/core" \
                --owner "$owner" >/dev/null
            ;;
        browser)
            sudo env RYEOS_APP_ROOT="${init_app_root:-$HOME/.local/share/ryeos}" \
                "$target_dir/ryeos-core-tools" build "$dest" \
                --registry-root "$share_dir/core" \
                --owner "$owner" >/dev/null
            ;;
    esac
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
    if ! ryeos_user 30 stop --force >/dev/null; then
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

# Shared bundle-set definition (one source of truth with populate-bundles.sh).
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$script_dir/bundle-sets.sh"

run_populate=0
run_init=1
restart_daemon=1
cleanup_shadows=1
key="$repo_root/.dev-keys/PUBLISHER_DEV.pem"
owner="ryeos-dev"
bundle_set="full"
jobs=""            # forwarded to populate as cargo -j N
crates=""          # forwarded to populate to rebuild only these crates
populate_all=0     # explicit opt-in to rebuild the whole bundle set

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
        --jobs)
            [[ $# -ge 2 ]] || die "--jobs requires a number"
            jobs="$2"
            shift 2
            ;;
        --crates)
            [[ $# -ge 2 ]] || die "--crates requires a space-separated crate list"
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

cd "$repo_root"

bundle_names=()
while IFS= read -r _bundle_name; do
    bundle_names+=("$_bundle_name")
done < <(ryeos_bundle_set_names "$bundle_set") || true
if [[ ${#bundle_names[@]} -eq 0 ]]; then
    die "--bundle-set must be 'full', 'central-host', 'standard', 'hosted-node', or 'hosted-workflow', got: $bundle_set"
fi
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
    # Be explicit about scope — never trigger a full workspace rebuild implicitly.
    if [[ -z "$crates" && $populate_all -eq 0 ]]; then
        die "--populate needs an explicit scope: pass --crates \"<crate ...>\" to rebuild only what changed (e.g. --crates ryeos-core-tools), or --all to rebuild the whole '$bundle_set' set"
    fi
    echo "[install-local-direct] populating bundles"
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
    if [[ "$populate_user" != "$(id -un)" ]]; then
        populate_shell="$(getent passwd "$populate_user" | cut -d: -f7)"
        [[ -x "$populate_shell" ]] || populate_shell="/bin/sh"
        printf -v populate_cmd 'cd %q && exec %q' "$repo_root" "$repo_root/scripts/populate-bundles.sh"
        for a in "${populate_args[@]}"; do printf -v populate_cmd '%s %q' "$populate_cmd" "$a"; done
        echo "[install-local-direct] populating bundles as $populate_user (build runs as the invoking user, not root)"
        sudo -H -u "$populate_user" "$populate_shell" -lc "$populate_cmd"
    else
        "$repo_root/scripts/populate-bundles.sh" "${populate_args[@]}"
    fi
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
for name in "${bundle_names[@]}"; do
    if [[ $run_populate -eq 0 ]]; then
        refresh_installed_bundle_payload "$name"
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
    # The node lives in the INVOKING USER's XDG data dir, not root's. Run init as that
    # user so ryeos's own app-root resolution (RYEOS_APP_ROOT > BaseDirs data dir) picks
    # the right node and writes user-owned state. Never init under sudo: $HOME would be
    # /root and XDG would be scrubbed — that is what silently sent the node to /root.
    init_user="${SUDO_USER:-$(id -un)}"
    init_user_home="$(getent passwd "$init_user" | cut -d: -f6)"
    init_as=()
    [[ "$init_user" != "$(id -un)" ]] && init_as=(sudo -H -u "$init_user")
    echo "[install-local-direct] running ryeos init as $init_user"
    state_root="${init_app_root:-$init_user_home/.local/share/ryeos}"
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
    "${init_as[@]}" ryeos "${init_args[@]}" "${trust_args[@]}"

    echo "[install-local-direct] verifying initialized bundle state"
    state_root="${init_app_root:-$init_user_home/.local/share/ryeos}"
    for name in "${bundle_names[@]}"; do
        test -d "$state_root/.ai/bundles/$name/.ai" || \
            die "initialized $name bundle missing from $state_root"
    done
    if [[ "$bundle_set" == "hosted-node" ]]; then
        for name in standard studio web browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized hosted-node state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized hosted-node state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "standard" ]]; then
        for name in hosted-node studio web browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized standard state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized standard state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "central-host" ]]; then
        # central-host is standard + web; it must NOT drag in the studio/browser
        # UI bundles or the hosted-node control plane.
        for name in hosted-node studio browser; do
            test ! -e "$state_root/.ai/bundles/$name" || \
                die "initialized central-host state unexpectedly contains $name bundle"
            test ! -e "$state_root/.ai/node/bundles/$name.yaml" || \
                die "initialized central-host state unexpectedly contains $name registration"
        done
    fi
    if [[ "$bundle_set" == "full" ]]; then
        grep -q '^  execute: client:ryeos/tui$' \
            "$state_root/.ai/bundles/studio/.ai/node/commands/tui.yaml" || \
            die "initialized tui command is stale or not client-backed"
    fi

    # ── Verify installed bundle signatures (offline doctor --strict) ──
    # Closes the "edited YAML, forgot to re-sign, discover at runtime" loop:
    # run the same preflight verification `ryeos doctor` wraps against every
    # installed bundle and fail the install on any red check. Offline, no daemon.
    core_tools_bin="$share_dir/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools"
    if [[ -x "$core_tools_bin" ]]; then
        echo "[install-local-direct] verifying installed bundle signatures (doctor --strict)"
        verify_failed=0
        for name in "${bundle_names[@]}"; do
            if [[ "$init_user" != "$(id -un)" ]]; then
                sudo -H -u "$init_user" env RYEOS_APP_ROOT="$state_root" \
                    "$core_tools_bin" doctor "$share_dir/$name" --strict >/dev/null \
                    || { echo "[install-local-direct] doctor FAILED for bundle: $name" >&2; verify_failed=1; }
            else
                RYEOS_APP_ROOT="$state_root" \
                    "$core_tools_bin" doctor "$share_dir/$name" --strict >/dev/null \
                    || { echo "[install-local-direct] doctor FAILED for bundle: $name" >&2; verify_failed=1; }
            fi
        done
        [[ $verify_failed -eq 0 ]] || \
            die "installed bundle verification failed — re-run with --populate to re-sign, or investigate the stale signature above"
    else
        echo "[install-local-direct] skipping bundle verification: core-tools binary not found at $core_tools_bin" >&2
    fi
fi

if [[ $daemon_was_running -eq 1 ]]; then
    echo "[install-local-direct] restarting daemon"
    # An incompatible projection schema epoch bump can make the first restart
    # rebuild projection.sqlite3 from CAS/refs before readiness. The reporter
    # announces the rebuild and its progress ONLY when one is detected, so the
    # wait neither reads as a hang nor cries wolf on an ordinary restart.
    # `ryeos start` output is kept (not sunk to /dev/null) so the CLI's own
    # readiness diagnostic surfaces too. The 930s timeout stays slightly above
    # ryeos start's internal wait.
    state_dir="${init_app_root:-$(getent passwd "$invoking_user" | cut -d: -f6)/.local/share/ryeos}/.ai/state"
    report_projection_rebuild "$state_dir" &
    rebuild_reporter_pid=$!
    if ! ryeos_user 930 start; then
        kill "$rebuild_reporter_pid" 2>/dev/null || true
        die "daemon did not restart cleanly"
    fi
    kill "$rebuild_reporter_pid" 2>/dev/null || true
    wait "$rebuild_reporter_pid" 2>/dev/null || true
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
