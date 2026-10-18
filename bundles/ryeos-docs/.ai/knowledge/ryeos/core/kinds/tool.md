<!-- ryeos:signed:2026-08-18T22:04:49Z:3de8593f896fe0a3ac0777a3ac149239261e441965602689cd187ac4f7e5aec7:SiSaPufwEwrv9p0vtpFEH1/6BVuauJPTbPRchBy+u30s/Q8gabSejvIHp5Btmva5QvFrJvZMkJNennkyQQROBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, tool, subprocess]
version: "1.1.0"
description: Tool kind reference.
---

# Kind: tool

Invariant: `tool` items execute through the signed callback-capable terminal
protocol and may use `@subprocess` as the canonical subprocess executor alias.

- Directory: `tools/`
- Formats: Python, YAML, JavaScript/TypeScript, JSON
- Protocol: `protocol:ryeos/core/tool_callback`
- Composer: identity
- Runtime blocks: config, env_config, config_resolve, execution_params, native_async, native_resume

Tool descriptors may declare `required_caps`, `required_secrets`, config schemas, executor ids, and command/runtime configuration. The plan builder rejects unknown runtime blocks.

## Adjacent source

Source owned by a tool stays beside it. A top-level tool admits only its root
file. A namespaced tool admits the regular files in its namespace, including
the conventional namespace `lib/` directory. Every project source file must be
signed by the exact root tool owner; bundle tools use the same per-file owner
testimony in addition to their exact publisher generation. The signed executor chain contributes one `source_scope`
ceiling that names the loader roots but cannot add files or widen ownership.

RyeOS captures and verifies this source before minting runtime authority,
retains its content manifest and authority binding, and shadows the matching
logical `.ai/tools/` path during execution. Enforced nodes receive the shadow
as a descriptor-pinned read-only mount. Disabled nodes receive the same exact
retained bytes in a daemon-private per-process input root, without changing the
tool's execution identity or requiring a full project pin. The protected
`RYEOS_ADMITTED_SOURCE` value carries only canonical source identity. Tool code
does not declare its own files through `external_content` and must not hash or
reopen live source to establish execution identity.

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
