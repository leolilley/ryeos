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
    scripts/release/resolve-version.sh \
    scripts/release/test-resolve-version.sh \
    scripts/pkg/bundle-sets.sh \
    scripts/pkg/install-local-direct.sh \
    scripts/populate-bundles.sh
