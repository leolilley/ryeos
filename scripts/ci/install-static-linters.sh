#!/usr/bin/env bash

set -euo pipefail

destination="${1:?usage: install-static-linters.sh DESTINATION}"
mkdir -p "$destination"

actionlint_version="1.7.10"
actionlint_archive="actionlint_${actionlint_version}_linux_amd64.tar.gz"
actionlint_sha256="f4c76b71db5755a713e6055cbb0857ed07e103e028bda117817660ebadb4386f"
actionlint_url="https://github.com/rhysd/actionlint/releases/download/v${actionlint_version}/${actionlint_archive}"

shellcheck_version="v0.10.0"
shellcheck_archive="shellcheck-${shellcheck_version}.linux.x86_64.tar.xz"
shellcheck_sha256="6c881ab0698e4e6ea235245f22832860544f17ba386442fe7e9d629f8cbedf87"
shellcheck_url="https://github.com/koalaman/shellcheck/releases/download/${shellcheck_version}/${shellcheck_archive}"

download_and_verify() {
    local url="$1"
    local output="$2"
    local expected_sha256="$3"

    curl --fail --location --proto '=https' --tlsv1.2 \
        --retry 3 --retry-all-errors \
        --output "$output" "$url"
    printf '%s  %s\n' "$expected_sha256" "$output" | sha256sum --check --status
}

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

download_and_verify \
    "$actionlint_url" "$work_dir/$actionlint_archive" "$actionlint_sha256"
tar -xzf "$work_dir/$actionlint_archive" -C "$work_dir" actionlint
install -m 0755 "$work_dir/actionlint" "$destination/actionlint"

download_and_verify \
    "$shellcheck_url" "$work_dir/$shellcheck_archive" "$shellcheck_sha256"
tar -xJf "$work_dir/$shellcheck_archive" -C "$work_dir" \
    "shellcheck-${shellcheck_version}/shellcheck"
install -m 0755 \
    "$work_dir/shellcheck-${shellcheck_version}/shellcheck" \
    "$destination/shellcheck"
