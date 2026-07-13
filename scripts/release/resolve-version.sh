#!/usr/bin/env bash

# Resolve and validate the release version without evaluating workflow input as
# shell source. The validated version is written to stdout for the caller to
# place in its own trusted output channel.

set -euo pipefail

event_name="${1:-}"
ref_name="${2:-}"
requested_version="${3:-}"

case "$event_name" in
    push)
        if [[ "$ref_name" != v* ]]; then
            echo "release version: push ref must start with 'v'" >&2
            exit 2
        fi
        version="${ref_name#v}"
        ;;
    workflow_dispatch)
        version="$requested_version"
        ;;
    *)
        echo "release version: unsupported event '$event_name'" >&2
        exit 2
        ;;
esac

# Docker tags cannot contain SemVer build metadata (`+...`), so RyeOS release
# versions intentionally support core SemVer plus an optional prerelease only.
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+)(\.[0-9A-Za-z-]+)*)?$ ]]; then
    echo "release version: '$version' is not supported SemVer (expected X.Y.Z or X.Y.Z-prerelease)" >&2
    exit 2
fi

prerelease="${version#*-}"
if [[ "$prerelease" != "$version" ]]; then
    IFS='.' read -r -a identifiers <<< "$prerelease"
    for identifier in "${identifiers[@]}"; do
        if [[ "$identifier" =~ ^[0-9]+$ && "$identifier" != "0" && "$identifier" == 0* ]]; then
            echo "release version: numeric prerelease identifier '$identifier' has a leading zero" >&2
            exit 2
        fi
    done
fi

printf '%s\n' "$version"
