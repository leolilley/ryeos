#!/usr/bin/env bash

# Package a populated, officially signed bundle source set for release.
#
# The output is deterministic for a fixed populated source tree and
# SOURCE_DATE_EPOCH. Before archiving, this script checks publisher metadata and
# runs a production `ryeos init` preflight with no trust-file override. That
# final check is the security boundary: every seed and bundle signature must
# validate against the official publisher key compiled into the release binary.

set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$root/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

usage() {
    ryeos_term_fail "usage: $0 --version X.Y.Z --source DIR --output FILE --source-date-epoch EPOCH --ryeos-bin PATH [--arch x86_64]"
    exit 2
}

version=""
source_dir=""
output=""
source_date_epoch=""
ryeos_bin=""
arch="x86_64"
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --source) source_dir="${2:-}"; shift 2 ;;
        --output) output="${2:-}"; shift 2 ;;
        --source-date-epoch) source_date_epoch="${2:-}"; shift 2 ;;
        --ryeos-bin) ryeos_bin="${2:-}"; shift 2 ;;
        --arch) arch="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done

[[ -n "$version" && -n "$source_dir" && -n "$output" && -n "$source_date_epoch" && -n "$ryeos_bin" ]] || usage
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+)(\.[0-9A-Za-z-]+)*)?$ ]] || {
    ryeos_term_fail "version is not SemVer: $version"
    exit 2
}
[[ "$arch" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]*$ ]] || {
    ryeos_term_fail "invalid architecture label: $arch"
    exit 2
}
[[ "$source_date_epoch" =~ ^[0-9]+$ ]] || {
    ryeos_term_fail "source date epoch must be a non-negative integer"
    exit 2
}
[[ -d "$source_dir/.ai" ]] || {
    ryeos_term_fail "source root is missing .ai/: $source_dir"
    exit 2
}
[[ -x "$ryeos_bin" ]] || {
    ryeos_term_fail "ryeos release binary is not executable: $ryeos_bin"
    exit 2
}

expected_name="ryeos-bundles-${version}-${arch}.tar.gz"
[[ "$(basename "$output")" == "$expected_name" ]] || {
    ryeos_term_fail "output must be named $expected_name"
    exit 2
}
[[ ! -e "$output" && ! -e "$output.sha256" ]] || {
    ryeos_term_fail "refusing to overwrite existing output: $output"
    exit 2
}

# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
ryeos_term_begin PUBLISH "packaging official bundle artifact"

trust_value() {
    local file="$1"
    local field="$2"
    local values
    values="$(sed -n "s/^[[:space:]]*${field}[[:space:]]*=[[:space:]]*\"\([^\"]*\)\"[[:space:]]*$/\\1/p" "$file")"
    if [[ "$(printf '%s\n' "$values" | sed '/^$/d' | wc -l)" -ne 1 ]]; then
        ryeos_term_fail "expected one $field value in $file"
        return 1
    fi
    printf '%s\n' "$values"
}

assert_official_trust_metadata() {
    local trust_file="$1"
    local fingerprint owner
    [[ -f "$trust_file" ]] || {
        ryeos_term_fail "missing publisher metadata: $trust_file"
        return 1
    }
    fingerprint="$(trust_value "$trust_file" fingerprint)"
    owner="$(trust_value "$trust_file" owner)"
    [[ "${fingerprint,,}" == "$official_fp" ]] || {
        ryeos_term_fail "$trust_file names non-official publisher $fingerprint"
        return 1
    }
    [[ "$owner" == "ryeos-official" ]] || {
        ryeos_term_fail "$trust_file owner must be ryeos-official, got $owner"
        return 1
    }
}

assert_official_trust_metadata "$source_dir/.ai/PUBLISHER_TRUST.toml"

