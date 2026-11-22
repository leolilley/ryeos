#!/usr/bin/env bash
# Focused external-installer tests. No build, sudo, daemon or service mutation.
set -euo pipefail

test_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$test_root/scripts/pkg/bundle-sets.sh"
source "$test_root/scripts/pkg/install-local-direct.sh"

test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT
test_log="$test_dir/actions"
test_release="$test_dir/release"
mkdir "$test_release"
cp /bin/true "$test_release/ryeos"
cp /bin/true "$test_release/ryeosd"
cp /bin/true "$test_release/ryeos-bundle-publisher"

# Stub only this external bootstrap boundary. Missing artifacts must be refused
# before any authorization request; refused authorization must precede stop.
id() { printf '%s\n' 1000; }
sudo() { printf 'authorize\n' >> "$test_log"; return "$test_sudo_status"; }
stop_daemon_for_install() { printf 'stop\n' >> "$test_log"; }
ryeos_term_suspend() { :; }
ryeos_term_fail() { :; }

attempt_install() {
    preflight_host_install "$test_release" "$@" || return 1
    stop_daemon_for_install
}

attempt_bundle_set_install() {
    preflight_bundle_set_host_install "$test_release" "$test_bundle_set" ryeos ryeosd || return 1
    stop_daemon_for_install
}

test_sudo_status=0
if attempt_install ryeos absent; then
    echo 'missing required binary was accepted' >&2
    exit 1
fi
[[ ! -e "$test_log" ]]

test_sudo_status=1
if attempt_install ryeos ryeosd; then
    echo 'missing authorization was accepted' >&2
    exit 1
fi
[[ "$(<"$test_log")" == authorize ]]

: > "$test_log"
test_sudo_status=0
attempt_install ryeos ryeosd
[[ "$(<"$test_log")" == $'authorize\nstop' ]]

: > "$test_log"
attempt_install ryeos ryeosd ryeos-bundle-publisher
[[ "$(<"$test_log")" == $'authorize\nstop' ]]

# Release-authority owns the measurement copy and refuses its absence before
# authorization or lifecycle shutdown. Ordinary sets do not require it.
: > "$test_log"
rm "$test_release/ryeos-bundle-publisher"
test_bundle_set=release-authority
if attempt_bundle_set_install; then
    echo 'release-authority accepted a missing publisher measurement executable' >&2
    exit 1
fi
[[ ! -s "$test_log" ]]
cp /bin/true "$test_release/ryeos-bundle-publisher"
attempt_bundle_set_install
[[ "$(<"$test_log")" == $'authorize\nstop' ]]

: > "$test_log"
rm "$test_release/ryeos-bundle-publisher"
test_bundle_set=standard
attempt_bundle_set_install
[[ "$(<"$test_log")" == $'authorize\nstop' ]]

# Root already holds installation authority; do not require another prompt.
: > "$test_log"
id() { printf '%s\n' 0; }
attempt_install ryeos ryeosd
[[ "$(<"$test_log")" == stop ]]

# Guard the actual entrypoint ordering, not merely the mocked composition.
preflight_line="$(sed -n '/^preflight_bundle_set_host_install "\$target_dir"/=' "$test_root/scripts/pkg/install-local-direct.sh")"
stop_line="$(sed -n '/^    if stop_daemon_for_install; then/=' "$test_root/scripts/pkg/install-local-direct.sh")"
[[ -n "$preflight_line" && -n "$stop_line" && "$preflight_line" -lt "$stop_line" ]]

# The selected node must survive the installer -> sudo -> login shell boundary.
# Capture argv only; do not execute sudo or a child shell in these cases.
(
    invoking_user=selected_operator
    init_app_root="$test_dir/a selected node"
    id() { printf '%s\n' installer; }
    getent() { printf 'selected_operator:x:1000:1000::/unused:/bin/bash\n'; }
    run_timeout() { printf '%s\n' "$@" > "$test_log"; }
    ryeos_user 10 node status
    printf -v expected_command 'exec env %q ryeos node status' "RYEOS_APP_ROOT=$init_app_root"
    [[ "$(tail -n 1 "$test_log")" == "$expected_command" ]]
)
(
    invoking_user=selected_operator
    init_app_root="$test_dir/a selected node"
    id() { printf '%s\n' selected_operator; }
    run_timeout() { printf '%s\n' "$RYEOS_APP_ROOT" "$@" > "$test_log"; }
    ryeos_user 10 node status
    [[ "$(head -n 1 "$test_log")" == "$init_app_root" ]]
)

