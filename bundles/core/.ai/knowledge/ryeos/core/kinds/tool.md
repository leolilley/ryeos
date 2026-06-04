<!-- ryeos:signed:2026-05-31T08:15:56Z:758919ffba7cf46a469f3b5ea0a530d1e70ba72419709efb1416c898e0b6284b:CPpQHV4Cm/qoA0dtMCKwcznX47qF+HPvc42S/ABfpWr39HBSIkIU+oQidN4mfa8aSpAGzpSwYkg7DpNmyK/RDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
