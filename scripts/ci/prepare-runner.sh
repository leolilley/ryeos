#!/usr/bin/env bash
# Prepare a hosted runner for jobs that build or run the workspace tests.
#
# Frees the preinstalled SDKs' disk, places TMPDIR on a loop-mounted ext4
# volume distinct from / (the node's durable-root probe refuses an app root on
# the root disk), and allows unprivileged user namespaces (the sandbox probes
# map one). The namespace sysctl selects the test-host baseline on the
# disposable VM; it is not an application fallback.
#
# With --host-service it also installs runit and makes sure a runsvdir
# supervises /etc/runit/runsvdir/default: the native service manager that the
# product's `ryeos node host setup` provisions a host-backed node on.
#
# Usage: prepare-runner.sh [--no-scratch] [--host-service]   (GITHUB_ENV must be set)
set -euo pipefail

scratch=1
host_service=0
for arg in "$@"; do
    case "$arg" in
        --no-scratch) scratch=0 ;;
        --host-service) host_service=1 ;;
        *) echo "unknown argument: $arg" >&2; exit 2 ;;
    esac
done

sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL
df -h /

if [[ "$scratch" == 1 ]]; then
    sudo fallocate -l 12G /var/tmp/ryeos-volume.img
    sudo mkfs.ext4 -q -F /var/tmp/ryeos-volume.img
    sudo mkdir -p /mnt/ryeos-tmp
    sudo mount -o loop /var/tmp/ryeos-volume.img /mnt/ryeos-tmp
    sudo chown "$(id -u):$(id -g)" /mnt/ryeos-tmp
    findmnt --target /mnt/ryeos-tmp
    echo "TMPDIR=/mnt/ryeos-tmp" >> "$GITHUB_ENV"
fi

sudo -n sysctl -w kernel.apparmor_restrict_unprivileged_userns=0

if [[ "$host_service" == 1 ]]; then
    ls -l /usr/bin/sudo /usr/bin/install; . /etc/os-release; echo "$PRETTY_NAME"
    "$(cd "$(dirname "$0")/../.." && pwd)/.ai/tools/ryeos/development/runner-host/lib/apt-update.sh"
    sudo apt-get install -y --no-install-recommends runit
    dpkg -L runit | grep -E '/(bin|sbin)/|runsvdir|\.service$' || true
    for tool in sv chpst svlogd runsvdir runsv; do
        command -v "$tool" || { echo "runit tool missing: $tool" >&2; exit 1; }
    done
    sudo mkdir -p /etc/runit/sv /etc/runit/runsvdir/default
    if ! pgrep -af runsvdir | grep -E -- '/etc/(runit/runsvdir/default|service)'; then
        sudo systemd-run --unit=ryeos-runsvdir --collect \
            /usr/bin/runsvdir -P /etc/runit/runsvdir/default
    fi
    ls -ld /etc/runit /etc/runit/sv /etc/runit/runsvdir/default /usr/bin/sv /usr/bin/chpst /usr/bin/svlogd /usr/bin/install
    pgrep -af runsvdir
fi
