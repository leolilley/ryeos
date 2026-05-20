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