# Restore the real helper after the preflight composition stubs. Each stop
# case runs in a subshell because production die exits the installer.
source "$test_root/scripts/pkg/install-local-direct.sh"
ryeos_term_suspend() { :; }
ryeos_term_info() { :; }
ryeos_term_fail() { :; }
ryeos_user() {
    if [[ "$2" == stop ]]; then
        printf 'stop\n' >> "$test_log"
        return "$test_stop_result"
    fi
    return 98
}
ryeos_status_quick() {
    [[ "$test_probe_result" == 0 ]] || return "$test_probe_result"
    if [[ -s "$test_log" ]]; then
        [[ "$test_final_probe_result" == 0 ]] || return "$test_final_probe_result"
        printf '%s\n' "$test_after_status"
    else
        printf '%s\n' "$test_before_status"
    fi
}
kill() { printf 'raw-kill\n' >> "$test_log"; exit 99; }

test_probe_result=0
test_final_probe_result=0
test_stop_result=0
test_before_status=running
test_after_status='initialized, stopped — run: ryeos start'

# Exercise the actual installer caller. `return 1` means initially stopped;
# uncertainty must exit, not return and allow the enclosing if to continue.
replacement_boundary() (
    daemon_was_running=0
    if stop_daemon_for_install; then
        daemon_was_running=1
    fi
    printf 'replace restart=%s\n' "$daemon_was_running" >> "$test_log"
)
expect_refused_replacement() {
    : > "$test_log"
    if replacement_boundary; then
        echo 'unsafe lifecycle outcome allowed replacement' >&2
        exit 1
    fi
    [[ "$(<"$test_log")" == "${1:-}" ]]
}

test_before_status=running
test_after_status='initialized, stopped — run: ryeos start'
: > "$test_log"
replacement_boundary
[[ "$(<"$test_log")" == $'stop\nreplace restart=1' ]]

for test_before_status in 'initialized, stopped — run: ryeos start' 'not initialized — run: ryeos init'; do
    : > "$test_log"
    replacement_boundary
    [[ "$(<"$test_log")" == 'replace restart=0' ]]
done
for test_before_status in '' 'stale daemon metadata' 'unrecognized state'; do
    expect_refused_replacement
done
test_before_status=running
test_probe_result=1
expect_refused_replacement
test_probe_result=0
test_final_probe_result=1
expect_refused_replacement stop
test_final_probe_result=0
test_stop_result=1
expect_refused_replacement stop
test_stop_result=0
test_after_status=running
expect_refused_replacement stop
printf 'installer lifecycle preflight and shutdown refusal: passed\n'

# Supervised upgrades use the shared host journal, never current PID liveness
# as the source of original intent. No sudo, node or host mutation in this test.
(
    target_dir="$test_release"
    state_root="$test_dir/node"
    restart_daemon=1
    run_init=1
    ryeos() { :; }
    test_host_mode=supervised
    test_host_intent=up
    test_host_begin_status=0
    host_upgrade_command() {
        if [[ "$1" == --inspect ]]; then
            printf '%s\n' "$test_host_mode"
        else
            printf '%s\n' "${*: -1}" >> "$test_log"
            printf '%s\n' "$test_host_intent"
            return "$test_host_begin_status"
        fi
    }
    : > "$test_log"
    prepare_host_upgrade
    [[ "$host_upgrade_desired" == up && "$(<"$test_log")" == begin ]]
    [[ "$host_upgrade_digest" =~ ^[a-f0-9]{64}$ ]]
    : > "$test_log"
    test_host_intent=down
    prepare_host_upgrade
    [[ "$host_upgrade_desired" == down && "$(<"$test_log")" == begin ]]

    for skip in run_init restart_daemon; do
        : > "$test_log"
        if ( declare "$skip=0"; prepare_host_upgrade ); then
            echo "supervised install accepted disabled $skip" >&2
            exit 1
        fi
        [[ ! -s "$test_log" ]]
    done
    : > "$test_log"
    test_host_mode=direct
    run_init=0
    restart_daemon=0
    prepare_host_upgrade
    [[ ! -s "$test_log" ]]
    test_host_mode=unknown
    if ( prepare_host_upgrade ); then
        echo 'unknown host association result was accepted' >&2
        exit 1
    fi
    test_host_mode=supervised
    run_init=1
    restart_daemon=1
    test_host_begin_status=1
    if ( prepare_host_upgrade ); then
        echo 'failed journal publication was accepted' >&2
        exit 1
    fi
)
printf 'installer supervised upgrade boundary: passed\n'

