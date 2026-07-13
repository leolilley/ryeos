<!-- ryeos:signed:2026-07-13T07:43:47Z:4d61a42e541c6686aee56eef149c47ee3d3bd82055b40a1533b7166953f0d8f3:/C+ORQuvjz+yX6eMIs21j0zjH8c7zJ0AUvKJEZS4J7t7WdgOhSQqSt7qSlVwu24gcwmuATDeTvewrqFaB5KwBw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: "ryeos/development"
name: "build-and-test"
title: "Build, Test, and Local Install Runbook"
description: "LLM-facing commands for building, signing bundles, testing, and local packaged installs"
entry_type: reference
version: "1.2.0"
```

# Build, Test, and Local Install Runbook

Use this as the first operational reference when an agent needs to build,
test, refresh bundles, or install this checkout locally.

## Command matrix

| Goal | Command |
|---|---|
| Full gate | `./scripts/gate.sh` |
| Rebuild/sign bundles only | `./scripts/gate.sh --no-tests` |
| Forward nextest args | `./scripts/gate.sh -p ryeos-cli` |
| Fresh repo-local daemon | `./scripts/dev-up.sh` |
| Fast packaged-layout install | `./scripts/pkg/install-local-direct.sh --trust-source-publishers` |
| Verify source bundles | `target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core`<br>`target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core` |

Prereqs: Rust stable, `cargo-nextest`, Linux, and usually `HOSTNAME` set.

## Canonical gate

```bash
./scripts/gate.sh
```

`gate.sh` is the CI/human default. It runs `scripts/populate-bundles.sh`, then
`cargo nextest run --workspace --no-fail-fast`.

Use `--no-tests` when you only need bundle bin/CAS/signature state refreshed:

```bash
./scripts/gate.sh --no-tests
```

## Bundle refresh rules

Run `scripts/populate-bundles.sh` through `gate.sh` unless you have a reason to
call it directly:

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev \
  --all
```

`--all` is required — populate refuses to rebuild the whole bundle set
implicitly (exits 2 otherwise). Use `--crates "<crate ...>"` to rebuild only
what changed (e.g. `--crates ryeos-core-tools`), and `--jobs N` to cap parallelism if
a full release build runs the machine out of memory.

It does all of this as one atomic authoring refresh:

1. builds release binaries owned by bundles;
2. deletes derived bundle state: `.ai/bin`, `.ai/objects`, `.ai/refs`, stale
   `PUBLISHER_TRUST.toml`;
3. stages binaries into `bundles/{core,standard}/.ai/bin/<triple>/`;
4. runs `ryeos-core-tools build` for core and standard, which signs items and
   rebuilds CAS manifests.

Hard rules:

- Do not manually copy one binary into a bundle as a fix.
- Do not edit signed bundle YAML and leave the old signature.
- Do not verify source bundles without `--registry-root`; installed bundle
  registrations may be stale while you are repairing the source tree.
- Use `.dev-keys/PUBLISHER_DEV.pem` for dev bundles. Do not use old `--seed 42`
  docs or ad-hoc keys.

## Init/reinit after bundle changes

After a merge or bundle/binary refresh, install the refreshed source bundles
into the system space actually used by the CLI/daemon.

Default user system space:

```bash
target/release/ryeos init \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

Repo-local dev system space, matching `scripts/dev-up.sh`:

```bash
target/release/ryeos init \
  --system-space-dir .local/ryeos \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

If a daemon is running against that system space, stop it before init and start
it after. A running daemon keeps the old in-memory engine/registries.

```bash
target/release/ryeos stop --force || true
target/release/ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
target/release/ryeos start
```

## Local packaged-layout install

Use this to install the current checkout into the same layout as a package,
without running `makepkg`/`yay`:

```bash
./scripts/pkg/install-local-direct.sh --trust-source-publishers
```

It populates bundles, stops a running daemon before replacing files, installs
`ryeos`/`ryeosd` to `/usr/bin`, installs bundle sources under
`/usr/share/ryeos/{core,standard}`, runs `ryeos init`, verifies the initialized
bundle state, and restarts the daemon if it was running before.

Important: bundle-owned binaries (`ryeos-core-tools`, parsers, composers,
runtimes, `ryeos-tui`, etc.) belong inside signed bundle bin trees under
`/usr/share/ryeos/<bundle>/.ai/bin/<triple>/`; they should not be installed on
PATH.

Post-install smoke:

```bash
command -v ryeos                 # should be /usr/bin/ryeos
ryeos status
ryeos execute tool:ryeos/core/identity/public_key
script -q -c 'ryeos tui --mock' /tmp/ryeos-tui-smoke.log
```

If `ryeos tui` works but `ryeos help tui` fails, fix the CLI help path. Do not
work around it by adding kind-specific CLI dispatch logic.

## Common failures

| Symptom | Cause | Fix |
|---|---|---|
| `hash mismatch` | Bundle binary/CAS manifest stale | `./scripts/gate.sh --no-tests` |
| `no kind schema roots found` | Core bundle not initialized in active system space | `ryeos init --source bundles ...` |
| `signature ... not in trust store` | Wrong signing key or missing trust file | Repopulate with `.dev-keys/PUBLISHER_DEV.pem`, init with `PUBLISHER_DEV_TRUST.toml` |
| `failed to acquire state lock` | Another daemon owns state | `ryeos stop --force`, then retry |
| `unknown variant ... expected ...` during publish/verify | New descriptor language but old binaries, or missing Rust support | Build/fix Rust first, then repopulate; do not add YAML fallbacks |

## Script intent

| Script | Use it for | Notes |
|---|---|---|
| `scripts/gate.sh` | canonical validation | builds/signs bundles, then nextest |
| `scripts/populate-bundles.sh` | bundle authoring refresh | derived state only; safe to rerun |
| `scripts/dev-up.sh` | isolated repo-local daemon | uses `.local/ryeos` system space |
| `scripts/pkg/install-local-direct.sh` | fast local packaged install | uses `/usr/bin` + `/usr/share/ryeos` |
| `scripts/smoke-execute-stream.sh` | signed `/execute/stream` SSE smoke | needs URL, key, audience |
