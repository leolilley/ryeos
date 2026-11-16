<!-- ryeos:signed:2026-11-16T03:56:03Z:1c623ef57c29b39aa292ed286c23e77406bd7ef83e5b1bed10c6d16239eb8bad:ultF/+B/PJrHp3PkC0A9WLaU4+qOg1G2lZLtBeZiFUu1HA5V8BadczuBvTu8dDQqWAzfzhI7+Ev/lAWZGItUBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, tool, subprocess]
version: "1.2.0"
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
logical `.ai/tools/` path during execution. A pinned or projectless launch on
an enforced node receives the shadow as a descriptor-pinned read-only mount.
A disabled node, and every launch with live project authority, receives the same
exact retained bytes in a daemon-private per-process input root, without
changing the tool's execution identity or requiring a full project pin. That
root holds only the verified source, never the live project. The protected
`RYEOS_ADMITTED_SOURCE` value carries only canonical source identity. Tool code
does not declare its own files through `external_content` and must not hash or
reopen live source to establish execution identity.

Tools on a runtime that declares `source_scope` (the python script and function
runtimes and the bash script runtime) never see the live project, whatever the
launch's project authority says. Their working directory and `${project_path}`
are the private input root, which holds only the verified source, and any file
they write there is discarded when the process ends. A schedule fire whose
`execution_policy` grants live project authority (`live_direct`, read-only or
read-write) is refused before the tool starts with
`live_project_unavailable_to_sourced_runtime`, so it cannot exit 0 with its
project writes silently lost. Other live launches (a project-backed
`ryeos execute`, handler routes, children of those) resolve to live authority by
default and are not refused; they run from the private root and the same rule
applies. To give a tool the live project, write it as an `@subprocess` tool
without `source_scope` (`executor_id: "@subprocess"` with its own `command`).
To run a python or bash runtime tool on a project, launch it pinned
(`--pin-project` or a pinned project policy), which gives a copy-on-write
generation, or return structured results.

The protocol treats stdin and terminal stdout as opaque while explicitly
declaring callback socket, callback token, thread-auth, thread, and project env
sources. Default wrappers normally encode params as JSON, but executor
`input_data` remains plan-owned. The daemon mints only the tool's verified
item/manifest capabilities; empty effective capabilities deny capability-gated
resource operations. Exact-thread and chain-local lifecycle methods still use
their documented token/access class. A schema that deliberately selects the
separate `opaque` protocol gets the same terminal I/O shape without callback
credentials or daemon-socket access.

## Host authority

A tool may declare `host_authority: required`: it asks to run on the host,
unconfined, as the daemon user. The kind projects it like `network_authority`
and freezes it into the plan as `execution_posture`. Declaring it grants
nothing: the node lane, an exact operator grant and the configured local
operator decide. Such a tool must use `protocol:ryeos/core/opaque`, run
`live`, and author none of `network_authority`, `filesystem_authority`,
`workspace_access`, `execution_endpoint`, `supported_target`,
`resource_authority`, `project_lane` or external content. See
`ryeos/core/node/host-access`.

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
