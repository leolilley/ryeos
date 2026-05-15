---
category: ryeos/core
tags: [fundamentals, spaces, resolution, bundles]
version: "2.0.0"
description: >
  The three-tier space resolution system — project, user, and system
  (bundle) spaces, how bundles are installed, and how items are found.
---

# Spaces

Rye OS resolves items across three tiers: **project**, **user**, and
**system**. The first match wins.

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

## User Space

```
~/.ai/
```

Cross-project personal items. Shared across all projects on the same
machine. Good for personal directives, aliases, or knowledge that
applies everywhere.

User space also holds the operator's signing key and trust store:

```
~/.ai/
├── config/keys/
│   ├── signing/private_key.pem      ← your Ed25519 identity
│   └── trusted/
│       ├── <publisher-fp>.toml      ← trusted publisher keys
│       ├── <user-fp>.toml           ← self-trust
│       └── <node-fp>.toml           ← daemon trust
├── tools/                           ← personal tools
├── directives/                      ← personal directives
└── knowledge/                       ← personal knowledge
```

## System Space

```
<system_space_dir>/.ai/
```

The daemon's runtime state and installed bundles. Defaults to the XDG
data directory (`~/.local/share/ryeos/` on Linux). Created by
`ryeos init`.

```
<system_space_dir>/.ai/
├── node/
│   ├── identity/private_key.pem     ← daemon's Ed25519 signing key
│   ├── vault/{private_key,public_key}.pem  ← X25519 sealed secrets
│   ├── config/                      ← daemon config
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
cannot edit them directly. To customize, copy to project or user
space first:

```
ryeos fetch tool:ryeos/core/sign --to project
# or
ryeos fetch tool:ryeos/core/sign --to user
```

The copied item lives in project/user space and shadows the system
version (first match wins).

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
ryeos init --source ryeos-bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

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
2. ~/.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}
3. <bundle>/.../.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}  (for each installed bundle)
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
