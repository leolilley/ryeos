<!-- ryeos:signed:2026-11-17T06:36:01Z:f4f6b6f03b17b49f392608d131ff95b7a29bce3ca35106dc792b1f3cb43af8b2:IclGRcKHCns88DzPSSaqSNyK2EU8/3kzDMAWycWlI6dYCjAZAK7dErHPZqyx8ZBMA6azGeDvmxaYE1mGytHoCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
they write there is discarded when the process ends.

A project-backed request says what it asks of the live project, and the thread
records it as `project_authority.live_access.intent`:

- `default` (a plain project-backed `ryeos execute`, handler routes, the
  default execution policy). The request did not ask for the live project; the
  daemon picks the root for the tool's shape. It is not refused.
- `live` (`ryeos execute --live-project`, an execution policy whose project
  leg carries `"intent": "live"`, and every schedule that sets `live_direct`).
  The request demands the live project. A launch whose resolved root is not the
  live project is refused before the tool starts with
  `live_project_unavailable_to_sourced_runtime` (403), so it cannot exit 0 with
  its project writes silently lost. Children inherit the intent.
  An offline CLI run (no daemon) carries and enforces the same demand with the
  same refusal, and refuses it for anything that is not a tool.

| Tool shape | `default` request | `live` request (`--live-project`, `project.intent: live`, schedule `live_direct`) | `--pin-project` / pinned policy |
| --- | --- | --- | --- |
| `@subprocess` tool, no `source_scope`, no external content | live project (writes land in the project) | live project | copy-on-write generation |
| Tool on a `source_scope` runtime (python script/function, bash script) | sparse private input root; writes discarded | refused: `live_project_unavailable_to_sourced_runtime` | copy-on-write generation |
| `@subprocess` tool that retains external content | sparse private input root; writes discarded | refused: `live_project_unavailable_to_sourced_runtime` | copy-on-write generation |
| Handler-route tool (scratch-root semantics) | its scratch root, as before; the route does not demand the live project | not offered by routes | not offered by routes |

The intent is what the request asked for; the root is what the tool got. The
daemon records the root once per launch attempt, when it selects the process
input root, as a `process_input_root_selected` thread event whose payload is
`{"root": "<kind>"}` and names no path. `root` is `live` (the live project),
`sparse_private` (the private input root of the table's sourced rows), `pinned`
(a pinned generation), `candidate_integration` or `projectless`. Read it in the
thread's events (`ryeos events replay`, the node events view)
beside `project_authority.live_access.intent` on `ryeos thread get`; a
recovered launch records one per attempt. The timeline drops it as bookkeeping.

To give a tool the live project, write it as an `@subprocess` tool without
`source_scope` (`executor_id: "@subprocess"` with its own `command`). To run a
python or bash runtime tool on a project, launch it pinned (`--pin-project` or
a pinned project policy), which gives a copy-on-write generation, or return
structured results.

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
