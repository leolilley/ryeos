# RyeOS OpenCode (design in progress)

First-class signed integration for hosting a pinned opencode server on a
RyeOS node, mirroring the Codex bundle: the generic structured-session
bridge plus one signed profile. Nothing here is admitted or qualified yet.

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

The bridge previously hardcoded stdio line-delimited JSON-RPC to the workload.
Profiles now require an explicit top-level `transport` (schema_version
3 -> 4, clean cut, no default; the Codex profiles were regenerated and
re-signed in the same change):

- `"stdio_jsonrpc"` — current behavior.
- `"http_sse"` — bridge spawns the workload, parses the listening line for
  the bound port, waits for `GET /global/health`, then speaks HTTP with
  per-boot basic auth. Requests map route rules to `http_method` +
  `http_path` templates (`{session_id}` placeholder, bound-session routes
  only); server-request replies map to `reply_http_path` (`{request_id}`
  placeholder). The SSE reader wraps each event as
  `{"method": <type>, "params": <properties>}` so notification rules and
  `/message/params/` observation pointers stay unchanged.

Current state: the admission compiler validates the complete v4 vocabulary
including the `http_sse` credential block, and the bridge implements the
transport: loopback listener discovery from the workload's stdout listening
line, per-boot basic-auth credentials supplied through profile-named
environments, request dispatch on the route's `http_method`/`http_path` with
bounded response bodies, server-sent events normalized into the stdio
notification/server-request envelopes, and approval replies POSTed to the
admitted `reply_http_path`. An integration test drives a fixture HTTP
server through binding, event streaming, and session-path substitution.

Loopback TCP inside the worker process scope is a new admission surface the
Codex stdio profile never needed; node policy must admit it explicitly for
this worker family rather than globally.

## Route mapping (initial)

| Route id | HTTP | Effect class |
|---|---|---|
| `credential.auth.set` | `PUT /auth/{provider}` | credential_write |
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

## Open items

- Bundle population/signing (`populate-bundles`), `test_contract.py` mirror,
  and init-profile registration remain; this tree is authored source only.
- The generic bridge is core-owned (`bin:core/ryeos-structured-session-bridge`)
  and shared with the Codex worker; this bundle declares the worker-kind
  dependency on core through its manifest requires list.
- Worker-node loopback admission: the opencode worker runs a loopback TCP
  listener inside its process scope, which the stdio Codex profile never
  needed; node isolation policy must admit it for this worker family.
- The environment config's `portable_state_contract: null` and omitted
  subject projection need install-time validation against the signed
  worker-environment schema.
- Live qualification against an activated node (credential enrollment,
  a credentialed session turn, restart recovery) is unrecorded.