# The pre-shutdown policy probe must check every selected bundle against the
# current signed command authority. An explicit schema cut verifies the old
# occupant without interpreting its retired bundle map.
(
    target_dir="$test_dir/policy-release"
    state_root="$test_dir/policy-node"
    invoking_user="$(/usr/bin/id -un)"
    id() { /usr/bin/id "$@"; }
    mkdir -p "$target_dir" "$state_root/.ai/node/policies"
    cat > "$target_dir/ryeosd" <<EOF
#!/bin/sh
printf '%s\\n' "\$@" > "$test_dir/policy-argv"
EOF
    chmod +x "$target_dir/ryeosd"
    policy_generation_path="$state_root/.ai/node/policies"
    preflight_existing_node_policy "$policy_generation_path" 0 core render-sandbox
    [[ "$(<"$test_dir/policy-argv")" == $'init-policy-preflight\n--app-root\n'"$state_root"$'\n--active-bundle\ncore\n--active-bundle\nrender-sandbox' ]]
    preflight_existing_node_policy "$policy_generation_path" 1 core render-sandbox
    [[ "$(<"$test_dir/policy-argv")" == $'init-policy-preflight\n--app-root\n'"$state_root"$'\n--schema-cut' ]]
)
printf 'installer selected-bundle policy preflight: passed\n'

# --prefix installs a development node beside a real install. Layout,
# refusals and lifecycle argv only: nothing is installed and no sudo runs.
# Restore the real lifecycle helpers stubbed by the shutdown cases above.
source "$test_root/scripts/pkg/install-local-direct.sh"
ryeos_term_suspend() { :; }
ryeos_term_info() { :; }
test_default_node=/home/operator/.local/share/ryeos
test_fail_log="$test_dir/prefix-failures"
ryeos_term_fail() { printf '%s\n' "$*" >> "$test_fail_log"; }

# default_prefix_is_unchanged: /usr is the packaged layout found via PATH.
(
    resolve_install_layout /usr "" "$test_default_node"
    [[ "$bin_dir" == /usr/bin && "$share_dir" == /usr/share/ryeos \
        && "$doc_dir" == /usr/share/doc/ryeos && "$ryeos_cmd" == ryeos \
        && "$install_on_path" == 1 ]]
)

# prefix_sets_bin_share_and_doc_dirs
(
    resolve_install_layout /opt/ryeos-dev/ /srv/dev-node "$test_default_node"
    [[ "$install_prefix" == /opt/ryeos-dev && "$bin_dir" == /opt/ryeos-dev/bin \
        && "$share_dir" == /opt/ryeos-dev/share/ryeos \
        && "$doc_dir" == /opt/ryeos-dev/share/doc/ryeos \
        && "$ryeos_cmd" == /opt/ryeos-dev/bin/ryeos && "$install_on_path" == 0 ]]
)

expect_prefix_refused() {
    local expected_message="$1"
    shift
    : > "$test_fail_log"
    if ( resolve_install_layout "$@" ); then
        echo "prefix layout accepted: $*" >&2
        exit 1
    fi
    grep -qF -- "$expected_message" "$test_fail_log" || {
        echo "prefix refusal for $* did not name: $expected_message" >&2
        cat "$test_fail_log" >&2
        exit 1
    }
}

# prefix_requires_app_root, and never the default node.
expect_prefix_refused --app-root /opt/ryeos-dev "" "$test_default_node"
expect_prefix_refused 'refuses the default node' \
    /opt/ryeos-dev "$test_default_node/" "$test_default_node"
