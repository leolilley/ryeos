<!-- ryeos:signed:2026-07-13T07:43:47Z:5e0ada05163877634ccf78c498ff63d2f670f6a67892021584767829e8c7c5d6:wL2JRvXdbAtfWe+20+WHv05qs+RuEc4/kh3Ex3+SK0GEkSjbx+9G1rYpapPeCt3TW5FShHVsujunLLUZC2x+Bw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: "ryeos/development"
name: "release-process"
title: "Release Process"
description: "Checklist for cutting RyeOS releases from next to main without stale versions, tags, or install validation mistakes"
entry_type: reference
version: "1.0.0"
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
`--crates "<crate ...>"` to rebuild only what changed (e.g. `--crates ryeos-core-tools`
for core-tools). `--jobs N` caps cargo parallelism if a full release build
exhausts memory. The release Dockerfiles already pass `--all`.

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

## 8. GHCR release channel

GHCR is the active deployment channel. The Docker release workflow builds from
the tagged repository state and runs `scripts/populate-bundles.sh` inside the
image builder with the publisher key secret, so generated bundle binaries,
CAS/refs/manifests, and trust docs are produced during image build rather than
coming from the raw GitHub source archive.

After pushing the tag, verify the GHCR workflow/image for the release tag:

```bash
# Check the GitHub Actions workflow for the tag in the UI or with gh if available.
# Expected image tag:
#   ghcr.io/leolilley/ryeosd-full:v$new
```

If a hosted-node image is part of a specific release, verify the workflow or
manual publish step for `Dockerfile.hosted-node` separately. The default release
workflow currently covers the full daemon image path.

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
- bundle sources to `/usr/share/ryeos/{core,standard,ryeos-ui,web,hosted-node}`;
- initialized bundles under `~/.local/share/ryeos/.ai/bundles/...` after
  `ryeos init`.

Default behavior:

```bash
./scripts/pkg/install-local-direct.sh --trust-source-publishers
```

The script will:

1. populate/sign bundles unless `--skip-populate` is used;
2. use `.dev-keys/PUBLISHER_DEV.pem` by default;
3. stop an already-running daemon using `ryeos node status`;
4. install `ryeos` and `ryeosd` into `/usr/bin`;
5. optionally install `lillux` if it was built;
6. install bundle sources under `/usr/share/ryeos`;
7. move stale PATH shadows from `/usr/local/bin` and `~/.local/bin`;
8. run `ryeos init --source /usr/share/ryeos ...`;
9. restart the daemon only if it was running before the install.

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

- `--skip-populate` can preserve stale bundle binaries/signatures. Use it only
  when intentionally reusing already-populated bundles.
- `--no-init` leaves initialized user state unchanged.
- `--no-daemon-restart` leaves any daemon restart to you.
- `--bundle-set hosted-node` intentionally installs only `core` and
  `hosted-node`; do not use it for full local release validation unless testing
  that lean layout.

## 9. Bundle signing implications

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
(or `--crates "<crate ...>"` to rebuild a subset) — populate refuses an implicit
full build.

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

## 10. Daemon startup and projection rebuild validation

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

Current startup/restart logic allows a long rebuild window.
`install-local-direct.sh` gives restart roughly 930 seconds so `ryeos start` can
report its own diagnostics.

Distinguish these cases:

| Symptom | Interpretation |
|---|---|
| Long rebuild, then `ryeos node status` says `running` | Successful startup |
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

## 11. Final release checklist

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

- [ ] `./scripts/pkg/install-local-direct.sh --trust-source-publishers` completes.
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
