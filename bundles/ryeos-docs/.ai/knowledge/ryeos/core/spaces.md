<!-- ryeos:signed:2026-11-13T04:44:36Z:143738cd3954c9f07486142bb8ed966d4dc96382a370045b4c094040f613450c:4lwVcJWdelOu5Xa12DvkYvfpwpdQjIpF1Ag6RS2NNsAcDe99i2giJYThzca8SXn/R/7HXW8/Fkm55OyPtL+7AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, spaces, resolution, bundles]
version: "2.2.1"
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
│   ├── policies/<section>.yaml      ← complete node-signed policy generation
│   ├── sync/policy.yaml             ← read-only derived sync view
│   ├── bundles/
│   │   ├── core.yaml                ← signed registration records
│   │   └── knowledge.yaml           ← one record per installed bundle
│   └── engine/kinds/                ← merged kind schemas cache
├── bundles/
│   ├── core/.ai/...                 ← installed core bundle
│   ├── knowledge/.ai/...            ← installed knowledge bundle (likewise directive, graph, ryeos-docs)
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

`.ai/node/policies/isolation.yaml` is one member of the complete node-signed
policy generation rather than installed bundle content. The selected init
profile explicitly publishes disabled isolation on a fresh distribution; no
runtime default creates it. A stopped-node policy transaction may replace the
section, and daemon startup loads the complete generation as one immutable
snapshot. See [Execution Sandbox](node/execution-isolation.md).

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
ryeos init --node-profile full --host-class local

# Development:
ryeos init --source bundles --node-profile full --host-class local \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

# Distribution image entrypoint:
ryeos init --source /opt/ryeos \
  --node-profile "$RYEOS_INIT_NODE_PROFILE" \
  --host-class "$RYEOS_INIT_HOST_CLASS"
```

Fresh initialization has no absent-policy fallback. The selected signed init
profile is narrowed to the one host class the deployment names (`local` on a
host the operator runs, `hosted` in a hosting provider's container, among the
classes the profile declares); the image never sets the class. The profile
names the exact source bundle inventory and all required node-policy
sections; a mismatch fails before installation.

Init is **idempotent** — running it again preserves keys, atomically
replaces bundles (stage → swap with one-generation backup), and
re-validates registrations. It also preserves an existing complete signed
policy generation; the distribution profile is first-publication authority, not
an overwrite path.

## Resolution Order

For a canonical ref like `tool:ryeos/core/sign`:

```
1. <project>/.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}
2. <bundle>/.../.ai/tools/ryeos/core/sign.{py,yaml,js,ts,...}  (for each installed bundle)
```

If a project-level item exists, it **shadows** the system version.
This allows local overrides without modifying bundles, with one exception:
names inside a bundle-owned namespace (below) are never shadowed.

Shadowing is always reported. `ryeos fetch` and `items.effective` mark an
item with `shadows_bundle_item: true` when the project copy won and a bundle
also ships an item under that ref, so an override is visible rather than
silent.

### Owned namespaces

A bundle declares the namespaces it owns in its signed manifest, in
`.ai/manifest.source.yaml` (the generated `.ai/manifest.yaml` carries it
forward and is re-signed by the publish pipeline):

```yaml
# <bundle>/.ai/manifest.source.yaml
name: core
owns_namespaces:
  - ryeos/core
```

An entry is a literal run of `/`-separated segments (letters, digits, `-`,
`_`, `.`). Wildcards, empty or `..` segments, and entries inside one another
are refused when the manifest is generated or loaded. `ryeos/core` owns
`tool:ryeos/core/runtimes/python/function`, `handler:ryeos/core/...` and
every other kind's ref whose bare id equals `ryeos/core` or starts with
`ryeos/core/`.

A project file whose ref is inside an owned namespace is not a candidate. It
is refused with a 409 `reserved_namespace_shadow` that names the owning
bundle, the project file and the cure, and the project copy never runs. The
same holds for executor-chain hops: a tool whose `executor_id` names a
runtime in an owned namespace gets that runtime from the bundle only. To
customize, put your copy in your own namespace (for example
`.ai/tools/<project>/runtimes/python/function.yaml`) and point `executor_id`
at it.

Two installed bundles cannot claim overlapping namespaces: bundle install and
`ryeos bundle verify` refuse the candidate, and a node refuses to start on an
installed set that does.

## Bundle Discovery

Bundle names are derived from directory names on disk — there are no
hardcoded names. Any immediate child directory of the source that
contains `.ai/` and has a valid name (lowercase alphanumeric, hyphens,
underscores, 1–64 chars) is recognized as a bundle.

The daemon boots fine with any registered bundle(s). If a required kind
isn't provided by any installed bundle, resolution fails at runtime
with a clear error — not at install time.
