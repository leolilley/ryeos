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

> **Status:** the `cargo nextest run --workspace` concurrency variant
> of this race (~5 `service_data_e2e` tests failing because nextest
> rebuilt `rye-inspect` mid-run) was closed structurally by commit θ
> of the Protocols-as-Data Stabilization wave. Tests that need a
> manifest-stable bundle copy now use
> [`ryeos_tools::test_support::isolated_core_bundle`] instead of
> `system_data_dir()`. Direct manual invocations still hit this caveat
> and the `gate.sh` re-sync remains the fix for them.

### Symptom — primary

The error message is the giveaway. Look for `rye-inspect` hash mismatch in HTTP body or panic output:

```
binary `rye-inspect` hash mismatch: manifest declares <hash-A>,
on-disk computed <hash-B>
```

This surfaces in many test files via 502 Bad Gateway responses. Common failing tests:

```
test tool_fetch_resolves_known_service          ... FAILED
test tool_verify_returns_trusted_for_core_service ... FAILED
test tool_fetch_with_content_includes_body      ... FAILED
test tool_fetch_unknown_ref_errors              ... FAILED
test tool_identity_public_key_returns_doc       ... FAILED
```

(or any other test that routes through `bin:rye-inspect`.)

### Symptom — also caused by this issue

If the dev tree's `core` bundle is invalid for ANY reason (stale manifest, mismatched signer fingerprint, missing CAS object), tests that spin up a real daemon will fail with messages like:

```
start daemon: daemon.json never appeared at /tmp/.tmpXXX/state/daemon.json
```

The daemon either fails its boot consistency check or gets stuck partway through bootstrap. Most `cleanup_e2e.rs`, `service_data_e2e.rs`, and `bundle_parity.rs` tests exhibit this.

If you see the daemon-startup symptom and the test is one that spins up a real daemon, **first** confirm the `rye-inspect` symlink hash matches the manifest:

```bash
sha256sum ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/rye-inspect
# Compare to the entry in:
cat ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/MANIFEST.json | grep -A1 rye-inspect
```

If they differ, this is the root cause regardless of which test is failing.

### Fix — preferred

Run the gate wrapper. It detects the drift, runs `rebuild-manifest` with the correct platform-author key, then runs nextest:

```bash
./scripts/gate.sh
```

To sync the manifest without running tests:

```bash
./scripts/gate.sh --no-tests
```

The script is idempotent — when on-disk and manifest hashes already match it prints `rye-inspect hash matches manifest (<hash>)` and skips the rebuild.

### Fix — manual

```bash
cargo run --bin rye-bundle-tool -- rebuild-manifest \
    --source ryeos-bundles/core \
    --key ~/.ai/config/keys/signing/private_key.pem
```

That re-hashes the on-disk binary, regenerates `MANIFEST.json` + the CAS-stored `SourceManifest` object + the signed `rye-inspect.item_source.json` sidecar, and updates `refs/bundles/manifest` to point at the new manifest hash. Signs everything with the platform-author key (the single key the engine test harness trusts).

### Common LLM mistake — DO NOT use `--seed 42`

Older docs may reference `--seed 42` for `rebuild-manifest`. **Do not use it for the `core` bundle.** The seed-42 key is NOT in the engine test trust store. Rebuilding with `--seed 42` produces a manifest signed by the wrong fingerprint and breaks ~130 tests workspace-wide with "signature from fingerprint X not in trust store" errors.

If you accidentally rebuilt with `--seed 42`:

```bash
git checkout ryeos-bundles/                       # revert manifest + item_source.json sidecars
rm -rf ryeos-bundles/core/.ai/objects/blobs/*/    # remove orphan CAS blobs the seed-key rebuild created
rm -rf ryeos-bundles/core/.ai/objects/objects/*/  # (clean up only the directories git status reports as untracked)
```

Then rebuild correctly with `--key ~/.ai/config/keys/signing/private_key.pem`.

The `--seed` flag exists only for self-signed daemon `fast_fixture` content (directives, routes, providers inside daemon test fixtures) — never for bundle artifacts the engine trust store has to verify.

Run the workspace gate after to confirm:

```bash
./scripts/gate.sh
```

Or manually:

```bash
cargo nextest run --workspace --no-fail-fast
```

Do NOT pipe through `grep -c FAIL` — that swallows error output and forces a full rebuild just to see which tests failed. nextest's exit code is 0 on success.

### Why the symlink at all

Convenience for development. The dev tree's "core" bundle is a working bundle that the daemon can resolve against, but the binary it advertises is whatever you just built. End-user installs avoid the issue entirely because `rye init` copies the bundle to `system_data_dir` as a static artifact — the user's installed `core` always has a real binary file, not a symlink.

### What about `standard`?

The standard bundle (`ryeos-bundles/standard/.ai/bin/<host-triple>/`) ships with **real binary files** committed to the repo (no symlinks). Its manifest only invalidates if those committed binaries are intentionally replaced. Don't replace them casually — if you do, run `rebuild-manifest --source ryeos-bundles/standard --key ~/.ai/config/keys/signing/private_key.pem` and commit the resulting manifest + item_source changes alongside.

### Workspace gate

The canonical gate is `./scripts/gate.sh`. It auto-syncs the manifest if drift is detected, then runs `cargo nextest run --workspace --no-fail-fast`. Direct `cargo nextest run` invocations are fine but skip the auto-sync. Do NOT pipe the output through `grep -c FAIL` — that hides which tests failed and forces a rebuild to see them.

### Future work

One cleanup remains on the radar (not yet scheduled): investigate making the manifest verification tolerant of "dev-tree symlink → just-rebuilt binary" without weakening the production trust contract. Probably a config flag the test harness opts into, never enabled in production builds.

(The "auto rebuild-manifest before tests" cleanup is now handled by `scripts/gate.sh`.)

(The "concurrent nextest race rebuilds rye-inspect mid-run" issue was closed by commit θ of the Protocols-as-Data Stabilization wave — see `ryeos_tools::test_support::isolated_core_bundle` and the migration in `ryeosd/tests/service_data_e2e.rs`.)
