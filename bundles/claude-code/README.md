<!-- ryeos:signed:2026-11-24T07:17:57Z:f5d04cbe81cb9dd89bff8e1a70b4b188b7353084357644f942c7573825a4ce34:+UZR3YRUS27N+AG1XCdo/ikzCQKwU1uP+iQh9gGrnJnhHjNfzzS9jNKivuIOcFuI/u26ieSvuijoKn6BKHkxDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# RyeOS Claude Code

Provider data for hosting Claude Code through RyeOS's generic
structured-session bridge. The bridge is Core-owned; this bundle holds the
adapter binary's contract: the signed workers, the schema-13 profile and its
route and notification schemas, the bounded-turn worker executions, the worker
environments and a credential-profile command.

**Status: real worker driver.** The adapter (`crates/tools/claude-code-adapter`)
speaks the bridge's stdio wire and drives the Claude Code CLI: one
`claude -p --output-format stream-json` process per turn, under strict
configuration, resuming the upstream session for later turns. Claude Code's own
executable is not pinned yet: the environments carry a placeholder path (see the
runbook).

## Layout

| Path | Content |
|---|---|
| `.ai/workers/claude-code/hosted.yaml`, `trusted-hosted.yaml` | `@subprocess` workers over the bridge; the executable is this bundle's own `bin:claude-code-adapter`, captured at admission and mounted in the execution runtime |
| `.ai/workers/claude-code/lib/hosted/structured-session.profile.json` | schema-13 `stdio_jsonrpc` profile |
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

## Test-only stub driver

A build with the adapter's `stub-driver` feature (the test bundle refresh only)
answers turns from `turn_start_payload.stub` instead of running Claude Code:
`duration_ms`, `outcome` (`completed`, `crash`, `interrupt`), `text`,
`progress_events`, `structured_output`. A default build refuses the `stub`
field. See the runbook.

## Providers

Provider names appear only in this bundle and the adapter crate. Redistribution
terms for the Claude Code executable must be settled before any archive of it is
published or pinned.
