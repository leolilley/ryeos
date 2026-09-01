<!-- ryeos:signed:2026-09-01T19:44:25Z:a0f39b68214faf14b2b060f2acd262803bb1f83c96563eaeaf6210f2ee0cc16a:OadCXi1oxDay3Qhj6eRo0aVxgbPZnQSvy8KbC0SMK8tbG5AoMoziB9+wYPBSXhFWqtQsIWB95/w8RPoASQ/2Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "build-and-test"
title: "Build, Test, and Local Install Runbook"
description: "LLM-facing commands for building, signing bundles, testing, and local packaged installs"
entry_type: reference
version: "1.4.2"
```

# Build, Test, and Local Install Runbook

Use this as the first operational reference when an agent needs to build,
test, refresh bundles, or install this checkout locally.

## Command matrix

| Goal | Command |
|---|---|
| Full gate | `./scripts/gate.sh` |
| Rebuild/sign bundles only | `./scripts/gate.sh --refresh-bundles --no-tests` |
| Rebuild/sign bundles, then run full gate | `./scripts/gate.sh --refresh-bundles` |
| Rebuild/sign bundles, then run only the serial crash-qualification matrices | `./scripts/gate.sh --refresh-bundles --crash-qualification-only` |
| Forward nextest args | `./scripts/gate.sh -p ryeos-cli` |
| Fresh repo-local daemon | initialize/start with `--app-root .local/ryeos` (commands below) |
| Fast packaged-layout install from already-built artifacts | `./scripts/pkg/install-local-direct.sh --trust-source-publishers` |
| Verify core/standard source bundles | `target/release/ryeos-core-tools bundle-verify bundles/core --registry-root bundles/core`<br>`target/release/ryeos-core-tools bundle-verify bundles/standard --registry-root bundles/core` |
| Verify optional local-inference bundle | `target/release/ryeos-core-tools bundle-verify bundles/local-inference --registry-root bundles/core --registry-root bundles/standard` |

Prereqs: Rust stable, `cargo-nextest`, Linux, and usually `HOSTNAME` set.

## Canonical gate

```bash
./scripts/gate.sh
```

`gate.sh` is the CI/human default for tests. By default it runs
`cargo nextest run --workspace --no-fail-fast` without rebuilding bundles.
Bundle authoring is intentionally explicit because it performs a full release
build and rewrites derived signed bundle state.

Use `--refresh-bundles` when the change affects bundle-owned source, binaries,
CAS state, or signatures. Add `--no-tests` only when you need that authoring
refresh without the test gate:

```bash
./scripts/gate.sh --refresh-bundles --no-tests
```

A full refreshed gate also runs the directive native-resume and explicit
cross-site worker-handoff crash matrices. The handoff matrix is deliberately
serial and takes roughly 40 minutes on the development host. CI therefore
runs the ordinary workspace suite and crash qualification as separate jobs:

```bash
./scripts/gate.sh --refresh-bundles --skip-crash-qualification
./scripts/gate.sh --refresh-bundles --crash-qualification-only
```

Both jobs remain mandatory. The split prevents either validation lane from
consuming the other's timeout; it does not reduce the matrix or its retained
qualification report.

## Bundle refresh rules

Run `scripts/populate-bundles.sh` through the explicit
`gate.sh --refresh-bundles` surface unless you have a reason to call it
directly:

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

Repo-local dev app root:

```bash
target/release/ryeos init \
  --app-root .local/ryeos \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
target/release/ryeos start --app-root .local/ryeos
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

By default it uses already-built checkout artifacts: it stops a running daemon
before replacing files, installs `ryeos`/`ryeosd` to `/usr/bin`, installs bundle sources under
`/usr/share/ryeos/<bundle>` for every bundle in the selected set (the default is
the full set defined by `scripts/pkg/bundle-sets.sh`), runs `ryeos init`,
verifies the initialized bundle state, and restarts the daemon if it was
running before.

To rebuild and republish the complete bundle set as part of the install, opt in
explicitly:

```bash
./scripts/pkg/install-local-direct.sh \
  --populate --all --trust-source-publishers
```

Use `--populate --crates "<crate ...>"` instead when only named bundle-owned
binaries need rebuilding.

### Clean-cut execution-state upgrades

The installer never silently discards authoritative execution history. If an
installed release advances an immutable thread/capsule/project-authority
contract, restart can correctly refuse retained history from the predecessor
epoch. Inspect and apply the explicit offline cutover while the daemon is
stopped:

```bash
ryeos node gc \
  --discard-thread-history \
  --discard-project-heads \
  --dry-run

ryeos node gc \
  --discard-thread-history \
  --discard-project-heads \
  --confirm-discard-thread-history \
  --confirm-discard-project-heads

ryeos start
```

If the runtime schema is incompatible, dry-run reports runtime-row counts as
unavailable instead of decoding those rows or claiming a false zero; the other
retirement counts remain visible. The confirmed report preserves that
unavailable classification instead of presenting the empty post-reset schema
as an exact zero-row retirement.

The confirmed command is destructive to local thread history and project
HEADs, so it requires the operator decision shown above rather than an install
flag or automatic fallback. It preserves project worktrees, installed bundles,
vault values, trust, and node/signing identity. No reinstall is needed after
the cutover when the new binaries and bundles were already installed.

Important: bundle-owned binaries (`ryeos-core-tools`, parsers, composers,
runtimes, `ryeos-tui`, etc.) belong inside signed bundle bin trees under
`/usr/share/ryeos/<bundle>/.ai/bin/<triple>/`; they should not be installed on
PATH.

Post-install smoke:

```bash
command -v ryeos                 # should be /usr/bin/ryeos
ryeos node status
ryeos node doctor --json
ryeos execute tool:ryeos/core/identity/public_key
script -q -c 'ryeos tui --mock' /tmp/ryeos-tui-smoke.log
```

The generated isolation policy is disabled by default. `node doctor` uses the
production policy loader; after changing mode or policy contents, restart the
node before judging execution behavior. See [Execution
Isolation](../../../../bundles/standard/.ai/knowledge/ryeos/core/node/execution-isolation.md).

If `ryeos tui` works but `ryeos help tui` fails, fix the CLI help path. Do not
work around it by adding kind-specific CLI dispatch logic.

## Common failures

| Symptom | Cause | Fix |
|---|---|---|
| `hash mismatch` | Bundle binary/CAS manifest stale | `./scripts/gate.sh --refresh-bundles --no-tests` |
| `no kind schema roots found` | Core bundle not initialized in active system space | `ryeos init --source bundles ...` |
| `signature ... not in trust store` | Wrong signing key or missing trust file | Repopulate with `.dev-keys/PUBLISHER_DEV.pem`, init with `PUBLISHER_DEV_TRUST.toml` |
| `failed to acquire state lock` | Another daemon owns state | `ryeos stop --force`, then retry |
| `unknown variant ... expected ...` during publish/verify | New descriptor language but old binaries, or missing Rust support | Build/fix Rust first, then repopulate; do not add YAML fallbacks |

## Script intent

| Script | Use it for | Notes |
|---|---|---|
| `scripts/gate.sh` | canonical validation | nextest by default; builds/signs bundles first only with `--refresh-bundles` |
| `scripts/populate-bundles.sh` | bundle authoring refresh | derived state only; safe to rerun |
| `scripts/pkg/install-local-direct.sh` | fast local packaged install | uses `/usr/bin` + `/usr/share/ryeos`; populates only with explicit `--populate` |
| `scripts/smoke-execute-stream.sh` | signed `/execute/stream` SSE smoke | needs URL, key, audience |
