#!/usr/bin/env bash

set -euo pipefail

tool_dir="${1:?usage: lint-workflows-and-shell.sh TOOL_DIRECTORY}"
actionlint="$tool_dir/actionlint"
shellcheck="$tool_dir/shellcheck"

test -x "$actionlint"
test -x "$shellcheck"

# Put the checksum-pinned ShellCheck on PATH so actionlint also checks every
# embedded `run:` block with the same version used for standalone scripts.
PATH="$tool_dir:$PATH" "$actionlint" -color

"$shellcheck" --severity=warning \
    scripts/ci/install-static-linters.sh \
    scripts/ci/lint-workflows-and-shell.sh \
    scripts/ci/test-daemon-image-init-policy.sh \
    scripts/ci/test-publisher-trust-policy.sh \
    scripts/release/resolve-version.sh \
    scripts/release/test-resolve-version.sh \
    scripts/release/official-publisher-fingerprint.sh \
    scripts/release/package-bundle-artifact.sh \
    scripts/release/test-package-bundle-artifact.sh \
    scripts/release/verify-bundle-artifact.sh \
    scripts/release/prepare-aur.sh \
    scripts/release/test-prepare-aur.sh \
    tests/e2e/container-image/qualify.sh \
    scripts/pkg/bundle-sets.sh \
    scripts/pkg/install-local-direct.sh \
    scripts/pkg/test-ryeos-terminal.sh \
    scripts/lib/ryeos-terminal.sh \
    scripts/gate.sh \
    tests/e2e/configured-remote/qualify.sh \
    tests/e2e/configured-remote/test_qualification.sh \
    scripts/dev-tui.sh \
    scripts/dev-ui-assets.sh \
    tests/e2e/execute-stream/smoke.sh \
    tests/e2e/installed-resume/smoke.sh \
    scripts/populate-bundles.sh \
    deploy/entrypoint.sh