tmp="$(mktemp -d)"
archive_tmp="$output.tmp.$$"
checksum="$output.sha256"
checksum_tmp="$checksum.tmp.$$"
completed=0
cleanup() {
    local status="$1"
    ryeos_term_handle_exit "$status"
    rm -rf "$tmp"
    rm -f "$archive_tmp" "$checksum_tmp"
    if [[ "$completed" -ne 1 ]]; then
        rm -f "$output" "$checksum"
    fi
    return "$status"
}
trap 'cleanup "$?"' EXIT

archive_root="ryeos-bundles-${version}-${arch}"
stage="$tmp/$archive_root"
mkdir -p "$stage"
cp -a "$source_dir/.ai" "$stage/.ai"

while IFS= read -r bundle; do
    [[ -n "$bundle" ]] || continue
    bundle_source="$source_dir/$bundle"
    [[ -d "$bundle_source/.ai" ]] || {
        ryeos_term_fail "full bundle set is missing $bundle/.ai"
        exit 2
    }
    assert_official_trust_metadata "$bundle_source/PUBLISHER_TRUST.toml"
    cp -a "$bundle_source" "$stage/$bundle"
done < <(ryeos_bundle_set_names full)

if find "$stage" -type l -print -quit | grep -q .; then
    ryeos_term_fail "symbolic links are not allowed in release bundles"
    exit 2
fi
if find "$stage" \( -type b -o -type c -o -type p -o -type s \) -print -quit | grep -q .; then
    ryeos_term_fail "special files are not allowed in release bundles"
    exit 2
fi
hardlinked_file="$(find "$stage" -type f -links +1 -print -quit)"
[[ -z "$hardlinked_file" ]] || {
    ryeos_term_fail "multiply-linked files are not allowed: $hardlinked_file"
    exit 2
}
privileged_file="$(find "$stage" -type f -perm /6000 -print -quit)"
[[ -z "$privileged_file" ]] || {
    ryeos_term_fail "setuid/setgid files are not allowed: $privileged_file"
    exit 2
}
unsafe_name="$(find "$stage" -type f \( \
    -iname '*.pem' -o \
    -iname '*.key' -o \
    -iname '*.p12' -o \
    -iname '*.pfx' -o \
    -iname 'id_rsa' -o \
    -iname 'id_ed25519' -o \
    -iname '*private*key*' \
\) -print -quit)"
[[ -z "$unsafe_name" ]] || {
    ryeos_term_fail "refusing to archive possible private key file: $unsafe_name"
    exit 2
}
private_key_markers="$(
    grep -IRlE -- '-----BEGIN ([A-Z0-9]+ )?(ENCRYPTED )?PRIVATE KEY-----' "$stage" \
        || true
)"
[[ -z "$private_key_markers" ]] || {
    ryeos_term_fail "refusing to archive private key material: $private_key_markers"
    exit 2
}

while IFS= read -r -d '' staged_path; do
    if [[ "$staged_path" =~ [[:cntrl:]] ]]; then
        ryeos_term_fail "control characters are not allowed in archive paths"
        exit 2
    fi
done < <(find "$stage" -print0)

# Do not pass --trust-file here. This proves the staged source set validates
# solely under the official public key embedded in the qualified RyeOS binary.
verify_app_root="$tmp/verify-app"
ryeos_term_update "verifying staged source set" "production init preflight"
"$ryeos_bin" init --app-root "$verify_app_root" --source "$stage" >/dev/null

mkdir -p "$(dirname "$output")"
ryeos_term_update "writing deterministic archive" "$(basename "$output")"
tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$tmp" \
    -cf - "$archive_root" \
    | gzip -n -9 > "$archive_tmp"
mv "$archive_tmp" "$output"

archive_sha256="$(sha256sum "$output" | awk '{print $1}')"
printf '%s  %s\n' "$archive_sha256" "$(basename "$output")" > "$checksum_tmp"
mv "$checksum_tmp" "$checksum"
completed=1

ryeos_term_end success "PUBLISH COMPLETE" "$output"
