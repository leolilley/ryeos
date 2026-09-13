#!/usr/bin/env bash
# Re-sign one or more source/bundle files with the local DEV publisher key
# (.dev-keys/PUBLISHER_DEV.pem) — the same key, fingerprint, and signing scheme
# `populate-bundles.sh` uses, but for individual files. Use this after editing a
# single signed item (e.g. a node/commands or kind-schema YAML) so you don't have
# to run a full `--populate` rebuild just to make one file's signature valid.
# Also use this per-item path for the repository-root development bundle.
# Whole-directory `build`/`bundle-sign` copies and exchanges its input root;
# it is for detached bundle trees, not this working Git checkout. Project
# snapshot exclusions are not publisher staging filters. This helper signs
# bytes only: validate them through the normal resolver/admission separately.
#
# Usage: scripts/dev/sign-dev.sh <file> [<file> ...]
# Env:   RYEOS_DEV_KEY  override the key path (default: .dev-keys/PUBLISHER_DEV.pem)
#
# Envelope is chosen by extension: `# ryeos:signed:...` for
# .yaml/.yml/.toml/.py, the same comment after a `.sh` shebang, and
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
  local file="$1" prefix suffix strip_re after_shebang
  [[ -f "$file" ]] || { echo "sign-dev: missing file: $file" >&2; return 1; }
  case "$file" in
    *.md)            prefix='<!-- '; suffix=' -->'; strip_re='^<!-- ryeos:signed:'; after_shebang=false ;;
    *.sh)            prefix='# '; suffix=''; strip_re='^# ryeos:signed:'; after_shebang=true ;;
    *.yaml|*.yml|*.toml|*.py) prefix='# '; suffix=''; strip_re='^# ryeos:signed:'; after_shebang=false ;;
    *)               echo "sign-dev: unknown envelope for $file (expected .yaml/.yml/.toml/.py/.sh/.md)" >&2; return 1 ;;
  esac

  local body_tmp signed_body_tmp hash_tmp tmp hash sig timestamp
  body_tmp="$(mktemp)"; signed_body_tmp="$(mktemp)"; hash_tmp="$(mktemp)"; tmp="$file.tmp.$$"
  # Body = file minus existing signature line(s); hash over the body; sign the
  # hex-hash string with the raw ed25519 key.
  sed "/$strip_re/d" "$file" > "$body_tmp"
  if [[ "$after_shebang" == true ]] && head -n 1 "$body_tmp" | grep -q '^#!'; then
    tail -n +2 "$body_tmp" > "$signed_body_tmp"
  else
    cp "$body_tmp" "$signed_body_tmp"
  fi
  hash="$(sha256sum "$signed_body_tmp" | cut -d' ' -f1)"
  printf '%s' "$hash" > "$hash_tmp"
  sig="$(openssl pkeyutl -sign -inkey "$KEY" -rawin -in "$hash_tmp" 2>/dev/null | base64_one_line)"
  timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [[ "$after_shebang" == true ]] && head -n 1 "$body_tmp" | grep -q '^#!'; then
    {
      head -n 1 "$body_tmp"
      printf '%sryeos:signed:%s:%s:%s:%s%s\n' "$prefix" "$timestamp" "$hash" "$sig" "$PUBLISHER_FP" "$suffix"
      cat "$signed_body_tmp"
    } > "$tmp"
  else
    {
      printf '%sryeos:signed:%s:%s:%s:%s%s\n' "$prefix" "$timestamp" "$hash" "$sig" "$PUBLISHER_FP" "$suffix"
      cat "$body_tmp"
    } > "$tmp"
  fi
  chmod --reference="$file" "$tmp"
  mv "$tmp" "$file"
  rm -f "$body_tmp" "$signed_body_tmp" "$hash_tmp"
  echo "signed: $file"
}

for f in "$@"; do sign_file "$f"; done
