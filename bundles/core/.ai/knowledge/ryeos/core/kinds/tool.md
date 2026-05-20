---
category: ryeos/core/kinds
tags: [kind, tool, subprocess]
version: "1.0.0"
description: Tool kind reference.
---

# Kind: tool

Invariant: `tool` items execute through the opaque subprocess protocol and may use `@subprocess` as the canonical subprocess executor alias.

- Directory: `tools/`
- Formats: Python, YAML, JavaScript/TypeScript, JSON
- Protocol: `protocol:ryeos/core/opaque`
- Composer: identity
- Runtime blocks: config, env_config, config_resolve, verify_deps, execution_params, native_async, native_resume

Tool descriptors may declare `required_caps`, `required_secrets`, config schemas, executor ids, and command/runtime configuration. The plan builder rejects unknown runtime blocks.
