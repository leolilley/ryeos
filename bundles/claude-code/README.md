<!-- ryeos:signed:2026-11-17T22:20:51Z:8ae27b7ef7b1683c8d6dea58371dfe4c0a56998f53c7882da95365efb292238c:J06z/hMQ+Ts0aWXXlAkowq1UVvLp9vk2i69mGkAneZp6KuvnG2v9tsc6oY9QI/n2k3D+f2lCSp9ac4f3PZkoAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# RyeOS Claude Code

Provider data for hosting Claude Code through RyeOS's generic
structured-session bridge. The bridge is Core-owned; this bundle holds the
adapter binary's contract: the signed workers, the schema-12 profile and its
route and notification schemas, the bounded-turn worker executions, the worker
environments and a credential-profile command.

**Status: stub slice.** The adapter (`crates/tools/claude-code-adapter`) speaks
the bridge's stdio wire but runs no model and no Claude Code. Its behaviour
comes only from the goal payload's `stub` object, so the bridge mapping,
completion, recovery and result path can be exercised before the real driver
exists. The real driver replaces the stub behind the same wire.

## Layout

| Path | Content |
|---|---|
| `.ai/workers/claude-code/hosted.yaml`, `trusted-hosted.yaml` | `@subprocess` workers over the bridge; the executable is this bundle's own `bin:claude-code-adapter`, captured at admission and mounted in the execution runtime |
| `.ai/workers/claude-code/lib/hosted/structured-session.profile.json` | schema-12 `stdio_jsonrpc` profile |
| `.ai/workers/claude-code/lib/hosted/schema/` | one JSON schema per route request, response and notification |
| `.ai/workers/claude-code/lib/hosted/settings.baseline.json` | the baseline written to `$CLAUDE_CONFIG_DIR/settings.json` |
| `.ai/worker-executions/claude-code/` | `bounded-turn` (read-write), `bounded-turn-read-only`, `bounded-turn-recovery`, `bounded-turn-continue` (resumes a completed predecessor named by `continue_from`) |
| `.ai/config/claude-code/environments/` | `ryeos.worker_environment.v7` for each worker and auth path: a model key and gateway address, or a Claude subscription token, delivered as secrets |
| `.ai/node/commands/claude-code-profile-create.yaml` | `ryeos claude-code profile create <id>` |
| `.ai/node/commands/claude-code-subscription-profile-create.yaml` | `ryeos claude-code profile create-subscription <id>` |
| `.ai/knowledge/claude-code/hosted-activation.md` | operator runbook |
| `test_contract.py` | authored-contract checks |

## Wire

Requests are `{id, method, params}` with integer ids, responses are
`{id, result|error}`, notifications are `{method, params}`; one object per
line.

Routes (id, upstream method, route sets):

- `session.start` (`session/start`, `session`) and `session.start_read_only`
  (`session/start_read_only`, `read_only`): bind a new session, fixing the
  access posture the adapter enforces.
- `turn.start` (`turn/start`), `turn.interrupt` (`turn/interrupt`).
- `session.resume` (`session/resume`) and `session.read` (`session/read`),
  runtime-only, for crash recovery.

Notifications: `turn/started` (idle to turn_running with `turn_id`),
`turn/completed` (turn_running to idle with `completed_turn_id`, a status and a
durable `result` of text, structured output and usage), and `turn/progress`
(validated, not retained).

## Stub goal contract

`turn_start_payload.stub`: `duration_ms`, `outcome` (`completed`, `crash`,
`interrupt`), `text`, `progress_events`, `structured_output`. See the runbook.

## Providers

Provider names appear only in this bundle and the adapter crate. Redistribution
terms for the Claude Code executable must be settled before any archive of it is
published or pinned.
