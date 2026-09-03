<!-- ryeos:signed:2026-09-03T11:56:15Z:22019d43cbad0a661427f383eedd6c7d361748275da92e5ceed7487eadf200a9:tagbFaG3vzAAZx6tJNUNvldtVGB+5fmUl+BrGXQrV21Q6qnWUdOdOXo6+JPNYk7wZ/vjtNHNlB+C9UVHuwE8BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "release-process"
title: "Release Process"
description: "Checklist for cutting RyeOS releases from next to main without stale versions, tags, or install validation mistakes"
entry_type: reference
version: "1.3.0"
```

# RyeOS Release Process

Use this runbook when cutting a RyeOS release tag.

RyeOS uses two active worktrees in the normal release flow:

| Worktree | Branch | Purpose |
|---|---|---|
| `/home/leo/projects/ryeos-next` | `next` | Development, fixes, release-prep commits |
| `/home/leo/projects/ryeos` | `main` | Release branch and tags |

`next` is the integration branch. `main` is the release branch. A release is
made by committing the fix/version bump on `next`, merging `next` into `main`
from the `main` worktree, tagging the resulting `main` release commit, then
pushing.

The active distribution channel is GHCR. The release tag and Docker/GHCR
workflow are the shipping path. AUR files live in the repo as packaging
scaffolding, but AUR is not currently an active release channel.

## Critical rules

- Do **not** check out `main` in `/home/leo/projects/ryeos-next` if `main` is
  already checked out in `/home/leo/projects/ryeos`.
- Do **not** move a release tag that has already been pushed or consumed. Cut a
  new patch release instead.
- Do **not** forget package version strings. The tag alone is not enough.
- Do **not** confuse a successful long projection rebuild with a daemon startup
  failure.
- Do **not** treat AUR as part of the release unless explicitly requested. The
  active release path is GHCR. Do not publish/update AUR from GitHub's raw tag
  archive unless the AUR artifact flow has been fixed; raw tag archives do not
  contain ignored, generated bundle artifacts from `scripts/populate-bundles.sh`.
- Do **not** stage unrelated untracked files, especially:
  - `.ai/knowledge/ryeos/future/portable-execution-white-paper-thesis.md`

## 0. Confirm worktrees and branches

```bash
git worktree list

git -C /home/leo/projects/ryeos-next branch --show-current
git -C /home/leo/projects/ryeos branch --show-current
```

Expected:

```text
/home/leo/projects/ryeos-next  ... [next]
/home/leo/projects/ryeos       ... [main]
```

If `main` is already checked out in `/home/leo/projects/ryeos`, do not run
`git checkout main` in `/home/leo/projects/ryeos-next`; Git will reject it
because a branch can only be checked out in one worktree at a time.

**Fallback if the `/home/leo/projects/ryeos` worktree does not exist** (it may
not — `main` is then checked out nowhere): do the merge in-place from the
`ryeos-next` checkout instead. Create a local `main` from the remote, merge,
tag, push, then switch back to `next`:

```bash
cd /home/leo/projects/ryeos-next
git fetch origin
git branch -f main origin/main             # create OR reset local main to the remote tip
git checkout main                          # (don't use `checkout -b main` — it errors if a
                                           #  stale local main from a prior release exists)
git merge --no-ff next -m "Merge next into main for v$new release"
# ... validate, tag, push (sections 5–7) ...
git checkout next                          # leave this worktree on next
git branch -D main                         # delete the local main so the next release starts clean
```

Tag and push from the resulting `main` HEAD (the merge commit). Watch out: if
`checkout -b main` silently fails because a local `main` already exists, the
merge runs as a no-op on `next` and the tag lands on a `next` commit while
`origin/main` stays stale. `git branch -f` + deleting `main` afterward avoids
this. This was the path used for the v0.5.16 / v0.5.17 releases.

## 1. Decide whether to move a tag or cut a new patch

Use a new patch version when:

- the previous tag was pushed;
- users or automation may have fetched it;
- install/runtime artifacts may have been built from it;
- the release had runtime issues after publication.

Example: if `v0.5.4` was pushed and had startup/runtime issues, do **not** move
`v0.5.4`; release `v0.5.5`.

Only move/recreate a tag when it is strictly local and unpushed:

```bash
git tag -l vX.Y.Z
git ls-remote --tags origin vX.Y.Z
```

If `git ls-remote` shows the tag on origin, treat it as public and immutable.

## 2. Bump release versions on `next`

Run from `/home/leo/projects/ryeos-next`.

Set the old and new versions:

```bash
old=0.5.4
new=0.5.5
```

Bump these exact files:

```text
crates/kernel/lillux/Cargo.toml
crates/kernel/lillux/pyproject.toml
crates/engine/ryeos-runtime/Cargo.toml
crates/tools/core-tools/Cargo.toml
crates/bin/cli/Cargo.toml
crates/bin/daemon/Cargo.toml
Cargo.lock
```

The root `Cargo.toml` is a workspace manifest and does not currently contain a
workspace package version. Do not invent one.

Suggested bump command:

```bash
cd /home/leo/projects/ryeos-next

