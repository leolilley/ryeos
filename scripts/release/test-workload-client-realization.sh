#!/usr/bin/env bash

# Focused artifact-contract test. It consumes an already-built exact workload
# client; it never compiles RyeOS or manufactures a substitute executable.

set -euo pipefail
export LC_ALL=C

usage() {
    echo "usage: $0 --version X.Y.Z --source-revision SHA --build-date RFC3339 --source-date-epoch EPOCH --binary FILE --license FILE" >&2
    exit 2
}

version=""
source_revision=""
build_date=""
source_date_epoch=""
binary=""
license=""
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --source-revision) source_revision="${2:-}"; shift 2 ;;
        --build-date) build_date="${2:-}"; shift 2 ;;
        --source-date-epoch) source_date_epoch="${2:-}"; shift 2 ;;
        --binary) binary="${2:-}"; shift 2 ;;
        --license) license="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$version" && -n "$source_revision" && -n "$build_date" \
    && -n "$source_date_epoch" && -n "$binary" && -n "$license" ]] || usage

root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
name="ryeos-workload-client-${version}-x86_64-unknown-linux-gnu.tar.gz"
mkdir "$tmp/one" "$tmp/two"

package() {
    "$root/scripts/release/package-workload-client-realization.sh" \
        --version "$version" \
        --source-revision "$source_revision" \
        --build-date "$build_date" \
        --source-date-epoch "$source_date_epoch" \
        --target x86_64-unknown-linux-gnu \
        --binary "$binary" \
        --license "$license" \
        --output "$1/$name"
}

package "$tmp/one"
package "$tmp/two"
cmp "$tmp/one/$name" "$tmp/two/$name"
cmp "$tmp/one/$name.sha256" "$tmp/two/$name.sha256"

"$root/scripts/release/verify-workload-client-realization.sh" \
    --version "$version" \
    --source-revision "$source_revision" \
    --build-date "$build_date" \
    --source-date-epoch "$source_date_epoch" \
    --archive "$tmp/one/$name" \
    --checksum "$tmp/one/$name.sha256" \
    --materialize "$tmp/materialized"
[[ -x "$tmp/materialized/bin/ryeos" ]]

if "$root/scripts/release/verify-workload-client-realization.sh" \
    --version "$version" \
    --source-revision "$source_revision" \
    --build-date "$build_date" \
    --source-date-epoch "$source_date_epoch" \
    --archive "$tmp/one/$name" \
    --checksum "$tmp/one/$name.sha256" \
    --materialize "$tmp/materialized" >/dev/null 2>&1; then
    echo "workload-client verifier replaced an existing materialization" >&2
    exit 1
fi

mkdir "$tmp/bad"
cp "$tmp/one/$name.sha256" "$tmp/bad/$name.sha256"
sed -i -E 's/^[0-9a-f]{64}/0000000000000000000000000000000000000000000000000000000000000000/' "$tmp/bad/$name.sha256"
if "$root/scripts/release/verify-workload-client-realization.sh" \
    --version "$version" \
    --source-revision "$source_revision" \
    --build-date "$build_date" \
    --source-date-epoch "$source_date_epoch" \
    --archive "$tmp/one/$name" \
    --checksum "$tmp/bad/$name.sha256" >/dev/null 2>&1; then
    echo "workload-client verifier accepted a contradictory checksum" >&2
    exit 1
fi

echo "workload-client realization contract test passed"
