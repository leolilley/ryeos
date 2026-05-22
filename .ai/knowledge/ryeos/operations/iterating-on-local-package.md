---
category: "ryeos/operations"
name: "iterating-on-local-package"
description: "How to iterate on the local AUR package without burning a full yay -Bi rebuild every change"
---

# Iterating On The Local AUR Package

Companion to [local-aur-install.md](./local-aur-install.md). That doc
covers the *what* (the safe install workflow). This doc covers the *how*
of iterating without paying the 1–2 minute `cargo build --release` tax
inside `yay -Bi` for every change.

## The cost you are trying to avoid

`yay -Bi --noconfirm dist/aur/pkgbuild` runs, in order:

1. `cargo fetch` (cold cache: ~30s, warm: instant)
2. `cargo build --release --locked` (cold: 2–4 min, warm: ~40s)
3. `cargo test --release --locked` (cold: 1–2 min, warm: ~15s)
4. `package()` (copy + install, ~5s)
5. `pacman -U` (~3s)

A single test failure inside step 3 burns ~3 minutes and aborts before
any artifact is installed. The pain compounds: dozens of failed `yay`
runs while chasing test fallout is the single biggest time sink in this
workflow.

## The golden rule

> **Make everything green with `cargo` before you ever invoke `yay`.**

The package build adds zero diagnostic value over a local `cargo test
--release --workspace`. Treat `yay -Bi` purely as a packaging smoke test,
not a debugging loop.

## Pre-flight checklist before `yay -Bi`

Run all of these from the repo root. Fix every failure here, **then**
package exactly once.

```bash
# 1. Bundle tree shape matches intent
test ! -d bundles/core/.ai/node/engine/kinds/knowledge   # core does NOT own knowledge
test   -f bundles/standard/.ai/node/engine/kinds/knowledge/knowledge.kind-schema.yaml
grep -q "uses_kinds: \[\]" bundles/standard/.ai/manifest.source.yaml

# 2. Bundles are signed and binaries match manifest hashes
./scripts/populate-bundles.sh \
    --key .dev-keys/PUBLISHER_DEV.pem \
    --owner ryeos-dev

# 3. Full workspace tests pass (the same target yay's check() runs)
cargo test --release --workspace --no-fail-fast

# 4. Clean build check (catches missing features, etc.)
cargo check --release --workspace --all-targets
```

If any step fails, fix it and rerun the failing step only. Only proceed
to packaging when steps 1–4 all pass.

## One-shot package + install + smoke

```bash
./scripts/pkg/prepare-local-aur-source.sh --allow-dirty \
  && yay -Bi --noconfirm dist/aur/pkgbuild \
  && ryeos stop --force 2>/dev/null; \
     rm -rf ~/.local/share/ryeos ~/.ai \
  && ryeos init \
       --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml \
       --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml \
  && ryeos start \
  && ryeos status \
  && ryeos bundle list \
  && ryeos execute tool:ryeos/core/identity/public_key \
  && ryeos stop --force
```

If this whole chain succeeds, the package is shippable. If anything
fails, the failure is real — not a test-isolation flake — and the next
edit must target the root cause, not the symptom.

## Recurring traps and their fixes

### Trap 1 — `makepkg` strips bundle binaries

**Symptom:** post-install `ryeos init` fails with

```text
binary `rye-parser-yaml-document` hash mismatch:
  manifest declares <A>, on-disk computed <B>
```

**Cause:** Bundle binaries under `bundles/<name>/.ai/bin/<triple>/` are
content-addressed; their hashes are baked into signed `*.yaml` handler
manifests by `populate-bundles.sh`. `makepkg`'s default post-build
`Stripping unneeded symbols from binaries and libraries` step rewrites
those binaries, invalidating every manifest hash.

**Fix (already in PKGBUILD):**

```bash
options=('!strip' '!lto')
```

If you ever regenerate the PKGBUILD from scratch, this line is
load-bearing. Cargo's release profile already produces optimized,
debug-stripped binaries — there is nothing for `makepkg` to add.

### Trap 2 — dev-signed bundles vs official publisher trust

**Symptom:** `ryeos init` (no flags) succeeds but bundle preflight
verification fails with an "untrusted signer" error referring to a
fingerprint that is **not** `c9d7301fba468b669d91a6000e9b6a4158c0e615dea4fe1f99906b8c9214bc28`.

