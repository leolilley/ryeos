<!-- ryeos:signed:2026-05-31T08:15:56Z:ca8a96d03da785ad210034f1fdc07f5dfb8935972561327cc114a709fa4b0b16:nL3UAVC68gaGPILP95ukKT6U9dLBD3dwMWzoHxdjEsAP9rcuYpK/YFZNcWbLMpsEfOdD5c5YtrJe2sNTfsFmCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, handler, bootstrap]
version: "1.0.0"
description: Handler kind reference.
---

# Kind: handler

Invariant: `handler` items describe parser and composer binaries loaded during bootstrap; they are registry entries, not root-executable items.

- Directory: `handlers/`
- Formats: signed YAML via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/identity`
- Execution: none
- Required fields include `name`, `category`, `serves`, `binary_ref`, and `abi_version`

Handlers break the bootstrap cycle by being loaded as raw signed YAML before the full parser/composer pipeline is available.
