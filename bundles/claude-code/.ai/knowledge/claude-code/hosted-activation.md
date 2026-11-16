<!-- ryeos:signed:2026-11-16T12:47:54Z:9a15ae3802a6d1f53f7ce94f600916ee71a71dade3cd2d4301f42b0f279e4aec:nm6aQs1CfkrVX/vRuVwstOcMI/5xtUIqpdhMWncRjfJsvmSSAtOf3DFuU63k5R53YFAyNlr/AWskFuiDhFaWBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: claude-code
tags: [claude-code, hosted-worker, activation, runbook]
version: "0.1.0"
description: >-
  Operator runbook for the claude-code bundle. This slice is a stub: the
  adapter answers the structured-session wire with scripted turns and runs no
  model and no Claude Code.
---

# claude-code hosted activation

**This slice is a stub.** `claude-code-adapter` speaks the structured-session
wire and produces scripted turns from the goal payload. It starts no Claude
Code process, opens no network connection and calls no model. Do not point a
real task at it: every turn "succeeds" with the text the goal's `stub` object
asks for, or with a default sentence.

## What is installed

- `worker:claude-code/hosted` and `worker:claude-code/trusted-hosted`. Both
  capture the bundle's own signed `bin:claude-code-adapter` into the execution
  runtime namespace at launch. The trusted worker uses the trusted
  process-group protocol and needs a node that admits it
  (`trusted_process_group_sessions`).
- `worker_execution:claude-code/bounded-turn` (read-write),
  `bounded-turn-read-only`, and `bounded-turn-recovery`. None requires durable
  storage or a credential private home, so a disposable node can run them.
- `config:claude-code/environments/default` and `trusted-lane`, the worker
  environments the executions bind. Each delivers `ANTHROPIC_API_KEY` and
  `ANTHROPIC_BASE_URL` to the worker as delivered secrets.

## Operator steps

1. Provide the two host values to the node. Each name must be an admitted
   provider-environment name whose recipient capability the session owner
   holds exactly; a wildcard grant does not satisfy it.
2. Create the credential profile. It activates from the two delivered names
   and needs no login ceremony:

       ryeos claude-code profile create <profile-id>

3. Launch a bounded turn with the `environment` binding set to
   `config:claude-code/environments/default` (or `trusted-lane` on a trusted
   node) and the profile id as `credential_profile_id`.

## Driving the stub

The turn goal's `turn_start_payload` carries the task and, in this slice, an
optional `stub` object:

    {"prompt": "...", "stub": {"duration_ms": 0, "outcome": "completed",
     "text": "hello", "progress_events": 3, "structured_output": null}}

`outcome` is `completed`, `crash` (turn ends with status `crashed`) or
`interrupt` (turn ends with status `interrupted`). A turn that cannot report
its own end would hang the session, so each outcome still emits
`turn/completed`. Without `stub` the turn completes at once.

## The live driver

Without a `stub` object the adapter runs Claude Code, one `claude -p` process
per turn, in its own process group. It needs:

- `CLAUDE_CODE_EXECUTABLE`, an absolute path to Claude Code's executable. The
  environment templates carry the placeholder `/path/to/claude-code/bin/claude`;
  set it to the real location (later, the `realization_path` of a pinned file
  realization). The digest is not pinned yet: the executable's redistribution
  terms and storage are separate decisions.
- `CLAUDE_CONFIG_DIR`, the credential home the bridge provides. Claude Code's
  sessions are kept under `projects/` there, and the adapter's own files under
  `adapter/` (its session map and its MCP configuration).
- `CLAUDE_CODE_MCP_CONFIG`, the signed MCP server list as
  `{"mcpServers": {...}}`.

`session/start` may carry `model`, `effort` and `tools` (a plain-name
allow-list; `mcp__server__*` patterns allowed). The session keeps them for
every turn, so route `fixed_params` plus `forbidden_fields` are what stop a
goal widening them. A turn may add `deadline_seconds`, `max_budget_usd`,
`output_schema` and `system_prompt`. Claude Code is started with only its own
settings (`--setting-sources user`), only the adapter's MCP file
(`--strict-mcp-config`), `--permission-mode dontAsk` with prompts denied, and
`--bare` when a gateway base URL is delivered.

An adapter built for shipping uses `--no-default-features`: that binary has no
stub and refuses the `stub` knob. The default build keeps the stub for tests.

## Not in this slice

Claude Code's executable storage and pinning, and the profile `fixed_params`
that fix model, effort and tools per item, arrive in later slices. Until then
`configuration_absence` already refuses a workspace that carries Claude Code
project settings, hooks or MCP definitions, and the profile is schema 12.
