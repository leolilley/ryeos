# Dev-Tree Caveats

Notes for contributors working in the local checkout. None of these affect end-user installs.

## Single-key signing model

Every signable artifact in the dev bundle tree (`ryeos-bundles/{core,standard}`) is signed with the **official publisher key** at `~/.ai/config/keys/signing/private_key.pem`. That single key covers:

- Kind schemas and handler descriptor YAMLs
- Binary `item_source.json` sidecars produced by `ryeos publish rebuild-manifest`
- The `MANIFEST.json` and `refs/bundles/manifest` it emits

The trust store the test harness pins (`ryeos_engine::test_support::live_trust_store`) is therefore one-entry: the official publisher public key (`OFFICIAL_PUBLISHER_FP` in `ryeos-tools/src/actions/init.rs`). The previous publisher-seed key (`[42; 32]`) is **only** retained inside the daemon `fast_fixture` for self-signed test content (directives, routes, providers); it is not required for bundle artifacts and is no longer trusted by the engine test helpers.

## `ryeos-core-tools` symlink invalidates `core`'s manifest after `cargo build`

`ryeos-bundles/core/.ai/bin/<host-triple>/ryeos-core-tools` is a symlink to `target/debug/ryeos-core-tools`. The bundle's `refs/bundles/manifest` records the sha256 of the binary file. Any `cargo build` cycle that recompiles `ryeos-core-tools` produces a new binary, so the symlinked file's hash diverges from the manifest entry.

Because `bin:` resolution requires a hash match (no soft fallback), every `tool:ryeos/core/{fetch,verify,identity}` invocation will fail with `BinHashMismatch` until the manifest is rebuilt.

### Symptom — primary

The error message is the giveaway. Look for `ryeos-core-tools` hash mismatch in HTTP body or panic output:

```
binary `ryeos-core-tools` hash mismatch: manifest declares <hash-A>,
on-disk computed <hash-B>
```

### Symptom — daemon won't start

If the dev tree's `core` bundle is invalid for ANY reason (stale manifest, mismatched signer fingerprint, missing CAS object), tests that spin up a real daemon will fail with:

```
start daemon: daemon.json never appeared at /tmp/.tmpXXX/state/daemon.json
```

The daemon either fails its boot consistency check or gets stuck partway through bootstrap.

If you see the daemon-startup symptom, **first** confirm the `ryeos-core-tools` symlink hash matches the manifest:

```bash
sha256sum ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/ryeos-core-tools
# Compare to the entry in:
cat ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/MANIFEST.json | grep -A1 ryeos-core-tools
```

If they differ, this is the root cause regardless of which test is failing.

### Fix — preferred

Run the gate wrapper. It detects the drift, runs `rebuild-manifest` with the correct official publisher key, then runs nextest:

```bash
./scripts/gate.sh
```

To sync the manifest without running tests:

```bash
./scripts/gate.sh --no-tests
```

The script is idempotent — when on-disk and manifest hashes already match it prints `ryeos-core-tools hash matches manifest (<hash>)` and skips the rebuild.

### Fix — manual

```bash
cargo run --bin ryeos publish -- rebuild-manifest \
    --source ryeos-bundles/core \
    --key ~/.ai/config/keys/signing/private_key.pem
```

That re-hashes the on-disk binary, regenerates `MANIFEST.json` + the CAS-stored `SourceManifest` object + the signed `ryeos-core-tools.item_source.json` sidecar, and updates `refs/bundles/manifest` to point at the new manifest hash. Signs everything with the official publisher key (the single key the engine test harness trusts).

### Common LLM mistake — DO NOT use `--seed 42`

Older docs may reference `--seed 42` for `rebuild-manifest`. **Do not use it for the `core` bundle.** The seed-42 key is NOT in the engine test trust store. Rebuilding with `--seed 42` produces a manifest signed by the wrong fingerprint and breaks ~130 tests workspace-wide with "signature from fingerprint X not in trust store" errors.

If you accidentally rebuilt with `--seed 42`:

```bash
git checkout ryeos-bundles/                       # revert manifest + item_source.json sidecars
rm -rf ryeos-bundles/core/.ai/objects/blobs/*/    # remove orphan CAS blobs
```

Then rebuild correctly with `--key ~/.ai/config/keys/signing/private_key.pem`.

The `--seed` flag exists only for self-signed daemon `fast_fixture` content (directives, routes, providers inside daemon test fixtures) — never for bundle artifacts the engine trust store has to verify.

## Why the symlink at all

Convenience for development. The dev tree's "core" bundle is a working bundle that the daemon can resolve against, but the binary it advertises is whatever you just built. End-user installs avoid the issue entirely because `ryeos init` copies the bundle to `system_data_dir` as a static artifact — the user's installed `core` always has a real binary file, not a symlink.

## What about `standard`?

The standard bundle (`ryeos-bundles/standard/.ai/bin/<host-triple>/`) ships with **real binary files** committed to the repo (no symlinks). Its manifest only invalidates if those committed binaries are intentionally replaced. Don't replace them casually — if you do, run `rebuild-manifest --source ryeos-bundles/standard --key ~/.ai/config/keys/signing/private_key.pem` and commit the resulting manifest + item_source changes alongside.

## Workspace gate

The canonical gate is `./scripts/gate.sh`. It auto-syncs the manifest if drift is detected, then runs `cargo nextest run --workspace --no-fail-fast`. Direct `cargo nextest run` invocations are fine but skip the auto-sync.

Do NOT pipe the output through `grep -c FAIL` — that hides which tests failed and forces a rebuild to see them.