**Cause:** Production `ryeos init` only auto-pins the
hardcoded official publisher key. Local builds sign with
`.dev-keys/PUBLISHER_DEV.pem` (fingerprint
`741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea`).
This is intentional: production must not trust arbitrary on-disk bundle
contents.

**Fix:** The package now ships per-bundle `PUBLISHER_TRUST.toml` files
under `/usr/share/ryeos/<name>/PUBLISHER_TRUST.toml`. Pass them at init:

```bash
ryeos init \
    --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml \
    --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml
```

The post-install hook prints this exact command after every install.

### Trap 3 — bundle tree drift across sessions

**Symptom:** Tests pass yesterday, fail today with no source changes.
`derive_provides_kinds` reports `knowledge` in core when it should not,
or vice versa.

**Cause:** `bundles/core/.ai/node/engine/kinds/knowledge/` is the kind of
directory that gets accidentally re-created by old scripts, ad-hoc
copies, or partial git operations. Source of truth is the committed
state — when in doubt, `git status` and `git diff` against `bundles/`.

**Fix:** Re-assert the intended shape, then republish:

```bash
rm -rf bundles/core/.ai/node/engine/kinds/knowledge
test -f bundles/standard/.ai/node/engine/kinds/knowledge/knowledge.kind-schema.yaml \
    || echo "MISSING — restore from git: git checkout HEAD -- bundles/standard/.ai/node/engine/kinds/knowledge"
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
```

### Trap 4 — flaky USER_SPACE test isolation

**Symptom:** `cargo test --release -p ryeos-cli` passes by itself but
fails in `--workspace` runs with `default_uses_cwd_when_cwd_contains_dot_ai`
or similar.

**Cause:** Two CLI test modules (`project_resolve`, `dispatcher`) both
mutate the process-wide `USER_SPACE` env var. They must share a single
mutex.

**Fix (already in place):** Use the shared `crate::test_env::lock()`
helper defined in `crates/bin/cli/src/test_env.rs`, wired into both
`src/lib.rs` and `src/main.rs`. Never reintroduce a module-local
`ENV_MUTEX`.

### Trap 5 — flaky scheduler timing tests under load

**Symptom:** `scheduler_pause_prevents_fires` fails in `--workspace`
runs but passes when run alone.

**Cause:** Wall-clock-based scheduler tests race when the system is busy
compiling and running other tests in parallel. Not a real bug.

**Fix:** Re-run the single test:

```bash
cargo test --release -p ryeosd --test scheduler_e2e scheduler_pause_prevents_fires
```

A green isolated run confirms the workspace failure was load-induced.
Do not chase it with code changes.

## Mapping live commands to package commands

When iterating, run against `target/release/ryeos` (built by
`cargo build --release` or `populate-bundles.sh`) instead of the
installed `/usr/bin/ryeos`. Identical behavior, zero packaging cycle.

| What you want                | Iteration command                                                 |
| ---                          | ---                                                               |
| Bring daemon up              | `target/release/ryeos start --system-space-dir /tmp/rs`           |
| Run a tool                   | `target/release/ryeos execute tool:ryeos/core/identity/public_key`|
| Reset local state            | `rm -rf /tmp/rs ~/.ai ~/.local/share/ryeos`                       |
| Re-sign bundles              | `./scripts/populate-bundles.sh --key … --owner …`                 |

Use `yay -Bi` only when you have explicitly changed:

- `deploy/aur/ryeos/PKGBUILD` or `ryeos.install`
- `scripts/pkg/prepare-local-aur-source.sh`
- The set or layout of installed binaries
- The shape of `/usr/share/ryeos/<bundle>/` (bundle name, `.ai/`
  contents, `PUBLISHER_TRUST.toml` presence)

## What "done" looks like

The package install is verified end-to-end when, against your real
`~/.ai` and `~/.local/share/ryeos`:

```bash
ryeos init --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml \
           --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml
ryeos start
ryeos status                                # → running, pid, url, socket
ryeos bundle list                           # → core + standard
ryeos identity public-key                   # → node fingerprint
ryeos execute tool:ryeos/core/identity/public_key  # → signed identity payload
ryeos stop --force
```

Every line returning a non-error JSON payload (or "stopped") means a
single `yay -Bi` was enough.

## Tear-down

Remove just the package; user space and node state survive:

```bash
sudo pacman -R ryeos
```

Remove everything:

```bash
sudo pacman -R ryeos
rm -rf ~/.ai ~/.local/share/ryeos
```

`dist/aur/` and the tarball are disposable — safe to `rm -rf` whenever
you want to reclaim disk.
