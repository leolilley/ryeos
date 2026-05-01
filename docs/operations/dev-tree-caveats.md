# Dev-Tree Caveats

Notes for contributors working in the local checkout. None of these affect end-user installs.

## Single-key signing model

Every signable artifact in the dev bundle tree (`ryeos-bundles/{core,standard}`) is signed with the **platform-author key** at `~/.ai/config/keys/signing/private_key.pem`. That single key covers:

* kind schemas and handler descriptor YAMLs,
* binary `item_source.json` sidecars produced by `rye-bundle-tool rebuild-manifest`,
* the `MANIFEST.json` and `refs/bundles/manifest` it emits.

The trust store the test harness pins (`ryeos_engine::test_support::live_trust_store`) is therefore one-entry: the platform-author public key (`09674c8...`). The previous publisher-seed key (`[42; 32]`) is **only** retained inside the daemon `fast_fixture` for self-signed test content (directives, routes, providers); it is not required for bundle artifacts and is no longer trusted by the engine test helpers.

## `rye-inspect` symlink invalidates `core`'s manifest after `cargo build`

`ryeos-bundles/core/.ai/bin/<host-triple>/rye-inspect` is a symlink to `target/debug/rye-inspect`. The bundle's `refs/bundles/manifest` records the sha256 of the binary file. Any `cargo build` cycle that recompiles `rye-inspect` produces a new binary, so the symlinked file's hash diverges from the manifest entry.

Because `bin:` resolution requires a hash match (Part C of the foundation-hardening wave — no soft fallback), every `tool:rye/core/{fetch,verify,identity}` invocation will fail with `BinHashMismatch` until the manifest is rebuilt.

### Symptom

```
test tool_fetch_resolves_known_service ... FAILED
test tool_verify_returns_trusted_for_core_service ... FAILED
test tool_fetch_with_content_includes_body ... FAILED
test tool_fetch_unknown_ref_errors ... FAILED
test tool_identity_public_key_returns_doc ... FAILED
```

(or any other test that routes through `bin:rye-inspect`.)

### Fix

```bash
cargo run --bin rye-bundle-tool -- rebuild-manifest \
    --source ryeos-bundles/core \
    --key ~/.ai/config/keys/signing/private_key.pem
```

That re-hashes the on-disk binary, regenerates `MANIFEST.json` + the CAS-stored `SourceManifest` object + the signed `rye-inspect.item_source.json` sidecar, and updates `refs/bundles/manifest` to point at the new manifest hash. Signs everything with the platform-author key (the single key the engine test harness trusts).

Run the workspace gate after to confirm:

```bash
cargo test --workspace --no-fail-fast 2>&1 | grep -c FAILED   # → 0
```

### Why the symlink at all

Convenience for development. The dev tree's "core" bundle is a working bundle that the daemon can resolve against, but the binary it advertises is whatever you just built. End-user installs avoid the issue entirely because `rye init` copies the bundle to `system_data_dir` as a static artifact — the user's installed `core` always has a real binary file, not a symlink.

### What about `standard`?

The standard bundle (`ryeos-bundles/standard/.ai/bin/<host-triple>/`) ships with **real binary files** committed to the repo (no symlinks). Its manifest only invalidates if those committed binaries are intentionally replaced. Don't replace them casually — if you do, run `rebuild-manifest --source ryeos-bundles/standard --key ~/.ai/config/keys/signing/private_key.pem` and commit the resulting manifest + item_source changes alongside.

### Future work

Two cleanups on the radar (not yet scheduled):

1. Replace the symlink with a copy step in a `cargo xtask` or pre-test hook that runs `rebuild-manifest` automatically when the binary content changes.
2. Investigate making the manifest verification tolerant of "dev-tree symlink → just-rebuilt binary" without weakening the production trust contract. Probably a config flag the test harness opts into, never enabled in production builds.
