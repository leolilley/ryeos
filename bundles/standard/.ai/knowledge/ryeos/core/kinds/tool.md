<!-- ryeos:signed:2026-07-15T07:49:18Z:712ef9a41805c1abfaeaedb76eae46b38ac6c683fad20303cd57c094a952e553:69C3Tdn/ludvnN6eFxXuZhzWuqwVY8kt3WTQaibpj8CSRqKxwJrqFWEd4mNJ29k0y8XO5ve9UFBVVgMkmS+sDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, tool, subprocess]
version: "1.0.0"
description: Tool kind reference.
---

# Kind: tool

Invariant: `tool` items execute through the signed callback-capable terminal
protocol and may use `@subprocess` as the canonical subprocess executor alias.

- Directory: `tools/`
- Formats: Python, YAML, JavaScript/TypeScript, JSON
- Protocol: `protocol:ryeos/core/tool_callback`
- Composer: identity
- Runtime blocks: config, env_config, config_resolve, verify_deps, execution_params, native_async, native_resume

Tool descriptors may declare `required_caps`, `required_secrets`, config schemas, executor ids, and command/runtime configuration. The plan builder rejects unknown runtime blocks.

The protocol treats stdin and terminal stdout as opaque while explicitly
declaring callback socket, callback token, thread-auth, thread, and project env
sources. Default wrappers normally encode params as JSON, but executor
`input_data` remains plan-owned. The daemon mints only the tool's verified
item/manifest capabilities; empty effective capabilities deny capability-gated
resource operations. Exact-thread and chain-local lifecycle methods still use
their documented token/access class. A schema that deliberately selects the
separate `opaque` protocol gets the same terminal I/O shape without callback
credentials or daemon-socket access.

## Runtime secrets and config

`required_secrets` is the tool-level contract for secret injection. At
dispatch time, Rye OS reads exactly those declared names from the node
vault, host environment, or `.env` overlay, then injects only those names
into the subprocess environment. Missing names fail before the tool is
spawned.

```yaml
category: ryeos-email/webhook/ses_event
executor_id: "@subprocess"
required_secrets:
  - RYEOS_EMAIL_ROUTE_SIGNING_SECRET
  - AWS_SES_WEBHOOK_SECRET
```

Use `required_secrets` for secrets only. Non-secret runtime values such
as public base URL, redirect allowlists, regions, and feature flags
should be modeled as ordinary tool config, project config, or parameters
so operators can inspect them without vault access.

Handler routes pass request data as the tool's parameters envelope; they
do not replace `required_secrets`. A public OAuth or webhook handler will
typically use both: route `source_config.request` for incoming HTTP data,
and tool `required_secrets` for provider credentials or signing keys.
