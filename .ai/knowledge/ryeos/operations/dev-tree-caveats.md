---
category: "ryeos/operations"
name: "dev-tree-caveats"
description: "Working with dev builds: manifest hashes, symlink pitfalls, signing key gotchas"
---

# Dev-Tree Caveats

Notes for contributors working in the local checkout. None of these affect end-user installs.

## Single-key signing model

Every signable artifact in the dev bundle tree (`ryeos-bundles/{core,standard}`) is signed with the **dev publisher key** at `.dev-keys/PUBLISHER_DEV.pem`. The trust store the test harness pins is one-entry: the dev publisher fingerprint.

## ryeos-core-tools symlink invalidates core's manifest

`ryeos-bundles/core/.ai/bin/<host-triple>/ryeos-core-tools` is a symlink to `target/debug/ryeos-core-tools`. Any `cargo build` that recompiles `ryeos-core-tools` produces a new binary, so the symlinked file's hash diverges from the manifest entry.

Because `bin:` resolution requires a hash match, every `tool:ryeos/core/{fetch,verify,identity}` invocation will fail until the manifest is rebuilt.

### Symptom

```
binary `ryeos-core-tools` hash mismatch: manifest declares <hash-A>,
on-disk computed <hash-B>
```

Or the daemon won't start:
```
start daemon: daemon.json never appeared at /tmp/.tmpXXX/state/daemon.json
```

### Fix

```bash
# Preferred: auto-detects drift and rebuilds
./scripts/gate.sh

# Or without running tests
./scripts/gate.sh --no-tests

# Manual
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

### DO NOT use `--seed 42`

The seed-42 key is NOT in the engine test trust store. Using it for bundle artifacts breaks ~130 tests with "signature not in trust store."

Recovery if you accidentally used it:
```bash
git checkout ryeos-bundles/
rm -rf ryeos-bundles/core/.ai/objects/blobs/*/
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
```

## The standard bundle

`ryeos-bundles/standard/.ai/bin/<triple>/` ships with **real binary files** committed to the repo (no symlinks). Its manifest only invalidates if those binaries are replaced. If you do replace them, re-sign:
```bash
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
```

## Workspace gate

The canonical gate is `./scripts/gate.sh`. Direct `cargo nextest run` is fine but skips the auto-sync.
