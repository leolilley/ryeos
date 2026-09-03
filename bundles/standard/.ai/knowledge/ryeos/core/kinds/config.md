<!-- ryeos:signed:2026-08-27T08:43:19Z:be6e72198c7d3b50c42a0f09064810f49008e51025cb205cd921c348c59d47f9:xRZjg3QivsC7FZLndGrImRlDezXTLdpQALCSOIRlFCO3tEYfSqJczgNevTbKCeO+chsdUib2GB1oTdRKodczDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, config]
version: "1.1.0"
description: Config kind reference.
---

# Kind: config

Invariant: `config` items are signed per-domain YAML mappings that are read by consumers but are not directly executable.

- Directory: `config/`
- Formats: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/identity`
- Execution: none
- Content: up to eight exact locator-free declarations, with an aggregate
  4 GiB large-content ceiling
- Required metadata: `category`; `name` is derived from the filename

Use config items for runtime routing, execution defaults, trust records, and other domain-specific settings where the schema is enforced by the consumer rather than by the generic kind contract.

The content contract does not make a config executable and does not cause its
content to be mounted automatically. It permits a trusted installed config to
name pinned manifests for a separate runtime that explicitly selects that
config as a content dependency. Empty `allowed_roots` is deliberate: config
content may be content-addressed and target-locally bound, but it cannot gain a
project, node-files, or bundle-relative filesystem locator.