expect_prefix_refused 'absolute path' opt/ryeos-dev /srv/dev-node "$test_default_node"
expect_prefix_refused 'shadows /usr/bin' /usr/local /srv/dev-node "$test_default_node"
expect_prefix_refused 'shadows /usr/bin' /usr/local/ /srv/dev-node "$test_default_node"
expect_prefix_refused 'dedicated prefix' / /srv/dev-node "$test_default_node"
for test_prefix in /opt/../usr /opt/./ryeos-dev /opt//ryeos-dev /opt/ryeos-dev/..; do
    expect_prefix_refused 'normalized absolute path' "$test_prefix" /srv/dev-node "$test_default_node"
done

# prefix_requires_root_owned_hierarchy: the same boundary Lillux enforces for
# the package namespace. A group/world-writable directory is refused for any
# uid, so this holds whether or not the test runs as root.
require_root_owned_hierarchy /usr 0
require_root_owned_hierarchy /usr/ryeos-test-absent-prefix/share 1
if require_root_owned_hierarchy /usr/ryeos-test-absent-prefix/share 0; then
    echo 'absent prefix accepted where it must exist' >&2
    exit 1
fi
mkdir "$test_dir/open-prefix"
chmod 0777 "$test_dir/open-prefix"
if require_root_owned_hierarchy "$test_dir/open-prefix" 1; then
    echo 'world-writable prefix accepted' >&2
    exit 1
fi
ln -s /usr "$test_dir/linked-prefix"
if require_root_owned_hierarchy "$test_dir/linked-prefix" 1; then
    echo 'symlinked prefix accepted' >&2
    exit 1
fi

# prefix_uses_absolute_ryeos: lifecycle calls run the prefix's own client,
# across the sudo/login boundary and without it.
(
    resolve_install_layout /opt/ryeos-dev /srv/dev-node "$test_default_node"
    init_app_root=/srv/dev-node
    invoking_user=selected_operator
    id() { printf '%s\n' installer; }
    getent() { printf 'selected_operator:x:1000:1000::/unused:/bin/bash\n'; }
    run_timeout() { printf '%s\n' "$@" > "$test_log"; }
    ryeos_user 30 stop --force
    printf -v expected_command 'exec env %q %q stop --force' \
        "RYEOS_APP_ROOT=/srv/dev-node" /opt/ryeos-dev/bin/ryeos
    [[ "$(tail -n 1 "$test_log")" == "$expected_command" ]]

    id() { printf '%s\n' selected_operator; }
    ryeos_user 10 node status
    [[ "$(sed -n 2p "$test_log")" == /opt/ryeos-dev/bin/ryeos ]]
)

# A supervised prefix upgrade needs the prefix's own installed client, not
# whatever `ryeos` PATH resolves.
(
    target_dir="$test_release"
    restart_daemon=1
    run_init=1
    host_upgrade_command() {
        if [[ "$1" == --inspect ]]; then printf 'supervised\n'; else printf 'up\n'; fi
    }
    ryeos_cmd="$test_dir/absent-prefix/bin/ryeos"
    if ( prepare_host_upgrade ) 2>/dev/null; then
        echo 'supervised prefix upgrade accepted a missing prefix client' >&2
        exit 1
    fi
    ryeos_cmd="$test_release/ryeos"
    prepare_host_upgrade
    [[ "$host_upgrade_desired" == up ]]
)

# prefix_skips_path_shadow_move and the entrypoint wiring: the main flow is
# not sourceable, so guard the exact lines that carry the prefix.
installer="$test_root/scripts/pkg/install-local-direct.sh"
grep -qx 'if \[\[ \$cleanup_shadows -eq 1 && \$install_on_path -eq 1 \]\]; then' "$installer"
grep -qx '    "${init_as\[@\]}" "\$ryeos_cmd" "${init_args\[@\]}" "${trust_args\[@\]}" || init_status=\$?' "$installer"
grep -qx 'elif \[\[ \$restart_daemon -eq 1 \]\] && command -v "\$ryeos_cmd" >/dev/null 2>&1; then' "$installer"
if grep -nE '^[^#]*(bin_dir|share_dir|doc_dir)="/usr' "$installer"; then
    echo 'installer still hard-codes an install directory' >&2
    exit 1
fi
layout_line="$(sed -n '/^resolve_install_layout "\$install_prefix_argument"/=' "$installer")"
target_line="$(sed -n '/^target_dir="\$cargo_target_root\/release"/=' "$installer")"
[[ -n "$layout_line" && -n "$target_line" && "$layout_line" -lt "$target_line" ]]
printf 'installer prefix layout: passed\n'
