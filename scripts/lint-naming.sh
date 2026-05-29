#!/usr/bin/env bash
# lint-naming.sh — Fails if forbidden patterns reappear in docs or source.
# Run from repo root. Intended as a CI gate.
set -euo pipefail

# Scan everything except generated/build artifacts.
SCAN_ROOTS=(
  deploy/
  bundles/
  ryeos-*/src/
  scripts/
  .ai/
  docs/
  README.md
)

# Build the file list once (skip binary dirs).
FILES=$(rg --files "${SCAN_ROOTS[@]}" 2>/dev/null || true)

if [ -z "$FILES" ]; then
  echo "lint-naming: no files to scan (empty roots?)"
  exit 0
fi

# Patterns that must never reappear. Each entry is a fixed substring
# searched literally (rg without -P).
DENY=(
  '--init-if-missing'              # removed flag
  'default_value = "*"'            # clap default scopes "*" — always wrong now
  'default: *'                     # help text stale default
  'ryeos daemon rotate-key'        # phantom command (never implemented)
  'write_authorized_key_toml_with_wildcard'  # removed in favor of WildcardPolicy enum
  'next_port()'                    # removed in favor of `--bind 127.0.0.1:0`
  'pick_free_port'                 # removed legacy alias
)

# Patterns that catch SHORT-FORM caps appearing as scope examples in
# docs and help text. The real capability format is canonical
# `ryeos.<verb>.<kind>.<subject>`. A bare `bundle.install` or
# `remote.admin` in a `--scopes` example will produce useless
# authorized_keys TOMLs (see crates/engine/ryeos-runtime/src/authorizer.rs).
DENY+=(
  '--scopes "bundle.'              # e.g. --scopes "bundle.install" (short form)
  '--scopes "remote.'              # e.g. --scopes "remote.admin"   (short form)
  '--scopes bundle.'               # un-quoted short form
  '--scopes remote.'               # un-quoted short form
)

found=0
for pattern in "${DENY[@]}"; do
  # -F treats the pattern as a fixed string (no regex), so `*`, `.`,
  # and `(` don't need escaping. The grep filter strips self-matches.
  hits=$(echo "$FILES" | xargs rg -F -n --no-heading "$pattern" 2>/dev/null \
    | grep -v "lint-naming.sh:" || true)
  if [ -n "$hits" ]; then
    echo "ERROR: forbidden pattern '$pattern' found:" >&2
    echo "$hits" >&2
    echo >&2
    found=1
  fi
done

if [ "$found" -eq 1 ]; then
  echo "lint-naming: FAILED — forbidden patterns detected" >&2
  exit 1
fi

echo "lint-naming: clean"
