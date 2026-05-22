<!-- ryeos:signed:2026-05-22T03:35:36Z:c8ebbc2f22219ece010cae3f4cc7f30199aa2b2a9d9553a85aaeb4e98f7f931d:H4yo2SLQsy+FTjAwRIxCpukBzZGRI4ZfA+dFRusDyBjlzsrtsgHGMQadYxDK+yzRvzBBOG8ay67EvqndD3QABw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
