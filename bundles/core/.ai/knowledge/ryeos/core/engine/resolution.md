---
category: ryeos/core/engine
tags: [engine, resolution, canonical-refs, spaces, bundles]
version: "1.0.0"
description: >
  How Rye OS resolves canonical refs to concrete signed items.
---

# Engine Resolution

Invariant: a canonical ref resolves by kind-directed directory lookup, with project space overriding user space and installed bundles.

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

## Format selection

The kind schema lists accepted extensions and parser refs. The resolver finds the concrete file and dispatches to the parser for the matching extension. For markdown knowledge or directives this also determines the signature envelope (`<!-- ... -->`); for YAML descriptors the envelope uses `#`.

## What does not happen

Resolution does not infer execution fallback from paths. If a kind delegates to a runtime registry, the kind schema must declare that. If a tool uses `@subprocess`, that alias is declared on the kind execution block.
