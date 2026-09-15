#!/usr/bin/env bash

# Verify one immutable workload-client external realization release asset.

set -euo pipefail
export LC_ALL=C
umask 022

maximum_archive_bytes=268435456
maximum_tree_bytes=536870912

usage() {
    echo "usage: $0 --version X.Y.Z --source-revision SHA --build-date RFC3339 --source-date-epoch EPOCH --archive FILE [--checksum FILE] [--materialize DIR]" >&2
    exit 2
}

version=""
source_revision=""
build_date=""
source_date_epoch=""
archive=""
checksum=""
materialize=""
materialize_lock=""
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --source-revision) source_revision="${2:-}"; shift 2 ;;
        --build-date) build_date="${2:-}"; shift 2 ;;
        --source-date-epoch) source_date_epoch="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --checksum) checksum="${2:-}"; shift 2 ;;
        --materialize) materialize="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$version" && -n "$source_revision" && -n "$build_date" \
    && -n "$source_date_epoch" && -n "$archive" ]] || usage
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+)(\.[0-9A-Za-z-]+)*)?$ ]]
[[ "$source_revision" =~ ^[0-9a-f]{40}$ ]]
[[ "$build_date" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]]
[[ "$source_date_epoch" =~ ^[0-9]+$ ]]
[[ -f "$archive" && ! -L "$archive" ]]
for command in awk basename cat cmp date dirname find mkdir mktemp mv objcopy readelf \
    rm rmdir sha256sum sort stat tar wc; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "workload-client realization verification requires $command" >&2
        exit 2
    }
done
[[ "$build_date" == "$(date --utc --date="@$source_date_epoch" '+%Y-%m-%dT%H:%M:%SZ')" ]]

archive_name="ryeos-workload-client-${version}-x86_64-unknown-linux-gnu.tar.gz"
[[ "$(basename "$archive")" == "$archive_name" ]] || {
    echo "unexpected workload-client realization archive name" >&2
    exit 2
}
(( $(stat -c '%s' "$archive") <= maximum_archive_bytes )) || {
    echo "workload-client realization archive exceeds its release bound" >&2
    exit 2
}
if [[ -n "$checksum" ]]; then
    [[ -f "$checksum" && ! -L "$checksum" \
        && "$(basename "$checksum")" == "$archive_name.sha256" ]]
    [[ "$(wc -l < "$checksum")" -eq 1 ]]
    checksum_line="$(cat "$checksum")"
    read -r expected_digest expected_name extra < "$checksum"
    actual_digest="$(sha256sum "$archive" | awk '{print $1}')"
    [[ -z "${extra:-}" \
        && "$expected_digest" =~ ^[0-9a-f]{64}$ \
        && "$expected_name" == "$archive_name" \
        && "$checksum_line" == "$expected_digest  $archive_name" \
        && "$actual_digest" == "$expected_digest" ]] || {
        echo "workload-client realization checksum is absent, malformed, or incorrect" >&2
        exit 2
    }
fi

if [[ -n "$materialize" ]]; then
    materialize_parent="$(dirname "$materialize")"
    [[ -d "$materialize_parent" && ! -L "$materialize_parent" ]] || {
        echo "workload-client materialization requires an existing real parent" >&2
        exit 2
    }
    materialize_lock="${materialize}.publish-lock"
    mkdir "$materialize_lock" 2>/dev/null || {
        echo "another workload-client materialization owns the destination" >&2
        exit 2
    }
    [[ ! -e "$materialize" ]] || {
        rmdir "$materialize_lock"
        echo "workload-client materialization destination already exists" >&2
        exit 2
    }
    tmp="$(mktemp -d "$materialize_parent/.ryeos-workload-client-verify.XXXXXX")" || {
        rmdir "$materialize_lock"
        exit 2
    }
else
    tmp="$(mktemp -d)"
fi
cleanup() {
    local status="$1"
    rm -rf "$tmp"
    if [[ -n "$materialize_lock" ]]; then
        rmdir "$materialize_lock"
    fi
    return "$status"
}
trap 'cleanup "$?"' EXIT
root_name="${archive_name%.tar.gz}"
actual_archive_inventory="$tmp/archive-inventory"
expected_archive_inventory="$tmp/expected-archive-inventory"
archive_listing="$tmp/archive-listing"
tar -tvzf "$archive" --numeric-owner > "$archive_listing"
archive_tree_bytes="$(awk '
    $3 !~ /^[0-9]+$/ { exit 17 }
    { total += $3 }
    END { print total + 0 }
' "$archive_listing")" || {
    echo "workload-client realization archive carries malformed member sizes" >&2
    exit 2
}
(( archive_tree_bytes <= maximum_tree_bytes )) || {
    echo "workload-client realization tree exceeds its release bound" >&2
    exit 2
}
awk '{ print $1 " " $NF }' "$archive_listing" > "$actual_archive_inventory"
cat > "$expected_archive_inventory" <<EOF
drwxr-xr-x ${root_name}/
-rw-r--r-- ${root_name}/LICENSE
-rw-r--r-- ${root_name}/RYEOS-BUILD
drwxr-xr-x ${root_name}/bin/
-rwxr-xr-x ${root_name}/bin/ryeos
EOF
cmp "$expected_archive_inventory" "$actual_archive_inventory" || {
    echo "workload-client realization archive entries, types, or modes are not closed" >&2
    exit 2
}
tar -xzf "$archive" --no-same-owner -C "$tmp"
[[ -d "$tmp/$root_name" && ! -L "$tmp/$root_name" ]]
actual_inventory="$tmp/inventory"
expected_inventory="$tmp/expected-inventory"
find "$tmp/$root_name" -mindepth 1 -printf '%y %P\n' | sort > "$actual_inventory"
cat > "$expected_inventory" <<'EOF'
d bin
f LICENSE
f RYEOS-BUILD
f bin/ryeos
EOF
cmp "$expected_inventory" "$actual_inventory" || {
    echo "workload-client realization inventory is not closed" >&2
    exit 2
}
[[ -x "$tmp/$root_name/bin/ryeos" ]]
elf_header="$(readelf -h "$tmp/$root_name/bin/ryeos")" || {
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
if readelf -l "$tmp/$root_name/bin/ryeos" | grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' \
    || readelf -d "$tmp/$root_name/bin/ryeos" | grep -Eq 'NEEDED'; then
    echo "workload-client realization binary is not fully static" >&2
    exit 2
fi
cat > "$tmp/expected-build" <<EOF
schema=ryeos.workload-client-build.v1
qualification=exact
version=$version
source_revision=$source_revision
build_date=$build_date
source_date_epoch=$source_date_epoch
target=x86_64-unknown-linux-gnu
profile=release
EOF
cmp "$tmp/expected-build" "$tmp/$root_name/RYEOS-BUILD" || {
    echo "workload-client realization sidecar names the wrong build coordinate" >&2
    exit 2
}
objcopy \
    --dump-section .ryeos_workload_client_build="$tmp/embedded-build" \
    "$tmp/$root_name/bin/ryeos"
cmp "$tmp/expected-build" "$tmp/embedded-build" || {
    echo "workload-client realization binary names the wrong build coordinate" >&2
    exit 2
}

if [[ -n "$materialize" ]]; then
    mv "$tmp/$root_name" "$materialize"
fi

echo "verified exact workload-client realization: $archive"
if [[ -n "$materialize" ]]; then
    echo "materialized verified tree: $materialize"
fi
