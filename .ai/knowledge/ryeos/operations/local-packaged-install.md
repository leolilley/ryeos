<!-- rye:signed:2026-05-24T09:21:50Z:80acce8752243c3aa307cfe6ccba66f95be343a754f155b3c2d7d8b2228a8b52:-lxy2I6J0lTBzRrZiYOOVACF8v7AemK8MZBTiLt1Pu9cbt1-oLWdokc2q_3zzu-JcV9ZFaghqyBRD8t-v3RSDw:4b987fd4e40303ac -->
```yaml
category: ryeos/operations
name: local-packaged-install
title: Local Packaged Install Layout and Recovery
entry_type: reference
version: "1.0.0"
author: amp
created_at: 2026-05-24T00:00:00Z
description: "Reference for the packaged local install flow: binaries on PATH, bundle sources in /usr/share/ryeos, and ryeos init copying into ~/.local/share/ryeos."
tags:
  - install
  - packaged-layout
  - bundles
  - init
  - path
  - local-share
```

# Local Packaged Install Layout and Recovery

This is the reference flow for testing RyeOS as a locally installed package while still building from this checkout.

The key distinction:

- **Package install location:** immutable-ish system files under `/usr/bin` and `/usr/share/ryeos`.
- **Runtime/init location:** mutable per-user node state under `~/.local/share/ryeos`.

So yes, in the packaged flow bundles appear in both places by design:

```text
repo bundles/{core,standard}
        │
        │ populate-bundles.sh signs + manifests
        ▼
/usr/share/ryeos/{core,standard}/.ai       package bundle source
        │
        │ ryeos init
        ▼
~/.local/share/ryeos/.ai/bundles/{core,standard}/.ai
                                            installed local runtime copy
```

That is not an accidental double install. `/usr/share/ryeos` is the packaged source tree; `~/.local/share/ryeos` is the initialized node/system space used by the daemon and CLI.

## Intended installed layout

User-facing binaries should resolve from the package path:

```text
/usr/bin/ryeos
/usr/bin/ryeosd
/usr/bin/ryeos-core-tools
/usr/bin/ryeos-directive-runtime
/usr/bin/ryeos-graph-runtime
/usr/bin/ryeos-knowledge-runtime
/usr/bin/ryeos-tui
/usr/bin/rye-parser-yaml-document
/usr/bin/rye-parser-yaml-header-document
/usr/bin/rye-parser-regex-kv
/usr/bin/rye-composer-extends-chain
/usr/bin/rye-composer-graph-permissions
/usr/bin/rye-composer-identity
```

Packaged bundle sources should live here:

```text
/usr/share/ryeos/core/.ai/...
/usr/share/ryeos/core/PUBLISHER_TRUST.toml
/usr/share/ryeos/standard/.ai/...
/usr/share/ryeos/standard/PUBLISHER_TRUST.toml
```

After `ryeos init`, local initialized bundles and registrations should exist here:

```text
~/.local/share/ryeos/.ai/bundles/core/.ai/...
~/.local/share/ryeos/.ai/bundles/standard/.ai/...
~/.local/share/ryeos/.ai/node/bundles/core.yaml
~/.local/share/ryeos/.ai/node/bundles/standard.yaml
```

## Canonical local package flow

Use this when you want to exercise the same layout as an installed package:

```bash
cd /home/leo/projects/ryeos-next

./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev

./scripts/pkg/prepare-local-aur-source.sh --allow-dirty

yay -Bi --noconfirm dist/aur/pkgbuild

ryeos init \
  --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml \
  --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml
```

Why the explicit trust files? Local checkout bundles are dev-signed with `.dev-keys/PUBLISHER_DEV.pem`, not the official release publisher key compiled into `ryeos init`.

## Fast direct-copy flow

Use this only for quick local repair/testing when you intentionally do not want to wait for `yay`/`makepkg`. It mirrors the package payload layout but is not pacman-owned.

The checked-in helper is:

```bash
./scripts/pkg/install-local-direct.sh
```

Default behavior is complete and safe:

1. Run `scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev`.
2. Install release binaries into `/usr/bin`.
3. Install bundle sources into `/usr/share/ryeos/{core,standard}`.
4. Move stale RyeOS shadows from `/usr/local/bin` and `~/.local/bin` aside.
5. Verify `ryeos` resolves to `/usr/bin/ryeos`.
6. Run `ryeos init` from PATH with installed `PUBLISHER_TRUST.toml` files.

Useful options:

```bash
# Reuse already-populated bundles and already-built release binaries.
./scripts/pkg/install-local-direct.sh --skip-populate

# Install files but do not run init.
./scripts/pkg/install-local-direct.sh --no-init

# Do not move /usr/local/bin or ~/.local/bin shadows aside.
./scripts/pkg/install-local-direct.sh --keep-shadows
```

The rest of this section shows what the helper does internally, for debugging or manual recovery.

First ensure bundle contents and manifests are current:

```bash
./scripts/populate-bundles.sh \
  --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev
```

Then copy already-built release binaries into `/usr/bin`:

```bash
sudo install -Dm755 target/release/ryeos /usr/bin/ryeos
sudo install -Dm755 target/release/ryeosd /usr/bin/ryeosd
sudo install -Dm755 target/release/ryeos-core-tools /usr/bin/ryeos-core-tools
sudo install -Dm755 target/release/ryeos-tui /usr/bin/ryeos-tui

sudo install -Dm755 target/release/ryeos-directive-runtime /usr/bin/ryeos-directive-runtime
sudo install -Dm755 target/release/ryeos-graph-runtime /usr/bin/ryeos-graph-runtime
sudo install -Dm755 target/release/ryeos-knowledge-runtime /usr/bin/ryeos-knowledge-runtime

sudo install -Dm755 target/release/rye-parser-yaml-document /usr/bin/rye-parser-yaml-document
sudo install -Dm755 target/release/rye-parser-yaml-header-document /usr/bin/rye-parser-yaml-header-document
sudo install -Dm755 target/release/rye-parser-regex-kv /usr/bin/rye-parser-regex-kv
sudo install -Dm755 target/release/rye-composer-extends-chain /usr/bin/rye-composer-extends-chain
sudo install -Dm755 target/release/rye-composer-graph-permissions /usr/bin/rye-composer-graph-permissions
sudo install -Dm755 target/release/rye-composer-identity /usr/bin/rye-composer-identity
```

