<!-- ryeos:signed:2026-11-24T08:30:56Z:d1a8e10d552836c4da6b8756af1840b1666b1b83aeb2b53143d4e3e67e1482d3:xgO0x89zTzRK2A4dEAdaUDIszEMxtYfDxEB9qe7xqQVFoPTVzWfyRHOPPQsVq7hpzrekw+AD7XtNZ7jHgw51Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [fundamentals, mcp, key-holder, tools]
version: "3.0.0"
description: >
  The MCP server as its own key holder: the single `cli` tool, the key it acts
  with, the grant that bounds it, and what that does and does not cover.
---

# MCP Key Holder

The MCP (Model Context Protocol) server lets an outside tool call Rye OS. It is
a thin wrapper that exposes a single tool, `cli`, which shells out to the
`ryeos` CLI binary. The server is a **key holder**: it acts with a signing key of
its own, under a grant that a person gives that key. It never borrows the
operator's key.

## Authority: the key and its grant

Authority in Rye OS comes from a key, the grants and capabilities attached to it,
and nothing else. The MCP server follows the same rule as every other client:

- It starts only when `RYEOS_CLIENT_KEY` names its key file in the server's
  environment. There is no default and no fallback to the operator's key.
- It hands that key to every `ryeos` process it starts, so every request is
  signed by the server's key.
- The daemon authorizes each request by that key's grant, the way it does for any
  other key. A service the grant does not name is refused with a
  missing-capability error, for whoever holds the key.
- `endorse` and other operator-only acts are refused because the key is not a
  pinned operator signer; `ryeos sign` run with it produces a signature by a key
  nobody pinned, so the result is untrusted.

The server adds no gating of its own and does not look at what is calling it. To
restrict it further, narrow the key's grant.

### Set it up

```bash
# 1. The server's own key, readable by its owner only.
openssl genpkey -algorithm ed25519 -out mcp-key.pem && chmod 600 mcp-key.pem

# 2. Its public key in the form a grant takes (ed25519:<base64 of the 32 raw bytes>).
echo "ed25519:$(openssl pkey -in mcp-key.pem -pubout -outform DER | tail -c 32 | base64)"

# 3. As the operator, authorize that public key with a narrowed grant and a
#    lifetime (see `ryeos help authorize-key` for the form).
ryeos authorize-key ...

# 4. Run the server with the key.
RYEOS_CLIENT_KEY=/abs/path/to/mcp-key.pem ryeosd-mcp
```

The key file must be an absolute path to a regular file readable by its owner
only (mode 0600), and it must not sit inside the node's own state
(`<app root>/.ai/node/`). Anything else is refused with `client_key_unreadable`.

Give the key only the capabilities its work needs, for example
`ryeos.execute.service.threads/list` and `ryeos.execute.service.threads/get` to
read threads, `ryeos.execute.tool.<id>` for particular tools, and the project
read or write capabilities for the projects it works in. Leave out
`ryeos.execute.service.identity/authorize-key`, the endorsement, vault, bundle
and remote-trust services. The daemon enforces what the grant actually says.

Grant hygiene:

- Give the grant a lifetime, and authorize the key again when it lapses.
- Withdraw it with `ryeos revoke-key` when the server is retired or the key may
  have leaked.
- Name each capability. Do not grant wildcard scopes.

### Threads and projects belong to the key

The MCP key is its own principal. Threads it starts are its own, not the
operator's: list them with `RYEOS_CLIENT_KEY=<path> ryeos thread list`; the
operator's own list does not show them. No registration speaks for this key, so
runs under it have no live project identity, and operator-scoped signers are not
trusted in them. That is intended for a narrowed key.

### What this does not cover

Commands that act directly on a node's files (key rotation, signer pinning,
project registration, state resets) have no daemon to check them. They need the
node's operator key, so with another key selected they are refused with
`operator_key_required`. But a file's permissions are an OS fact: on a node that
does not enclose its processes, a process of the same OS user can read the
operator's key files. Operations on a node's files require the operator key, and
only its holder can use it; to keep that key out of reach, run the server as a
separate OS user or inside the enclosure.

Do not expose the server over a network without a separate auth-terminating
proxy: the key it holds is the only gate.

## Protocol revisions

The server uses the Python MCP SDK 2.x (`mcp>=2.3,<3`) and serves both MCP
revisions from one process: the stateless revision (2026-07-28: `server/discover`,
protocol version and client capabilities on every request) and the earlier
`initialize` handshake. A client picks its own era with its first request. What a
client reports about itself (`clientInfo`) is display only; nothing here decides
anything by it, and the server's credential is only ever `RYEOS_CLIENT_KEY` from
its own environment (the MCP stdio rule), never anything a request carries.

## The `cli` Tool

The server exposes one tool:

```json
{
  "tool": "cli",
  "args": ["fetch", "tool:my/helper", "--with-content"],
  "project_path": "/path/to/project"
}
```

### Parameters

| Field | Type | Required | Description |
|---|---|---|---|
| `args` | string[] | Yes | argv passed to `ryeos`. Do NOT include `ryeos` as the first element. |
| `project_path` | string | No | Sets the subprocess working directory. Defaults to the MCP server's cwd. |
| `timeout_s` | number | No | Seconds before the subprocess is killed. Default 60. Minimum 1. |

### Returns

A JSON object with `exit_code`, `stdout`, `stderr`, and (if stdout is valid JSON)
a parsed `json` field. On validation errors or timeouts, a typed error is
returned with `error` and `type` fields.

### Discovery

```json
{"tool": "cli", "args": ["help"]}
```

Lists the verbs available in the current project. Verbs are loaded data-driven
from signed `.ai/node/commands/*.yaml` items in the installed bundles, so
installing a bundle makes its verbs callable with no MCP redeploy. Whether a verb
succeeds depends on the key's grant.

### Binary Discovery

The server finds the `ryeos` binary via:

1. `RYE_BIN` environment variable
2. `shutil.which("ryeos")` on PATH

## Example Invocations

```json
// Fetch an item
{"tool": "cli", "args": ["fetch", "tool:my/helper", "--with-content"], "project_path": "/path"}

// Thread operations
{"tool": "cli", "args": ["thread", "list"], "project_path": "/path"}
{"tool": "cli", "args": ["thread", "tail", "T-abc123"], "project_path": "/path"}

// Execute a directive (needs the execute capability for it)
{"tool": "cli", "args": ["execute", "directive:my/workflow"], "project_path": "/path"}
```

## Workflow Patterns

### Pattern 1: Execute and Follow
```json
{"tool": "cli", "args": ["execute", "directive:deploy"], "project_path": "/path"}
```
The response contains the execution result. For directives, follow the returned
directions.

### Pattern 2: Discover First
```json
{"tool": "cli", "args": ["help"], "project_path": "/path"}
```
List the available verbs, then run the one you need.

### Pattern 3: Read-Only Inspection
```json
{"tool": "cli", "args": ["fetch", "tool:my/helper", "--with-content"], "project_path": "/path"}
```
Inspect an item without executing it.
