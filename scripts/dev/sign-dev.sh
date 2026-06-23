#!/usr/bin/env bash
# Re-sign one or more source/bundle files with the local DEV publisher key
# (.dev-keys/PUBLISHER_DEV.pem) — the same key, fingerprint, and signing scheme
# `populate-bundles.sh` uses, but for individual files. Use this after editing a
# single signed item (e.g. a node/commands or kind-schema YAML) so you don't have
# to run a full `--populate` rebuild just to make one file's signature valid.
#
# Usage: scripts/dev/sign-dev.sh <file> [<file> ...]
# Env:   RYEOS_DEV_KEY  override the key path (default: .dev-keys/PUBLISHER_DEV.pem)
#
# Envelope is chosen by extension: `# ryeos:signed:...` for .yaml/.yml/.toml,
# `<!-- ryeos:signed:... -->` for .md.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KEY="${RYEOS_DEV_KEY:-$ROOT/.dev-keys/PUBLISHER_DEV.pem}"

[[ -f "$KEY" ]] || { echo "sign-dev: dev key not found: $KEY" >&2; exit 2; }
[[ $# -ge 1 ]] || { echo "Usage: scripts/dev/sign-dev.sh <file> [<file> ...]" >&2; exit 2; }

base64_one_line() { base64 -w0 2>/dev/null || base64 | tr -d '\n'; }

# Fingerprint = sha256 of the raw 32-byte ed25519 public key (matches
# populate-bundles.sh `publisher_fingerprint`).
PUBLISHER_FP="$(openssl pkey -in "$KEY" -pubout -outform DER 2>/dev/null \
  | tail -c 32 | sha256sum | cut -d' ' -f1)"

sign_file() {
  local file="$1" prefix suffix strip_re
  [[ -f "$file" ]] || { echo "sign-dev: missing file: $file" >&2; return 1; }
  case "$file" in
    *.md)            prefix='<!-- '; suffix=' -->'; strip_re='^<!-- ryeos:signed:' ;;
    *.yaml|*.yml|*.toml) prefix='# '; suffix='';     strip_re='^# ryeos:signed:' ;;
    *)               echo "sign-dev: unknown envelope for $file (expected .yaml/.yml/.toml/.md)" >&2; return 1 ;;
  esac

  local body_tmp hash_tmp tmp hash sig timestamp
  body_tmp="$(mktemp)"; hash_tmp="$(mktemp)"; tmp="$file.tmp.$$"
  # Body = file minus existing signature line(s); hash over the body; sign the
  # hex-hash string with the raw ed25519 key.
  sed "/$strip_re/d" "$file" > "$body_tmp"
  hash="$(sha256sum "$body_tmp" | cut -d' ' -f1)"
  printf '%s' "$hash" > "$hash_tmp"
  sig="$(openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$hash_tmp" 2>/dev/null | base64_one_line)"
  timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  {
    printf '%sryeos:signed:%s:%s:%s:%s%s\n' "$prefix" "$timestamp" "$hash" "$sig" "$PUBLISHER_FP" "$suffix"
    cat "$body_tmp"
  } > "$tmp"
  mv "$tmp" "$file"
  rm -f "$body_tmp" "$hash_tmp"
  echo "signed: $file"
}

for f in "$@"; do sign_file "$f"; done
