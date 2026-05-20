---
category: ryeos/core/kinds
tags: [kind, config]
version: "1.0.0"
description: Config kind reference.
---

# Kind: config

Invariant: `config` items are signed per-domain YAML mappings that are read by consumers but are not directly executable.

- Directory: `config/`
- Formats: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/identity`
- Execution: none
- Required metadata: `category`; `name` is derived from the filename

Use config items for runtime routing, execution defaults, trust records, and other domain-specific settings where the schema is enforced by the consumer rather than by the generic kind contract.
