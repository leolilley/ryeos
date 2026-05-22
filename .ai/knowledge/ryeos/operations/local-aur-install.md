---
category: "ryeos/operations"
name: "local-aur-install"
description: "Safe local yay/AUR package install workflow for testing ryeOS from a checkout"
---

# Local AUR Install Workflow

Goal: establish a repeatable, safe way to test a **fresh package install**
from the local checkout with `yay`, without allowing `yay`/`makepkg` to
clean, reset, or otherwise mutate the live repository.

## Problem to avoid

Do **not** run `yay -Bii .` from the repository root, from
`deploy/aur/ryeos`, or from any directory containing important tracked
files. `yay` clean-build behavior can delete/reset the directory it is
building from. The safe flow must give `yay` a disposable package build
directory and an immutable source artifact.

## Workflow shape

```text
repo checkout
   │
   ├─ scripts/populate-bundles.sh
   ▼
signed bundles in bundles/{core,standard}
   │
   ├─ scripts/pkg/prepare-local-aur-source.sh
   ▼
dist/aur/
   ├─ ryeos-<version>+local.<shortsha>.tar.gz
   └─ pkgbuild/
      ├─ PKGBUILD
      ├─ .SRCINFO
      └─ ryeos.install
   │
   ├─ yay -Bi --noconfirm dist/aur/pkgbuild
   ▼
installed package
   ├─ /usr/bin/ryeos
   ├─ /usr/bin/ryeosd
   └─ /usr/share/ryeos/{core,standard}/.ai/
   │
   ├─ ryeos init
   ├─ ryeos start
   └─ ryeos status
```

## Implemented workflow

### 1. Prepare the local source artifact

Run:

```bash
./scripts/pkg/prepare-local-aur-source.sh --allow-dirty
```

The script:

1. Run from repo root or resolve repo root safely.
2. Refuse to write into the repo root, `deploy/aur/ryeos`, or any other
   non-disposable directory.
3. Optionally require a clean worktree; allow dirty state only with
   `--allow-dirty` because local package tests often need uncommitted
   bundle/doc changes.
4. Ensure `dist/aur/` exists and is disposable.
5. Create a source tarball from the checkout, excluding at least:
   - `.git/`
   - `.jj/`
   - `target/`
   - `dist/`
   - `.cache/`
   - any local daemon/user state
6. Name the source tarball with version + local commit identity, e.g.:

   ```text
   dist/aur/ryeos-0.4.9+local.e8c650fa.tar.gz
   ```

7. Copy/synthesize a PKGBUILD package directory at:

   ```text
   dist/aur/pkgbuild/
   ```

8. Rewrite/source the PKGBUILD to consume the tarball by `file://` and
   include the actual sha256:

   ```bash
   source=("ryeos-${pkgver}.tar.gz::file:///abs/path/to/dist/aur/ryeos-...")
   sha256sums=("<computed sha256>")
   ```

9. Generate `.SRCINFO` with `makepkg --printsrcinfo` inside
   `dist/aur/pkgbuild`.
10. Print the exact install command:

    ```bash
    yay -Bi --noconfirm dist/aur/pkgbuild
    ```

### 2. Keep production AUR metadata separate

The checked-in `deploy/aur/ryeos/PKGBUILD` can continue representing the
real package source (eventually a release tag or commit). The local
workflow should not require editing that file every time a local checkout
is tested.

If shared code is desired, the script can template from
`deploy/aur/ryeos/PKGBUILD`, but the generated PKGBUILD in
`dist/aur/pkgbuild` should be concrete and self-contained for the local
build.

### 3. Package dependencies

The package must not list `cc` as a makedep. On Arch, use `gcc` for the
C compiler required by bundled `libsqlite3-sys` builds:

```bash
makedepends=('rust' 'cargo' 'gcc')
```

Add `git` only for git-sourced production PKGBUILDs. It is not needed for
the generated local tarball PKGBUILD.

### 4. Use package installation only from disposable directory

Canonical command after script generation:

```bash
yay -Bi --noconfirm dist/aur/pkgbuild
```

or:

```bash
cd dist/aur/pkgbuild
yay -Bi --noconfirm .
```

Never invoke `yay -Bii .` in the repo or in `deploy/aur/ryeos`.

## Fresh local install test flow

From repo root:

```bash
# 1. Ensure bundle signatures/manifests match current content.
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev

# 2. Prepare disposable local AUR package source/build directory.
./scripts/pkg/prepare-local-aur-source.sh --allow-dirty

# 3. Install through yay from the generated package directory.
yay -Bi --noconfirm dist/aur/pkgbuild

# 4. Fresh local node state.
ryeos stop --force || true
rm -rf ~/.local/share/ryeos ~/.ryeos

# 5. Exercise packaged lifecycle.
ryeos init
ryeos start
ryeos status
```

Expected package layout after install:

```text
/usr/bin/ryeos
/usr/bin/ryeosd
/usr/bin/ryeos-core-tools
/usr/bin/ryeos-directive-runtime
/usr/bin/ryeos-graph-runtime
/usr/bin/ryeos-knowledge-runtime
/usr/share/ryeos/core/.ai/
/usr/share/ryeos/standard/.ai/
```

Expected install hook output:

```text
ryeos bundles installed to /usr/share/ryeos
Initialize with: ryeos init
```

## Guardrails in the script

The script fails fast if:

- It is about to use the repo root as the yay build directory.
- It is about to use `deploy/aur/ryeos` as the yay build directory.
- The tarball would include `.git`, `.jj`, `target`, or `dist`.
- `dist/aur/pkgbuild` cannot be removed/recreated safely.
- The generated package source tarball does not contain
  `bundles/core/.ai` and `bundles/standard/.ai`.
- The generated package source tarball does not contain `Cargo.lock`.

The script prints what it generated and the exact next command.
