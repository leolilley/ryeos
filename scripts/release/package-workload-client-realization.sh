#!/usr/bin/env bash

# Package the restricted workload-local RyeOS client as one immutable external
# realization. This is a release-construction step, not a package manager: the
# resulting tree is consumed by RyeOS's ordinary external-content import and
# target-local binding authorities.

set -euo pipefail
export LC_ALL=C
umask 022

maximum_archive_bytes=268435456
maximum_tree_bytes=536870912

usage() {
    echo "usage: $0 --version X.Y.Z --source-revision SHA --build-date RFC3339 --source-date-epoch EPOCH --target TRIPLE --binary FILE --license FILE --output FILE" >&2
    exit 2
}

version=""
source_revision=""
build_date=""
source_date_epoch=""
target=""
binary=""
license=""
output=""
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --source-revision) source_revision="${2:-}"; shift 2 ;;
        --build-date) build_date="${2:-}"; shift 2 ;;
        --source-date-epoch) source_date_epoch="${2:-}"; shift 2 ;;
        --target) target="${2:-}"; shift 2 ;;
        --binary) binary="${2:-}"; shift 2 ;;
        --license) license="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

[[ -n "$version" && -n "$source_revision" && -n "$build_date" \
    && -n "$source_date_epoch" && -n "$target" && -n "$binary" \
    && -n "$license" && -n "$output" ]] || usage
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+)(\.[0-9A-Za-z-]+)*)?$ ]] || {
    echo "workload-client realization version is not supported SemVer: $version" >&2
    exit 2
}
[[ "$source_revision" =~ ^[0-9a-f]{40}$ ]] || {
    echo "workload-client realization source revision is not a full Git SHA-1" >&2
    exit 2
}
[[ "$build_date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || {
    echo "workload-client realization build date is not canonical UTC RFC3339" >&2
    exit 2
}
[[ "$source_date_epoch" =~ ^[0-9]+$ ]] || {
    echo "workload-client realization source-date epoch is not canonical" >&2
    exit 2
}
[[ "$target" == x86_64-unknown-linux-gnu ]] || {
    echo "unsupported first workload-client realization target: $target" >&2
    exit 2
}
[[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] || {
    echo "workload-client realization binary is missing, linked, or not executable: $binary" >&2
    exit 2
}
[[ -f "$license" && ! -L "$license" ]] || {
    echo "workload-client realization license is missing or linked: $license" >&2
    exit 2
}
for command in awk basename cat chmod cmp date dirname grep gzip install mkdir mktemp mv \
    objcopy readelf rm rmdir sha256sum stat tar; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "workload-client realization packaging requires $command" >&2
        exit 2
    }
done
[[ "$build_date" == "$(date --utc --date="@$source_date_epoch" '+%Y-%m-%dT%H:%M:%SZ')" ]] || {
    echo "workload-client build date and source-date epoch identify different instants" >&2
    exit 2
}
elf_header="$(readelf -h "$binary")" || {
    echo "workload-client realization is not an ELF executable" >&2
    exit 2
}
grep -Eq '^[[:space:]]*Class:[[:space:]]+ELF64$' <<<"$elf_header" \
    && grep -Eq '^[[:space:]]*Data:[[:space:]]+2.s complement, little endian$' <<<"$elf_header" \
    && grep -Eq '^[[:space:]]*Machine:[[:space:]]+Advanced Micro Devices X86-64$' <<<"$elf_header" \
    && grep -Eq '^[[:space:]]*Type:[[:space:]]+(EXEC|DYN)[[:space:]]' <<<"$elf_header" || {
    echo "workload-client realization does not match x86_64-unknown-linux-gnu" >&2
    exit 2
}
if readelf -l "$binary" | grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' \
    || readelf -d "$binary" | grep -Eq 'NEEDED'; then
    echo "workload-client realization must be fully static: $binary" >&2
    exit 2
fi

archive_name="ryeos-workload-client-${version}-x86_64-unknown-linux-gnu.tar.gz"
[[ "$(basename "$output")" == "$archive_name" ]] || {
    echo "workload-client realization output must be named $archive_name" >&2
    exit 2
}
mkdir -p "$(dirname "$output")"
output_lock="${output}.publish-lock"
mkdir "$output_lock" 2>/dev/null || {
    echo "another workload-client publisher owns the output" >&2
    exit 2
}
[[ ! -e "$output" && ! -e "$output.sha256" ]] || {
    rmdir "$output_lock"
    echo "refusing to overwrite workload-client realization output: $output" >&2
    exit 2
}

tmp="$(mktemp -d)" || {
    rmdir "$output_lock"
    exit 2
}
archive_tmp="$output.tmp.$$"
checksum_tmp="$output.sha256.tmp.$$"
completed=0
cleanup() {
    local status="$1"
    rm -rf "$tmp"
    rm -f "$archive_tmp" "$checksum_tmp"
    if [[ "$completed" -ne 1 ]]; then
        rm -f "$output" "$output.sha256"
    fi
    rmdir "$output_lock"
    return "$status"
}
trap 'cleanup "$?"' EXIT

root_name="${archive_name%.tar.gz}"
stage="$tmp/$root_name"
mkdir -p "$stage/bin"
chmod 0755 "$stage" "$stage/bin"
install -m 0755 "$binary" "$stage/bin/ryeos"
install -m 0644 "$license" "$stage/LICENSE"
cat > "$tmp/expected-build" <<EOF
schema=ryeos.workload-client-build.v1
qualification=exact
version=$version
source_revision=$source_revision
build_date=$build_date
source_date_epoch=$source_date_epoch
target=$target
profile=release
EOF
objcopy \
    --dump-section .ryeos_workload_client_build="$stage/RYEOS-BUILD" \
    "$stage/bin/ryeos"
cmp "$tmp/expected-build" "$stage/RYEOS-BUILD" || {
    echo "workload-client binary does not embed the requested exact build coordinate" >&2
    exit 2
}
chmod 0644 "$stage/RYEOS-BUILD"

tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$tmp" \
    -cf - "$root_name" \
    | gzip -n -9 > "$archive_tmp"
(( $(stat -c '%s' "$archive_tmp") <= maximum_archive_bytes )) || {
    echo "workload-client realization archive exceeds its release bound" >&2
    exit 2
}
tree_bytes="$(stat -c '%s' "$stage/bin/ryeos" "$stage/LICENSE" "$stage/RYEOS-BUILD" \
    | awk '{ total += $1 } END { print total + 0 }')"
(( tree_bytes <= maximum_tree_bytes )) || {
    echo "workload-client realization tree exceeds its release bound" >&2
    exit 2
}
mv "$archive_tmp" "$output"
(
    cd "$(dirname "$output")"
    sha256sum "$(basename "$output")"
) > "$checksum_tmp"
mv "$checksum_tmp" "$output.sha256"
completed=1

echo "packaged exact workload-client realization: $output"
