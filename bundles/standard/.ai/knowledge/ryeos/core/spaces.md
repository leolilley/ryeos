<!-- ryeos:signed:2026-07-14T01:54:46Z:87023d458c72212e35fdafe38798163f834498c660293db553d6461d5e733bb4:m/+J3qmOcIy8+Fejq/qrwwQ4EHyq/pI41gZULFdSJ1sbjNuRlsPydjrZUwxcZm3+iLLWpFZ9UcvNrF+iCA9wDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, spaces, resolution, bundles]
version: "2.1.0"
description: >
  The two-tier space resolution system — project and system (bundle)
  spaces, how bundles are installed, and how items are found.
---

# Spaces

Rye OS resolves items across two tiers: **project** and **system**.
The first match wins.

## Project Space

```
<project_root>/.ai/
```

Project-specific items. This is where you put directives, tools, and
knowledge unique to a project. When the CLI runs from a project
directory, the project space is checked first.

Examples:
- `.ai/directives/deploy.md` — project deploy workflow
- `.ai/tools/my-helper.sh` — project-specific tool
- `.ai/knowledge/project/architecture.md` — project context for agents

Project space items are signed by the operator's local key. They are
typically tracked in git (or at least `.ai/knowledge/` is).

## System Space

```
<system_space_dir>/.ai/
```

The daemon's runtime state, operator keys/trust, and installed bundles.
Defaults to the XDG data directory (`~/.local/share/ryeos/` on Linux),
overridable via `RYEOS_APP_ROOT`. Created by `ryeos init`.

```
<system_space_dir>/.ai/
├── config/keys/
│   ├── signing/private_key.pem      ← operator's Ed25519 identity
│   └── trusted/
│       ├── <publisher-fp>.toml      ← trusted publisher keys
│       ├── <operator-fp>.toml       ← operator self-trust
│       └── <node-fp>.toml           ← daemon trust
├── node/
│   ├── identity/private_key.pem     ← daemon's Ed25519 signing key
│   ├── vault/{private_key,public_key}.pem  ← X25519 sealed secrets
│   ├── config.yaml                  ← daemon config
│   ├── sandbox.yaml                 ← create-once node execution policy
│   ├── bundles/
│   │   ├── core.yaml                ← signed registration records
│   │   └── standard.yaml
│   └── engine/kinds/                ← merged kind schemas cache
├── bundles/
│   ├── core/.ai/...                 ← installed core bundle
│   ├── standard/.ai/...             ← installed standard bundle
│   └── <custom>/.ai/...             ← any additional bundles
└── state/
    ├── objects/                     ← CAS object store
    └── refs/                        ← CAS refs
```

Items installed via bundles are signed by the publisher's key and
verified during install. System space items are **immutable** — you
cannot edit them directly. To customize, copy to project space first:

```
ryeos fetch tool:ryeos/core/sign --to project
```

The copied item lives in project space and shadows the system
version (first match wins).

`sandbox.yaml` is operator-owned mutable node policy rather than installed
bundle content. It defaults to disabled, is loaded as an immutable snapshot at
daemon startup, and is documented in [Execution
Sandbox](node/execution-sandbox.md).

## Installing Bundles

Bundles are installed by `ryeos init` (not the daemon). The CLI:

1. Scans the source directory for child directories containing `.ai/`
2. Verifies every item in each bundle against the trust store
3. Verifies the bundle manifest (signature, identity, provides_kinds)
4. Checks cross-bundle dependencies (requires_kinds satisfied)
5. Copies bundles into `<system>/.ai/bundles/<name>/`
6. Writes signed registration records at `<system>/.ai/node/bundles/<name>.yaml`

```bash
# Packaged install (default source):
ryeos init

# Development:
ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

# Docker:
ryeos init --source /opt/ryeos
```

Init is **idempotent** — running it again preserves keys, atomically
replaces bundles (stage → swap with one-generation backup), and
re-validates registrations.

## Resolution Order

For a canonical ref like `tool:ryeos/core/sign`:

```
1. <project>/.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}
2. <bundle>/.../.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}  (for each installed bundle)
```

If a project-level item exists, it **shadows** the system version.
This allows local overrides without modifying bundles.

## Bundle Discovery

Bundle names are derived from directory names on disk — there are no
hardcoded names. Any immediate child directory of the source that
contains `.ai/` and has a valid name (lowercase alphanumeric, hyphens,
underscores, 1–64 chars) is recognized as a bundle.

The daemon boots fine with any registered bundle(s). If a required kind
isn't provided by any installed bundle, resolution fails at runtime
with a clear error — not at install time.