files=(
  crates/kernel/lillux/Cargo.toml
  crates/kernel/lillux/pyproject.toml
  crates/engine/ryeos-runtime/Cargo.toml
  crates/tools/core-tools/Cargo.toml
  crates/bin/cli/Cargo.toml
  crates/bin/daemon/Cargo.toml
)

perl -0pi -e "s/version = \"$old\"/version = \"$new\"/g" "${files[@]}"
```

Refresh/check `Cargo.lock` by running Cargo:

```bash
cargo check -p ryeos-node -p ryeos-cli -p ryeosd
```

Then confirm no old release package version remains in the release-version
files or lockfile:

```bash
rg "$old" \
  crates/kernel/lillux/Cargo.toml \
  crates/kernel/lillux/pyproject.toml \
  crates/engine/ryeos-runtime/Cargo.toml \
  crates/tools/core-tools/Cargo.toml \
  crates/bin/cli/Cargo.toml \
  crates/bin/daemon/Cargo.toml \
  Cargo.lock
```

Expected: no matches, unless the old version is intentionally mentioned in
prose outside these files.

## 3. Validate before committing on `next`

Minimum validation:

```bash
cargo check -p ryeos-node -p ryeos-cli -p ryeosd
cargo test -p ryeos-node
cargo test -p ryeos-state
bash -n scripts/pkg/install-local-direct.sh
```

Do not run the broader local gate during the release cut. Leave the full gate to
GitHub Actions after pushing the release branches/tag.

For bundle-aware changes, ensure bundles are freshly populated/signed:

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev \
  --all
```

`--all` is REQUIRED: `populate-bundles.sh` refuses to rebuild the whole bundle
set implicitly (it would otherwise exit 2). Pass `--all` for a full rebuild, or
`--crates "<Cargo package ...>"` for a focused development rebuild (e.g.
`--crates ryeosd` for a daemon-only correction). `--jobs N` caps Cargo
parallelism if a full release build exhausts memory. The release Dockerfiles
already pass `--all`.

Do not manually copy binaries into bundle trees or hand-edit signed bundle YAML
as a release fix.

## 4. Commit on `next`

Review exactly what will be committed:

```bash
git -C /home/leo/projects/ryeos-next status --short
git -C /home/leo/projects/ryeos-next diff --stat
```

Stage only relevant files. Do not stage unrelated untracked docs or local state.

Example:

```bash
git add \
  crates/kernel/lillux/Cargo.toml \
  crates/kernel/lillux/pyproject.toml \
  crates/engine/ryeos-runtime/Cargo.toml \
  crates/tools/core-tools/Cargo.toml \
  crates/bin/cli/Cargo.toml \
  crates/bin/daemon/Cargo.toml \
  Cargo.lock \
  <actual-fix-files>

git commit -m "Fix <release issue> for v$new"
```

## 5. Merge `next` into `main` from the main worktree

Run from `/home/leo/projects/ryeos`, not from `/home/leo/projects/ryeos-next`.

```bash
cd /home/leo/projects/ryeos

git branch --show-current
git status --short
git fetch origin
git merge --no-ff next -m "Merge next into main for v$new release"
```

If there are conflicts in version files or `Cargo.lock`, resolve them to the
new release version.

After conflict resolution:

```bash
cargo check -p ryeos-node -p ryeos-cli -p ryeosd
cargo test -p ryeos-node
cargo test -p ryeos-state

git status --short
git add <resolved-files>
git commit
```

If the merge completed without conflicts, no extra commit command is needed;
the merge commit already exists.

## 6. Tag the release on `main`

Confirm `HEAD` is the intended release commit on `main`:

```bash
cd /home/leo/projects/ryeos

git branch --show-current
git log --oneline --decorate -5
```

Create an annotated tag:

```bash
git tag -a "v$new" -m "RyeOS v$new"
git show --stat "v$new"
```

Verify the tag points at the `main` release commit, not at an older `next`
commit.

## 7. Push order

Push branches first, then the tag. This avoids publishing a tag whose target
commit is not yet reachable from the remote release branch.

```bash
git push origin next main
git push origin "v$new"
```

A single push can work:

```bash
git push origin next main "v$new"
```

But if being careful after a broken release, prefer the two-step branch-then-tag
push.

After pushing:

```bash
git ls-remote --heads origin next main
git ls-remote --tags origin "v$new"
```

## Interrupted release recovery

The release workflow never overwrites immutable image tags or GitHub release
assets. It may resume only when the existing artifact identity is independently
verified: an image must have the expected keyless workflow signature, source
provenance, and SBOM; an existing bundle archive and checksum are downloaded
and verified again; and an archive-only upload may remain canonical after its
officially signed bundle contents pass structural and cryptographic preflight.
Mutable `latest` tags move only after every immutable output passes again.

Two ambiguous states require operator intervention:

- If an immutable image tag exists without the expected workflow signature,
  quarantine it. Confirm the exact digest and absence of the signature, then
  normally burn the incomplete version and cut a new release. Delete that
  registry version only when repository policy explicitly permits reuse and
  you have confirmed it is the incomplete unsigned digest. Never replace a
  signed immutable version.
- If a GitHub release has a checksum but no archive, remove only the confirmed
  orphan checksum through release asset controls and rerun. The checksum cannot
  establish which missing bytes should be restored. This differs from an
  archive-only state, where the archive itself can be verified and its missing
  checksum derived.

## 8. GHCR release channel

GHCR is the active deployment channel. The `Publish RyeOS release artifacts`
workflow builds from the immutable tagged repository state and runs
`scripts/populate-bundles.sh` inside the artifact/image builders with the
publisher key secret. Generated bundle binaries, CAS/refs/manifests, and trust
docs therefore come from the release workflow rather than the raw GitHub source
archive.

After pushing the tag, verify that the workflow succeeds and that all immutable
release outputs exist:

```text
GitHub release assets:
  ryeos-bundles-$new-x86_64.tar.gz
  ryeos-bundles-$new-x86_64.tar.gz.sha256

GHCR image tags:
  ghcr.io/leolilley/ryeos-standard:$new
  ghcr.io/leolilley/ryeos-central-host:$new
```

The workflow qualifies the exact image digests, checks provenance and SBOM
attestations, verifies keyless signatures, promotes the immutable version tags,
and only then advances both `latest` tags. Verify the immutable tags, release
assets, and successful workflow run; do not use the mutable tags as the release
identity.

## 9. AUR is deferred, not the active release channel

AUR is not currently used for shipping RyeOS releases. Do not update AUR as part
of the standard release flow.

The checked-in AUR PKGBUILDs are not sufficient for an official release as-is
because they source GitHub's raw tag archive. Raw tag archives omit ignored,
generated bundle artifacts produced by `scripts/populate-bundles.sh`, including
bundle-owned binaries, CAS objects/refs, populated manifests, and trust docs.

Before any future AUR publication, create or automate an official populated
release tarball, point `source=...` at that artifact instead of the raw tag
archive, replace `sha256sums=('SKIP')` with a real checksum, and validate with a
clean `makepkg` build/install/init.

## 10. Local packaged-layout install validation

`scripts/pkg/install-local-direct.sh` is for fast local repair/testing. It
intentionally bypasses the package manager/AUR flow while installing the same
runtime layout:

- binaries to `/usr/bin`;
- selected bundle sources to `/usr/share/ryeos/<bundle>` (default: the full set
  defined by `scripts/pkg/bundle-sets.sh`);
