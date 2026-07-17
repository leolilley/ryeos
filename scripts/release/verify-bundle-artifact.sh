#!/usr/bin/env bash

# Validate a published bundle archive without regenerating it. This is used by
# release retries: signed envelopes contain authoring timestamps, so an existing
# immutable archive is canonical and must be verified in place, not compared to
# newly re-signed bytes.

set -euo pipefail
export LC_ALL=C
root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$root/scripts/lib/ryeos-terminal.sh"
ryeos_term_init

usage() {
    ryeos_term_fail "usage: $0 --version X.Y.Z --archive PATH [--checksum PATH]"
    exit 2
}

version=""
archive=""
checksum=""
while (($#)); do
    case "$1" in
        --version) version="${2:-}"; shift 2 ;;
        --archive) archive="${2:-}"; shift 2 ;;
        --checksum) checksum="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n "$version" && -n "$archive" ]] || usage
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+)(\.[0-9A-Za-z-]+)*)?$ ]] || {
    ryeos_term_fail "unsupported version: $version"
    exit 2
}
[[ -f "$archive" ]] || { ryeos_term_fail "archive missing: $archive"; exit 2; }

archive_name="ryeos-bundles-${version}-x86_64.tar.gz"
archive_root="${archive_name%.tar.gz}"
ryeos_term_begin VERIFY "bundle artifact"
[[ "$(basename "$archive")" == "$archive_name" ]] || {
    ryeos_term_fail "expected archive name $archive_name"
    exit 2
}

if [[ -n "$checksum" ]]; then
    [[ -f "$checksum" && "$(basename "$checksum")" == "${archive_name}.sha256" ]] || {
        ryeos_term_fail "invalid checksum file: $checksum"
        exit 2
    }
    [[ "$(wc -l < "$checksum")" -eq 1 ]] || {
        ryeos_term_fail "checksum file must contain exactly one entry"
        exit 2
    }
    checksum_line="$(cat "$checksum")"
    read -r expected_digest expected_name extra < "$checksum"
    [[ -z "${extra:-}" && "$expected_digest" =~ ^[0-9a-f]{64}$ && "$expected_name" == "$archive_name" && "$checksum_line" == "$expected_digest  $archive_name" ]] || {
        ryeos_term_fail "malformed checksum file"
        exit 2
    }
    actual_digest="$(sha256sum "$archive" | awk '{print $1}')"
    [[ "$actual_digest" == "$expected_digest" ]] || {
        ryeos_term_fail "archive checksum mismatch"
        exit 2
    }
fi

entries="$(mktemp)"
listing="$(mktemp)"
extracted="$(mktemp -d)"
cleanup_verify() {
    local status="$1"
    ryeos_term_handle_exit "$status"
    rm -f "$entries" "$listing"
    rm -rf "$extracted"
    return "$status"
}
trap 'cleanup_verify "$?"' EXIT
tar --absolute-names -tzf "$archive" > "$entries"
tar --absolute-names -tvzf "$archive" > "$listing"

if awk -v root="$archive_root" '
    $0 != root && $0 != root "/" && index($0, root "/") != 1 { bad = 1 }
    /(^|\/)\.\.($|\/)/ || /^\// { bad = 1 }
    END { exit !bad }
' "$entries"; then
    ryeos_term_fail "path escapes $archive_root"
    exit 2
fi
if awk '
    substr($1, 1, 1) != "-" && substr($1, 1, 1) != "d" { bad = 1 }
    substr($1, 1, 10) ~ /[sS]/ { bad = 1 }
    END { exit !bad }
' "$listing"; then
    ryeos_term_fail "link, special, setuid, or setgid entry present"
    exit 2
fi
duplicate_entry="$(awk 'seen[$0]++ { print; exit }' "$entries")"
[[ -z "$duplicate_entry" ]] || {
    ryeos_term_fail "duplicate archive entry: $duplicate_entry"
    exit 2
}

# The path and entry-type checks above make extraction into this private
# directory safe. Mirror the packager's leakage guards before treating a
# previously published archive as canonical on a release retry.
tar --no-same-owner --no-same-permissions -xzf "$archive" -C "$extracted"
stage="$extracted/$archive_root"
[[ -d "$stage" ]] || {
    ryeos_term_fail "archive root directory missing after extraction"
    exit 2
}
# The private extraction tree contains no links or special files at this point.
# Make every staged path inspectable so restrictive archive modes cannot hide a
# textual key marker from the leakage scan below.
chmod -R u+rX "$stage"
while IFS= read -r -d '' staged_path; do
    if [[ "$staged_path" =~ [[:cntrl:]] ]]; then
        ryeos_term_fail "control characters are not allowed in archive paths"
        exit 2
    fi
done < <(find "$stage" -print0)
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
    ryeos_term_fail "possible private key file present: $unsafe_name"
    exit 2
}
private_key_markers="$(
    grep -IRlE -- '-----BEGIN ([A-Z0-9]+ )?(ENCRYPTED )?PRIVATE KEY-----' "$stage" \
        || true
)"
[[ -z "$private_key_markers" ]] || {
    ryeos_term_fail "private key material present: $private_key_markers"
    exit 2
}

grep -qx "$archive_root/.ai/PUBLISHER_TRUST.toml" "$entries" || {
    ryeos_term_fail "source-root publisher metadata missing"
    exit 2
}
# shellcheck source=scripts/pkg/bundle-sets.sh
source "$root/scripts/pkg/bundle-sets.sh"
while IFS= read -r bundle; do
    grep -qx "$archive_root/$bundle/.ai/" "$entries" || {
        ryeos_term_fail "$bundle/.ai missing"
        exit 2
    }
    grep -qx "$archive_root/$bundle/PUBLISHER_TRUST.toml" "$entries" || {
        ryeos_term_fail "$bundle publisher metadata missing"
        exit 2
    }
done < <(ryeos_bundle_set_names full)

official_fp="$("$root/scripts/release/official-publisher-fingerprint.sh")"
trust_doc="$(tar -xOzf "$archive" "$archive_root/.ai/PUBLISHER_TRUST.toml")"
artifact_fp="$(printf '%s\n' "$trust_doc" | sed -n 's/^[[:space:]]*fingerprint[[:space:]]*=[[:space:]]*"\([0-9A-Fa-f]\{64\}\)"[[:space:]]*$/\1/p')"
artifact_owner="$(printf '%s\n' "$trust_doc" | sed -n 's/^[[:space:]]*owner[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p')"
[[ "${artifact_fp,,}" == "$official_fp" && "$artifact_owner" == ryeos-official ]] || {
    ryeos_term_fail "official publisher metadata mismatch"
    exit 2
}

ryeos_term_end success "VERIFY COMPLETE" "$archive"
