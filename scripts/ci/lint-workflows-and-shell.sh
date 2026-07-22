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
    scripts/lint-cli-presentation.sh \
    scripts/release/resolve-version.sh \
    scripts/release/test-resolve-version.sh \
    scripts/release/official-publisher-fingerprint.sh \
    scripts/release/package-bundle-artifact.sh \
    scripts/release/test-package-bundle-artifact.sh \
    scripts/release/verify-bundle-artifact.sh \
    scripts/release/prepare-aur.sh \
    scripts/release/test-prepare-aur.sh \
    scripts/release/qualify-container-image.sh \
    scripts/pkg/bundle-sets.sh \
    scripts/pkg/install-local-direct.sh \
    scripts/pkg/test-ryeos-terminal.sh \
    scripts/lib/ryeos-terminal.sh \
    scripts/gate.sh \
    scripts/dev-tui.sh \
    scripts/dev-ui-assets.sh \
    scripts/smoke-execute-stream.sh \
    scripts/smoke-installed-resume.sh \
    scripts/populate-bundles.sh \
    deploy/entrypoint.sh
