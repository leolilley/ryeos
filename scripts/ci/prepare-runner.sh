#!/usr/bin/env bash
# Prepare a hosted runner for jobs that build or run the workspace tests.
#
# Frees the preinstalled SDKs' disk, places TMPDIR on a loop-mounted ext4
# volume distinct from / (the node's durable-root probe refuses an app root on
# the root disk), and allows unprivileged user namespaces (the sandbox probes
# map one). The namespace sysctl selects the test-host baseline on the
# disposable VM; it is not an application fallback.
#
# Usage: prepare-runner.sh [--no-scratch]   (GITHUB_ENV must be set)
set -euo pipefail

scratch=1
[[ "${1:-}" != "--no-scratch" ]] || scratch=0

sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc /usr/local/.ghcup /opt/hostedtoolcache/CodeQL
df -h /

if [[ "$scratch" == 1 ]]; then
    sudo fallocate -l 6G /var/tmp/ryeos-volume.img
    sudo mkfs.ext4 -q -F /var/tmp/ryeos-volume.img
    sudo mkdir -p /mnt/ryeos-tmp
    sudo mount -o loop /var/tmp/ryeos-volume.img /mnt/ryeos-tmp
    sudo chown "$(id -u):$(id -g)" /mnt/ryeos-tmp
    findmnt --target /mnt/ryeos-tmp
    echo "TMPDIR=/mnt/ryeos-tmp" >> "$GITHUB_ENV"
fi

sudo -n sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
