<!-- ryeos:signed:2026-11-12T23:57:18Z:6e6d79f2d1684bb998d577a13a8dd016740cedd62483d3ee36602915588bf559:JEBslUZHFSf1uhoNWDmEau3hLB0JaRrjfMooPk0u4URYufOKuyD+OnmixjB86kAYgGBKntJMZzdPmo3wYREXAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/engine
tags: [engine, resolution, canonical-refs, spaces, bundles]
version: "1.0.0"
description: >
  How Rye OS resolves canonical refs to concrete signed items.
---

# Engine Resolution

Invariant: a canonical ref resolves by kind-directed directory lookup, with project space overriding installed bundles.

## Canonical refs

Canonical refs have the form `kind:path/to/item`. The `kind` selects a kind schema; the path is interpreted relative to that kind's `location.directory` and without the file extension.

Examples:

- `tool:ryeos/core/sign` → `tools/ryeos/core/sign.yaml`
- `parser:ryeos/core/yaml/yaml` → `parsers/ryeos/core/yaml/yaml.yaml`
- `service:threads/list` → `services/threads/list.yaml`

## Search order

Resolution searches:

1. Project `.ai/`
2. User `.ai/`
3. Installed system bundles, as registered under daemon state

First match wins. Bundle order is a daemon bootstrap concern, but each installed bundle remains a signed `.ai/` tree and contributes sections independently.

The path of a ref never names a bundle. `config:codex/foo` is `.ai/config/codex/foo.yaml` in whichever root holds it first; `codex` is a directory. A downstream bundle that adds items, such as a site bundle of signed policies, should use a directory no other installed bundle uses, so its refs cannot shadow or be shadowed by a shipped item. A shadowed candidate is recorded in the resolution, not refused.

## Format selection

The kind schema lists accepted extensions and parser refs. The resolver finds the concrete file and dispatches to the parser for the matching extension. For markdown knowledge or directives this also determines the signature envelope (`<!-- ... -->`); for YAML descriptors the envelope uses `#`.

## What does not happen

Resolution does not infer execution fallback from paths. If a kind delegates to a runtime registry, the kind schema must declare that. If a tool uses `@subprocess`, that alias is declared on the kind execution block.