If `target/release/lillux` exists and the package recipe expects it, install it too:

```bash
if [ -x target/release/lillux ]; then
  sudo install -Dm755 target/release/lillux /usr/bin/lillux
fi
```

Then copy bundle sources to `/usr/share/ryeos`:

```bash
sudo mkdir -p /usr/share/ryeos
sudo rm -rf /usr/share/ryeos/core /usr/share/ryeos/standard

for bundle_dir in bundles/*/; do
  name="$(basename "$bundle_dir")"
  sudo mkdir -p "/usr/share/ryeos/$name"
  sudo cp -a "$bundle_dir/.ai" "/usr/share/ryeos/$name/.ai"
  if [ -f "$bundle_dir/PUBLISHER_TRUST.toml" ]; then
    sudo install -Dm644 "$bundle_dir/PUBLISHER_TRUST.toml" \
      "/usr/share/ryeos/$name/PUBLISHER_TRUST.toml"
  fi
done

sudo chown -R root:root /usr/share/ryeos/core /usr/share/ryeos/standard
```

Finally run init through `ryeos` on PATH:

```bash
hash -r
ryeos init \
  --trust-file /usr/share/ryeos/core/PUBLISHER_TRUST.toml \
  --trust-file /usr/share/ryeos/standard/PUBLISHER_TRUST.toml
```

## PATH shadow cleanup

The packaged flow expects `ryeos` to resolve to `/usr/bin/ryeos`. Stale local copies can shadow or duplicate the package install.

Check all candidates:

```bash
type -a ryeos
command -v ryeos
```

Expected:

```text
ryeos is /usr/bin/ryeos
```

If `/usr/local/bin` or `~/.local/bin` copies exist, move them aside rather than adding another install location:

```bash
stamp="$(date +%Y%m%d%H%M%S)"

for b in \
  ryeos ryeosd lillux ryeos-core-tools ryeos-tui \
  ryeos-directive-runtime ryeos-graph-runtime ryeos-knowledge-runtime \
  rye-parser-yaml-document rye-parser-yaml-header-document rye-parser-regex-kv \
  rye-composer-extends-chain rye-composer-graph-permissions rye-composer-identity
do
  if [ -e "/usr/local/bin/$b" ]; then
    sudo mv "/usr/local/bin/$b" "/usr/local/bin/$b.bak.$stamp"
  fi
  if [ -e "$HOME/.local/bin/$b" ]; then
    mkdir -p "$HOME/.local/bin/ryeos-shadow-backups-$stamp"
    mv "$HOME/.local/bin/$b" "$HOME/.local/bin/ryeos-shadow-backups-$stamp/$b"
  fi
done

hash -r
type -a ryeos
```

Do not switch this packaged flow to `~/.local/bin`. That is a different install scheme and conflicts with the current package recipe and default init source.

## Verification checklist

Run these after install/init:

```bash
command -v ryeos
type -a ryeos

test -x /usr/bin/ryeos
test -x /usr/bin/ryeosd
test -x /usr/bin/ryeos-core-tools

test -d /usr/share/ryeos/core/.ai
test -d /usr/share/ryeos/standard/.ai
test -f /usr/share/ryeos/core/PUBLISHER_TRUST.toml
test -f /usr/share/ryeos/standard/PUBLISHER_TRUST.toml

test -d "$HOME/.local/share/ryeos/.ai/bundles/core/.ai"
test -d "$HOME/.local/share/ryeos/.ai/bundles/standard/.ai"
test -f "$HOME/.local/share/ryeos/.ai/node/bundles/core.yaml"
test -f "$HOME/.local/share/ryeos/.ai/node/bundles/standard.yaml"
```

Expected `command -v ryeos`:

```text
/usr/bin/ryeos
```

Expected init output includes:

```json
"bundles_installed": [
  "core",
  "standard"
]
```

## Common mistakes

### Mistake: thinking `/usr/share/ryeos` and `~/.local/share/ryeos` are duplicate installs

They are different layers. `/usr/share/ryeos` is the packaged source. `~/.local/share/ryeos` is the initialized local system space.

### Mistake: putting the packaged CLI in `~/.local/bin`

For this flow, the CLI belongs in `/usr/bin`. If `~/.local/bin/ryeos` exists, it is a stale or alternate install and should not shadow the packaged binary.

### Mistake: installing only binaries but not bundle sources

Plain `ryeos init` defaults to `/usr/share/ryeos`. If `/usr/share/ryeos/core/.ai` and `/usr/share/ryeos/standard/.ai` are missing or stale, init will fail or install stale bundles.

### Mistake: copying one binary into a bundle

Do not repair bundle drift by manually copying `target/release/ryeos-core-tools` into `bundles/core/.ai/bin/...`. Run `scripts/populate-bundles.sh` so binaries, CAS, manifests, signatures, and trust docs are regenerated together.

### Mistake: using dev-signed bundle sources without trust files

Official packaged releases should work with plain `ryeos init`. Local checkout installs use the dev publisher key, so pass the two `/usr/share/ryeos/.../PUBLISHER_TRUST.toml` files when initializing.
