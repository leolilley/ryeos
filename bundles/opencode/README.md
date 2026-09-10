<!-- ryeos:signed:2026-09-10T10:35:03Z:71f738e406372fb7ed96a57bc19eba26a0b1aa3627e88a40b36e54bea4a2956b:pUzmJvSTcfAy+i7kxFGtXRMQrsMDkwMXBQg7DMGhTOT+mwum52qNWshjAhvJ4Go0bUjn2yhZL4w8Wl1uKAfhBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# RyeOS OpenCode

Provider data for hosting a pinned OpenCode server through RyeOS's generic
structured-session bridge.  The bridge is Core-owned; this bundle contains
only OpenCode's exact executable, protocol schemas and signed profile.

This provider is not a substitute for the ChatGPT-subscription Codex product
path.  It proves the substrate is provider-neutral.  Live OpenCode acceptance
remains contingent on an explicitly provisioned low-cost provider credential;
RyeOS never imports an ambient OpenCode credential.

## Verified protocol facts (opencode 1.18.30, probed 2026-09-10)

- `opencode serve --port <n> --hostname 127.0.0.1 --pure` starts a headless
  HTTP server. `--pure` disables external plugins. The bound address is
  printed on stdout as `opencode server listening on http://127.0.0.1:<port>`
  (logs go to stderr; stdout carries the listening line).
- `OPENCODE_SERVER_PASSWORD` (basic auth, user defaults to `opencode`) is the
  server-side authentication boundary; unset prints a warning and leaves the
  server unsecured. The bridge must set a per-boot random password.
- OpenAPI 3.1 JSON is served from `GET /doc` with `Accept: application/json`.
  The 1.18.30 spec is vendored as 109 self-contained schemas beside this
  profile source: 15 route request/response schemas plus all 94 event
  documents, component refs rewritten to local `#/$defs` fragments.
- `GET /event` is an SSE stream. Each frame is one `data:` line:
  `{"id":"evt_...","type":"server.connected","properties":{}}`; the first
  event is `server.connected`.
- Sessions are scoped to the server process cwd (`POST /session` records the
  `directory`); the bridge already pins cwd to the CoW workspace.
- `GET /session/status` returned `{}` while idle sessions existed — absence
  from the status map must be treated as idle, presence as busy. A session
  survives a server restart unchanged inside its private home, so node-local
  recovery is `GET /session/{id}` plus the status map.
- State layout under the redirected XDG roots (probed with all four
  `XDG_*_HOME` variables set): data dir holds `auth.json`, `mcp-auth.json`,
  the unified `opencode.db*` (tables: session, message, part, permission,
  credential, account, todo, event, ...), `log/**`, `repos/**`; the config
  dir holds the baseline config seed plus a workload-written
  `node_modules/**`; the state dir holds `locks/**`. An older `storage/**`
  per-project layout exists on legacy installs but 1.18.30 writes the DB.

## Probe results that shape the profile

