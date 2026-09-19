#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 --version VERSION --archive PATH --checksum PATH" >&2
  exit 2
}

version=
archive=
checksum=
while (($#)); do
  case "$1" in
    --version) version="${2:-}"; shift 2 ;;
    --archive) archive="${2:-}"; shift 2 ;;
    --checksum) checksum="${2:-}"; shift 2 ;;
    *) usage ;;
  esac
done

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || usage
[[ -f "$archive" && ! -L "$archive" && -f "$checksum" && ! -L "$checksum" ]] || {
  echo "contained OCI hook archive or checksum is absent/unsafe" >&2
  exit 1
}
expected="ryeos-contained-oci-hook-${version}-x86_64-unknown-linux-gnu.tar.gz"
[[ "$(basename "$archive")" == "$expected" ]] || {
  echo "unexpected contained OCI hook archive name" >&2
  exit 1
}
[[ "$(basename "$checksum")" == "${expected}.sha256" ]] || {
  echo "unexpected contained OCI hook checksum name" >&2
  exit 1
}
[[ "$(wc -l < "$checksum")" -eq 1 ]] || {
  echo "contained OCI hook checksum must contain exactly one record" >&2
  exit 1
}
read -r expected_digest expected_name extra < "$checksum"
actual_digest="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$expected_digest" =~ ^[0-9a-f]{64}$ && "$expected_name" == "$expected" \
    && -z "${extra:-}" && "$(<"$checksum")" == "$expected_digest  $expected" \
    && "$actual_digest" == "$expected_digest" ]] || {
  echo "contained OCI hook checksum is malformed or does not match this archive" >&2
  exit 1
}
entries="$(tar -tzf "$archive" | LC_ALL=C sort)"
[[ "$entries" == $'LICENSE\nryeos-lillux-oci-hook' ]] || {
  echo "contained OCI hook archive inventory is not exact" >&2
  exit 1
}
mode="$(tar -tvzf "$archive" ryeos-lillux-oci-hook | awk '{print $1}')"
[[ "$mode" == "-r-xr-xr-x" ]] || {
  echo "contained OCI hook is not packaged mode 0555" >&2
  exit 1
}
