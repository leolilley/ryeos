```yaml
category: ryeos/future
name: declared-config-shadows
title: Declared Config Shadows
entry_type: implementation_guide
version: "0.1.0"
description: A `shadows:` manifest field making cross-bundle config overrides signed intent instead of a lint inference.
tags:
  - manifest
  - config
  - namespace-lint
  - publish
```

# Future: Declared Config Shadows

## Status

Deferred. Cross-bundle config shadowing works and is supported; publish
surfaces it as an INFO-level note inferred by the namespace lint. This doc
records the upgrade path from inference to signed intent.

## The mechanism being shadowed

Project-first resolution lets a bundle override another bundle's runtime
config by shipping an item under the foreign ref — e.g. a project bundle
carrying `config:ryeos-runtime/execution` to override runtime defaults.
The foreign namespace is REQUIRED for the shadow to work (renaming it into
the bundle's own namespace silently breaks the override), which is exactly
why the namespace lint cannot treat foreign-namespace items as errors:
it narrows to the real hazard (cap-minting items) and downgrades inert
foreign-namespace items to an INFO note naming what they shadow.

## The residual weakness

The shadow is inferred from directory layout. Nothing in the bundle's
signed surface SAYS "this bundle intends to override ryeos-runtime's
execution config" — so a reviewer reading the manifest sees nothing, an
accidental foreign-namespace item is indistinguishable from a deliberate
shadow, and tooling cannot warn when a shadow's target stops existing.

## Design

A `shadows:` list in `manifest.source.yaml` — signed intent:

```yaml
shadows:
  - config:ryeos-runtime/execution
  - config:ryeos-runtime/limits
```

The namespace lint then checks declared-vs-observed both ways: an observed
shadow with no declaration warns ("undeclared override — declare it or
rename it"); a declaration with no matching item warns (stale intent).
The INFO note becomes a verification, not an inference.

Seams: `BundleManifestSource`
(`crates/daemon/ryeos-bundle/src/manifest.rs`) and
`lint_item_namespaces` (`crates/tools/core-tools/src/actions/publish.rs`).

## Trigger

Shadow-related confusion recurring downstream. One real-world incident
produced the INFO note; a second incident means inference is not enough
and the intent must be signed.
