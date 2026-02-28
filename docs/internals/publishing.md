```yaml
id: publishing
title: "Publishing and Deployment"
description: How to publish Rye OS packages to PyPI, manage trusted publishers, bump versions, and deploy services
category: internals
tags:
  [publishing, pypi, deployment, github-actions, versioning, trusted-publisher]
version: "1.0.0"
```

# Publishing and Deployment

This page covers the full publishing pipeline: how packages are built and published to PyPI via GitHub Actions, how trusted publishers are configured, how to bump versions, and how to deploy services.

## PyPI Publishing via GitHub Actions

All packages are published to PyPI using **trusted publishing** (OIDC) — no API tokens or secrets needed. The workflow lives at `.github/workflows/publish.yml`.

### Triggers

The workflow runs on:

1. **Tag push** — pushing a tag matching `v*` publishes all packages
2. **Manual dispatch** — select a specific package (or "all") from the Actions UI

```bash
# Publish all packages
git tag v0.1.2 -m "v0.1.2"
git push origin v0.1.2

# Or manually via GitHub Actions UI:
# Actions → Publish to PyPI → Run workflow → select package
```

### How It Works

The workflow has two jobs:

**`publish-python`** — builds and publishes Python packages (hatchling):

| Package      | Build path               |
| ------------ | ------------------------ |
| lillux       | `lillux/kernel`          |
| ryeos-engine | `ryeos`                  |
| ryeos-core   | `ryeos/bundles/core`     |
| ryeos        | `ryeos/bundles/standard` |
| ryeos-web    | `ryeos/bundles/web`      |
| ryeos-code   | `ryeos/bundles/code`     |
| ryeos-mcp    | `ryeos-mcp`              |
| ryeos-cli    | `ryeos-cli`              |

**`publish-rust`** — builds and publishes Rust binaries (maturin):

| Package      | Build path     |
| ------------ | -------------- |
| lillux-proc  | `lillux/proc`  |
| lillux-watch | `lillux/watch` |

Both jobs use `max-parallel: 1` because PyPI's OIDC pending publisher system can only match one publisher per token at a time.

Each job uses the `pypi` GitHub environment and `id-token: write` permission for OIDC authentication.

### Selective Publishing

The workflow uses a matrix with a check step that skips packages not matching the selected input:

```bash
# Publish only ryeos-engine via GitHub Actions UI
# Actions → Publish to PyPI → Run workflow → select "ryeos-engine"
```

On tag pushes, all packages run. If a version already exists on PyPI, that individual job fails with "File already exists" — this is harmless and expected.

## Trusted Publishers (OIDC)

PyPI trusted publishing uses GitHub's OIDC identity to authenticate — no stored secrets.

### Setting Up a New Package

For a package that **doesn't exist on PyPI yet**, register a "pending publisher" at the account level:

1. Go to [pypi.org/manage/account/publishing/](https://pypi.org/manage/account/publishing/)
2. Add a pending publisher with these values:

| Field             | Value                |
| ----------------- | -------------------- |
| PyPI Project Name | e.g., `ryeos-engine` |
| Owner             | `leolilley`          |
| Repository Name   | `ryeos`              |
| Workflow Name     | `publish.yml`        |
| Environment Name  | `pypi`               |

3. Run the workflow — the first successful publish converts the "pending" publisher to an "active" publisher

### Managing Existing Publishers

Once a package is published, manage its publisher at:
`https://pypi.org/manage/project/<package-name>/settings/publishing/`

### GitHub Environment

The repository needs a `pypi` environment configured:
**Settings** → **Environments** → **New environment** → name it `pypi`

No secrets or protection rules are needed — OIDC handles authentication.

### Current Active Publishers

| Package      | Status |
| ------------ | ------ |
| lillux       | Active |
| lillux-proc  | Active |
| lillux-watch | Active |
| ryeos-engine | Active |
| ryeos-core   | Active |
| ryeos        | Active |
| ryeos-web    | Active |
| ryeos-code   | Active |
| ryeos-mcp    | Active |
| ryeos-cli    | Pending — register at pypi.org/manage/account/publishing/ |

## Version Bumping

All packages should be bumped together to keep versions in sync. Version numbers live in:

### Python packages (pyproject.toml)

```
ryeos/pyproject.toml                          → ryeos-engine
ryeos/bundles/core/pyproject.toml             → ryeos-core
ryeos/bundles/standard/pyproject.toml         → ryeos
ryeos/bundles/web/pyproject.toml              → ryeos-web
ryeos/bundles/code/pyproject.toml             → ryeos-code
ryeos-mcp/pyproject.toml                      → ryeos-mcp
ryeos-cli/pyproject.toml                      → ryeos-cli
lillux/kernel/pyproject.toml                  → lillux
```

### Rust packages (pyproject.toml AND Cargo.toml)

Rust packages have **two** version fields that must be kept in sync:

```
lillux/proc/pyproject.toml                    → lillux-proc (maturin uses this for wheel version)
lillux/proc/Cargo.toml                        → lillux-proc (Rust binary version)
lillux/watch/pyproject.toml                   → lillux-watch (maturin uses this for wheel version)
lillux/watch/Cargo.toml                       → lillux-watch (Rust binary version)
```

> **Important:** Maturin reads the version from `pyproject.toml`, not `Cargo.toml`. If you only bump `Cargo.toml`, the wheel will still have the old version.

### Bump workflow

```bash
# 1. Bump all versions (example: 0.1.1 → 0.1.2)
#    Update all pyproject.toml, Cargo.toml, and bundle.py files

# 2. Commit and push
git add -A
git commit -m "bump all packages to 0.1.2"
git push origin main

# 3. Tag and push
git tag v0.1.2 -m "v0.1.2"
git push origin v0.1.2

# 4. Monitor the workflow at:
#    https://github.com/leolilley/ryeos/actions/workflows/publish.yml
```

### PyPI won't let you overwrite versions

If a version already exists on PyPI, the upload fails with "File already exists." You must either:

- Bump to a new version number, or
- Yank the old version on PyPI and re-upload (not recommended for published releases)

### Retagging

If a tag was pushed before all changes were ready:

```bash
# Delete old tag locally and remotely
git tag -d v0.1.2
git push origin :refs/tags/v0.1.2

# Create new tag on current HEAD and push
git tag v0.1.2 -m "v0.1.2"
git push origin v0.1.2
```

## Publishing Order

Packages must be published in dependency order. The workflow uses `max-parallel: 1` and matrix ordering to handle this, but be aware of the layers:

```
Layer 1 — lillux-proc, lillux-watch       (standalone Rust, no deps)
Layer 2 — lillux                          (depends on lillux-proc)
Layer 3 — ryeos-engine                    (depends on lillux)
Layer 4 — ryeos-core                      (depends on ryeos-engine)
Layer 5 — ryeos                           (depends on ryeos-core)
Layer 6 — ryeos-web, ryeos-code, ryeos-mcp, ryeos-cli (depend on ryeos)
```

For a **first-time publish** of all packages, you may need to run the workflow multiple times — later layers will fail if their dependencies haven't been uploaded yet. Subsequent releases (where deps already exist on PyPI) publish cleanly in a single run.

## Services

### registry-api

The registry API is a standalone FastAPI service deployed to Railway.

- **Source:** `services/registry-api/`
- **Not a pip package** — deployed as a container
- **Dependencies:** fastapi, supabase, httpx, python-jose, pydantic, pydantic-settings

Deployment is managed separately from the PyPI publishing workflow.

## Troubleshooting

### "File already exists" error

The version is already on PyPI. Bump the version number and retag.

### "pending publisher" not matching

PyPI matches exactly on: owner, repo name, workflow filename, and environment name. Double-check all four fields. The environment must be `pypi` (lowercase).

### Rust package builds fail

Maturin requires Rust toolchain. The workflow uses `PyO3/maturin-action@v1` which handles this. If builds fail, check that `Cargo.toml` version matches what you expect.

### OIDC token errors

Ensure the job has `permissions: id-token: write` and uses `environment: pypi`. Both are required for OIDC.

### Only one package needs publishing

Use the manual dispatch: Actions → Publish to PyPI → Run workflow → select the specific package.