- initialized bundles under `~/.local/share/ryeos/.ai/bundles/...` after
  `ryeos init`.

Default behavior:

```bash
./scripts/pkg/install-local-direct.sh --trust-source-publishers
```

For an existing stopped development node when the release deliberately changes
the complete node-policy schema or selected bundle-set profile, perform the
explicit one-time cut in the same install:

```bash
./scripts/pkg/install-local-direct.sh \
  --trust-source-publishers --reset-node-policy-generation
```

Do not pass that flag for a fresh node. It replaces only the signed policy
generation; the following init aligns the trusted profile's prospective exact
bundle inventory and preserves all non-policy state.

The script will:

1. reuse already-built binaries and populated bundle sources by default;
2. stop an already-running daemon using `ryeos node status`;
3. install `ryeos` and `ryeosd` into `/usr/bin`;
4. optionally install `lillux` if it was built;
5. install the selected bundle sources under `/usr/share/ryeos`;
6. move stale PATH shadows of installed user-facing binaries from
   `/usr/local/bin`, `~/.local/bin`, and the invoking user's configured Cargo
   home `bin` directory, preserving the user-local entries in timestamped
   backups;
7. run `ryeos init --source /usr/share/ryeos ...`;
8. restart the daemon only if it was running before the install.

Bundle rebuilding/republishing is opt-in and expensive:

```bash
./scripts/pkg/install-local-direct.sh \
  --populate --all --trust-source-publishers
```

Use `--populate --crates "<Cargo package ...>"` for an explicit development
subset. The selected packages are rebuilt; unselected bundle payloads retain
their exact existing artifact generations. The publisher key defaults to
`.dev-keys/PUBLISHER_DEV.pem` only when population is requested.

Important caveats:

- `install-local-direct.sh` may print `complete` without starting a daemon if no
  daemon was running before the install.
- Always check runtime state explicitly:

  ```bash
  ryeos node status
  ```

- If needed, start manually:

  ```bash
  ryeos start
  ryeos node status
  ```

- Do not use `ryeos status` as the daemon status check. Use:

  ```bash
  ryeos node status
  ```

- The default deliberately installs the checkout's exact closed bundle
  generation without comparing it to mutable source mtimes. Pass
  `--populate --crates "<Cargo package ...>"` for focused regeneration;
  release/E2E qualification uses `--populate --all`.
- `--no-init` leaves initialized user state unchanged.
- `--no-daemon-restart` leaves any daemon restart to you.
- `--bundle-set hosted-node` intentionally installs only `core`,
  `central-auth`, and `hosted-node`; do not use it for full local release
  validation unless testing that lean layout.

## 11. Bundle signing implications

For bundle source changes or Rust changes that affect bundled binaries,
refresh/sign bundles as bundles:

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev \
  --all
