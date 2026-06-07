<!-- ryeos:signed:2026-06-07T03:08:13Z:8fa3638c2e9be081fad2c05b99c35963f5fc62448051744faef84d263de89059:T7S0ZJgR+0rK/imS52xtEFGXi5rpt0Lq8tSwgbyRTtOB2pjVB/bXVKr9bjRWxnfiaLwOB2sC9flu0FZ5LcD4DQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