- **XDG redirection requires explicit environment variables.** With only
  `HOME` redirected, opencode still loaded the operator's real
  `~/.config/opencode/*` (XDG roots resolve through the OS user record) and
  a `{file:~...}` reference there failed startup against the redirected
  home. The bridge must set `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
  `XDG_STATE_HOME`, and `XDG_CACHE_HOME` to home-relative paths — the
  standard POSIX convention, not an opencode-specific spelling.
- **`OPENCODE_CONFIG_CONTENT` overrides project `opencode.json`** (verified:
  inline `share=disabled, autoupdate=false` defeated a project file setting
  both). It is the immutable per-boot authority channel above the admitted
  workspace's own config.
- **`--pure` does not suppress project `.opencode/agent/*.md`** — a project
  workspace agent appeared in `/agent` under `--pure`. Project-authored
  agents, commands, and instructions are admitted workspace prompt content,
  not plugins; the profile pins the agent on every turn route and the
  default agent in the inline config, so they cannot be silently selected.
- **Session state is not file-portable.** The bound session's rows live in
  the monolithic `opencode.db` beside the `credential` table. v1 therefore
  classifies the whole DB as node-private credential state: node-local
  resume works through the persisted home, and cross-site continuation of
  the upstream conversation is a documented non-goal (project candidates
  still hand off through the normal frozen-checkpoint machinery).
- **Permission asks correlate by `properties.id`** (pattern `per...`) with
  `sessionID`; the reply body is `{"response":"once"|"always"|"reject"}`.
  Under the deny-only posture every non-accept decision maps to `reject`
  and `once`/`always` are refused before upstream contact.

## Substrate change: profile transport

The bridge accepts only an explicit signed top-level `transport`.  Profile
schema 6 is a clean cut: it separates logical command input, signed path/query
projection, and upstream request body.  A path field is never injected into a
vendor body just because it is needed for addressing.

- `"stdio_jsonrpc"` — current behavior.
- `"http_sse"` — bridge uses the provider profile's bounded loopback-listener
  announcement, verifies a signed readiness endpoint, then speaks HTTP with
  per-boot basic auth. Route rules declare an exact body schema and every
  path segment's source (`input` or `bound_session`). SSE validates the signed
  event envelope projection before routing its properties; stream media type,
  partial events and aggregate frame size are bounded.

The compiler validates the closed HTTP/SSE vocabulary including listener,
readiness, event-envelope, path and body contracts.  A disposable-node test
must still execute the real loopback server: the Codex development sandbox
cannot create loopback sockets, so its unit fixture is intentionally not
claimed as installed-host or provider-contact evidence.

Loopback TCP inside the worker process scope is a new admission surface the
Codex stdio profile never needed; node policy must admit it explicitly for
this worker family rather than globally.

## Route mapping (initial)

| Route id | HTTP | Effect class |
|---|---|---|
| `credential.providers.read` | `GET /provider` | credential_read |
| `credential.auth.remove` | `DELETE /auth/{provider}` | credential_delete |
| `session.start` | `POST /session` | external_effect |
| `session.read` | `GET /session/{session_id}` | pure_read |
| `session.status.read` | `GET /session/status` | pure_read |
| `session.persist` | `PATCH /session/{session_id}` | session_mutation (runtime) |
| `turn.start` | `POST /session/{session_id}/prompt_async` | external_effect |
| `turn.interrupt` | `POST /session/{session_id}/abort` | external_effect |

`POST /session/{id}/message` waits for completion; `prompt_async` returns
204 and settles through events (`EventMessagePartUpdated`,
`EventSessionIdle`), which is the correct shape for durable observation.

## Notification mapping (initial subset)

| Event type | Treatment |
|---|---|
| `session.idle` | durable; turn-completion state observation |
| `session.created` / `session.deleted` | durable |
| `message.updated` / `message.part.updated` | durable, bounded payload |
| `message.part.delta` | durable, digest-only payload |
| `session.error` | durable (`workload.error` analog) |
| `permission.v2.asked` | server-request (deny-only) |
| `installation.update_available` | durable observation (update check flag) |
| remaining TUI/pty/lsp events | ignored_notifications |

Approval replies go to `POST /session/{session_id}/permissions/{permission_id}`.

## Configuration authority

- Immutable argv/env: `--pure`, `--hostname 127.0.0.1`, `--port` (bridge
  selected), per-boot `OPENCODE_SERVER_PASSWORD`, and
  `OPENCODE_CONFIG_CONTENT` (inline JSON, precedence above project config —
  the authority surface for model pinning, `permission: {"*":"ask"}`,
  `autoupdate: false`, `share: "disabled"`, empty `mcp`, provider pinning).
- Baseline seed: `OPENCODE_CONFIG` pointing at a per-generation-reset
  `<home>/opencode.json` compatibility file (single relative file name, same
  contract as Codex `config.toml`).
- XDG redirection into the private home: `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
  `XDG_STATE_HOME`, `XDG_CACHE_HOME` (and `OPENCODE_CONFIG_DIR`).

## portable_state selectors (data dir, verified against 1.18.30)

- `auth.json`, `mcp-auth.json`, `opencode.db*` -> node_private_credential_state
  (the unified DB carries the credential table)
- `log/**`, `repos/**` -> rebuildable_cache
- config tree: baseline seed -> forbidden_or_unknown (workload may rewrite
  it; never policy), `.config/opencode/node_modules/**`,
  `.config/opencode/.gitignore` -> rebuildable_cache
- `locks/**` under the state root -> rebuildable_cache
- everything else -> forbidden_or_unknown

## Network admission (verified against the substrate)

The per-execution network ceiling is `node_policy` or `isolated`, frozen
into the execution plan. The worker kind declares no network projection,
so its omission result is `node_policy`: under the default ceiling the
worker shares the node's host networking and the loopback transport works.

- An `isolated` ceiling creates a network namespace whose loopback
  interface is never raised, so an http_sse worker's listener discovery
  fails at launch. That is fail-closed: hardened nodes refuse this worker
  family rather than silently widening network authority. A future
  loopback-only ceiling could admit it explicitly.
- Unlike the Codex profile's workload-level sandbox network denial, an
  opencode worker's bash commands inherit the node network ceiling. The
  inline config keeps `webfetch` at ask (deny-only in v1), but bash egress
  is bounded only by node policy; a worker-kind network projection would
  be the mechanical fix if that gap must close.

## Current limits

- Credential write is deliberately absent.  A raw API key, OAuth access token
  or refresh token would be retained in durable command testimony.  Live use
  must wait for the generic vault-backed late secret-input contract; no caller
  may place provider secrets in a hosted command.
- OpenCode's unified database is node-local credential state.  It can resume
  locally but cannot claim portable upstream-conversation handoff without a
  portable subject projection.
- The default environment has no workload client and grants no Tool wildcard.
  A qualified authoring environment supplies a finite exact Tool set.
  worker-environment schema.
- Live qualification against an activated node (credential enrollment,
  a credentialed session turn, restart recovery) is unrecorded.