```

This builds release binaries, stages bundle bin trees, signs signable bundle
items, rebuilds CAS manifests, and emits trust documents. `--all` is required
(or `--crates "<Cargo package ...>"` for a development subset) — populate
refuses an implicit full build.

Do not:

- hand-edit signed bundle YAML and keep the old signature header;
- manually copy `target/release/*` into `bundles/*/.ai/bin/<triple>/`;
- add trust bypasses or raw YAML fallbacks to work around signing failures;
- commit private or newly-generated keys.

For project knowledge docs under `.ai/knowledge/...`, use project item signing
if required by the workflow:

```bash
ryeos sign knowledge:ryeos/development/release-process
```

Do not confuse project item signing with bundle signing.

## 12. Daemon startup and state-cutover validation

A release can pass `cargo check`, rebuild projection data, and still fail at
daemon startup. Validate startup explicitly.

Useful commands:

```bash
ryeos node status
ryeos start
ryeos node status
```

If startup is slow after a projection schema/epoch change, it may be doing a
healthy one-time rebuild from CAS/refs. Do not kill it just because it is taking
longer than a normal start.

An immutable authoritative execution schema mismatch is different: the current
release intentionally has no predecessor reader, and installation never
discards history automatically. Startup reports the explicit offline cutover.
Inspect it before confirming:

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

For an incompatible runtime schema, the dry-run marks runtime-row counts
unavailable rather than decoding rows or reporting a false zero; its other
retirement counts remain available for the decision. The confirmed report
retains that unavailable classification instead of reporting the empty
post-reset schema as an exact zero.

This retires the prior local execution-history/project-HEAD epoch while
preserving worktrees, bundles, vault values, trust, and node/signing identity.
Do not add compatibility readers or make the installer perform this destructive
decision implicitly.

Current startup/restart logic allows a long rebuild window.
`install-local-direct.sh` gives restart roughly 930 seconds so `ryeos start` can
report its own diagnostics.

Distinguish these cases:

| Symptom | Interpretation |
|---|---|
| Long rebuild, then `ryeos node status` says `running` | Successful startup |
| Exact-current-contract schema mismatch with cutover command | Explicit history retirement decision required |
| `ryeos start` exits early | Startup failure |
| No readiness after timeout | Investigate as failure |
| Cargo/tests pass but daemon will not start | Runtime/startup bug remains |

Check daemon startup stderr log when startup fails:

```bash
cat ~/.local/share/ryeos/.ai/state/ryeosd-start.stderr.log
```

or inspect its tail:

```bash
tail -200 ~/.local/share/ryeos/.ai/state/ryeosd-start.stderr.log
```

## 13. Final release checklist

Before tagging:

- [ ] Fix committed on `next`.
- [ ] Version bumped to the new release in:
  - [ ] `crates/kernel/lillux/Cargo.toml`
  - [ ] `crates/kernel/lillux/pyproject.toml`
  - [ ] `crates/engine/ryeos-runtime/Cargo.toml`
  - [ ] `crates/tools/core-tools/Cargo.toml`
  - [ ] `crates/bin/cli/Cargo.toml`
  - [ ] `crates/bin/daemon/Cargo.toml`
  - [ ] `Cargo.lock`
- [ ] `rg "$old" <release-version-files> Cargo.lock` has no unintended
  matches.
- [ ] `cargo check -p ryeos-node -p ryeos-cli -p ryeosd` passes.
- [ ] `cargo test -p ryeos-node` passes.
- [ ] `cargo test -p ryeos-state` passes.
- [ ] `bash -n scripts/pkg/install-local-direct.sh` passes.
- [ ] Bundle signing/population done if bundle contents or bundled binaries
  changed.
- [ ] `git status --short` does not include unrelated untracked files.
- [ ] `next` merged into `main` from `/home/leo/projects/ryeos`.
- [ ] Merge conflicts, if any, resolved to the new release version.
- [ ] Annotated tag created on the `main` release commit.
- [ ] Branches pushed before tag:
  - [ ] `git push origin next main`
  - [ ] `git push origin vX.Y.Z`

After local install validation:

- [ ] The applicable local install completes: a fresh/current-policy node uses
      `./scripts/pkg/install-local-direct.sh --trust-source-publishers`; an
      existing predecessor-policy node adds `--reset-node-policy-generation`.
- [ ] `ryeos node status` checked explicitly.
- [ ] If daemon was not running before install, `ryeos start` run manually if
  startup validation is needed.
- [ ] Long projection rebuild distinguished from actual startup failure.
- [ ] Startup stderr log checked if daemon fails to become ready.

## Common pitfalls from v0.5.4/v0.5.5

1. **Forgotten package versions**

   The release tag was not enough. All package version strings and `Cargo.lock`
   must reflect the new patch release.

2. **Wrong worktree checkout**

   Trying to check out `main` in `/home/leo/projects/ryeos-next` fails when
   `main` is already checked out in `/home/leo/projects/ryeos`. Merge from the
   existing `main` worktree instead.

3. **Local install status confusion**

   Use `ryeos node status`, not `ryeos status`. Also, `install-local-direct.sh`
   restarts only a daemon that was running before install; it does not always
   mean the daemon is now running.

4. **Projection rebuild mistaken for failure**

   A long first startup after schema/epoch changes can be a valid projection
   rebuild. Confirm with `ryeos node status` and startup logs before declaring
   failure.

5. **Successful rebuild mistaken for successful startup**

   Cargo checks, tests, and a long projection rebuild do not prove the daemon
   starts. Always validate daemon readiness separately.
