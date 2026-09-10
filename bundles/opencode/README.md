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
- OpenAPI 3.1 JSON is served from `GET /doc` with `Accept: application/json`
  — this is the vendorable schema source, analogous to
  `bundles/codex/schema/app-server-0.147.0/`.
- `GET /event` is an SSE stream. Each frame is one `data:` line:
  `{"id":"evt_...","type":"server.connected","properties":{}}`; the first
  event is `server.connected`. The 1.18.30 catalog has 94 event types.
- Sessions are scoped to the server process cwd (`POST /session` records the
  `directory`); the bridge already pins cwd to the CoW workspace.
- `GET /session/status` returned `{}` while idle sessions existed — absence
  from the status map must be treated as idle, presence as busy.
- State layout under the XDG data dir: `auth.json`, `mcp-auth.json`,
  `opencode.db*`, `storage/**`, `log/**`, `snapshot/**`, `tool-output/**`,
  `repos/**`, `bin/`. A probe server loaded the operator's real
  `~/.config/opencode/*`; XDG redirection into the private home is mandatory.

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

## portable_state selectors (data dir, initial classes)

- `auth.json`, `mcp-auth.json` -> node_private_credential_state
- `log/**`, `snapshot/**`, `tool-output/**`, `repos/**`, `bin/**` ->
  rebuildable_cache
- `opencode.db*` -> forbidden_or_unknown (until proven otherwise)
- `storage/**` -> forbidden_or_unknown except the bound session's portable
  rows (exact layout pinned against the qualified version)

## Open items

- Exact storage layout for a single session (needs a credentialed run
  against the pinned version) before portable_session_state selectors can
  be finalized.
- Whether `--pure` suppresses project `.opencode/` plugin loading (assumed;
  verify) and whether project config can still widen permissions above
  `OPENCODE_CONFIG_CONTENT` (docs say no; verify against source).
- Acquisition recipes: pin the 1.18.30 standalone release artifacts and
  member digests.
- `EventPermissionV2Asked` payload shape (extract from vendored OpenAPI).
- Worker-executions (login/session/bounded-turn), commands, knowledge
  runbook, `test_contract.py` mirror, init-profile registration — all follow
  the Codex bundle shape after the substrate transport lands.
