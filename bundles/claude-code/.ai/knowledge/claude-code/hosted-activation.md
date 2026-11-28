<!-- ryeos:signed:2026-11-27T16:11:42Z:9ade7b624da31e5c65cef857e74bf88eaaf9556295c2ef64a6ad18adca4aa97b:Cvc3U3r0uzYf5G9ueWuUzKqJukdlaSYbK41PBIgUCexAfDdHeGP837UD39y2fRh1hwQDBLvpJTpwS3lRC6UnCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: claude-code
tags: [claude-code, hosted-worker, activation, runbook]
version: "0.1.0"
description: >-
  Operator runbook for the claude-code bundle: installing and launching the
  Claude Code worker driver, which runs one Claude Code process per turn.
---

# claude-code hosted activation

`claude-code-adapter` is a worker driver: it speaks the structured-session
wire and runs Claude Code, one `claude -p` process per turn (see "The live
driver"). Claude Code's own executable is pinned by digest: activate it on the
node (see "The live driver") before launching.

## What is installed

- `worker:claude-code/hosted` and `worker:claude-code/trusted-hosted`. Both
  capture the bundle's own signed `bin:claude-code-adapter` into the execution
  runtime namespace at launch. The trusted worker uses the trusted
  process-group protocol and needs a node that admits it
  (`trusted_process_group_sessions`).
- `worker_execution:claude-code/bounded-turn` (read-write),
  `bounded-turn-read-only`, and `bounded-turn-recovery`. None requires durable
  storage or a credential private home, so a disposable node can run them.
- The worker environments the executions bind, two per auth path:
  - `config:claude-code/environments/default` and `trusted-lane` deliver a
    model key: `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL`.
  - `subscription` and `subscription-trusted-lane` deliver a Claude
    subscription token: `CLAUDE_CODE_OAUTH_TOKEN`.

  The adapter refuses a session unless exactly one of the two is present, and
  a subscription token never goes through a gateway.

## Operator steps (model key)

1. Provide the two host values to the node. Each name must be an admitted
   provider-environment name whose recipient capability the session owner
   holds exactly; a wildcard grant does not satisfy it.
2. Create the credential profile. It activates from the two delivered names
   and needs no login ceremony:

       ryeos credential-profile create <profile-id> \
         --activation '{"kind":"delivered_secret","names":["ANTHROPIC_API_KEY","ANTHROPIC_BASE_URL"]}'

3. Launch a bounded turn with the `environment` binding set to
   `config:claude-code/environments/default` (or `trusted-lane` on a trusted
   node) and the profile id as `credential_profile_id`.

## Operator steps (Claude subscription)

1. On a machine with a browser, run `claude setup-token` once and sign in
   with the subscription. It prints a long-lived token.
2. Provide that token to the node as the host value `CLAUDE_CODE_OAUTH_TOKEN`,
   under the same exact recipient rule as the model key.
3. Create the credential profile from it:

       ryeos credential-profile create <profile-id> \
         --activation '{"kind":"delivered_secret","names":["CLAUDE_CODE_OAUTH_TOKEN"]}'

4. Launch with the `environment` binding set to
   `config:claude-code/environments/subscription` (or
   `subscription-trusted-lane`). Turns draw on the subscription's own usage
   limits.

## The test-only stub driver

Test builds only (see the end of the next section): the turn goal's
`turn_start_payload` carries the task and an optional `stub` object that
replaces the Claude Code run with a scripted turn:

    {"prompt": "...", "stub": {"duration_ms": 0, "outcome": "completed",
     "text": "hello", "progress_events": 3, "structured_output": null}}

`outcome` is `completed`, `crash` (turn ends with status `crashed`) or
`interrupt` (turn ends with status `interrupted`). A turn that cannot report
its own end would hang the session, so each outcome still emits
`turn/completed`. In a stub build a turn without `stub` completes at once.

## The live driver

The adapter runs Claude Code, one `claude -p` process per turn, in its own
process group. It needs:

- `CLAUDE_CODE_EXECUTABLE`, the path of Claude Code's executable, a file
  inside the pinned `claude-code` realization each environment declares. The
  environment carries no path: the node resolves it at placement. Activate the
  pinned executable once per node and environment, before the first session:
  `ryeos external-content activate config:claude-code/activation/<environment> online`
  (`default`, `trusted-lane`, `subscription` or `subscription-trusted-lane`).
  The recipe downloads the vendor's linux-x64 archive for version 2.1.332 from
  the npm registry, refuses redirects, and verifies the archive's sha256 and
  the executable's own sha256 (`a2df1f41...1d026`, the digest Anthropic's
  release manifest also publishes) before importing it; the bytes are not
  redistributed with this bundle. Moving to another version is a new signed
  recipe and environment digest, never an edit of the pin in place.
- `CLAUDE_CONFIG_DIR`, the credential home the bridge provides. Claude Code's
  sessions are kept under `projects/` there, and the adapter's own files under
  `adapter/` (its session map and its MCP configuration).
- `CLAUDE_CODE_MCP_CONFIG`, the signed MCP server list as
  `{"mcpServers": {...}}`.

`session/start` may carry `model`, `effort` and `tools` (a plain-name
allow-list; `mcp__server__*` patterns allowed). The session keeps them for
every turn, so the signed worker execution item pins them: its
`route_fixed_params` map (route id to fields) extends that route's
`fixed_params` at launch, and the route then refuses any goal payload that
names a pinned field. `bounded-turn` and `bounded-turn-recovery` pin
`effort: high` and `tools: [Read, Grep, Glob, Edit, Write, Bash]`;
`bounded-turn-read-only` pins `effort: high` and `tools: [Read, Grep, Glob]`
(a read-only session refuses write-capable tools in any case). `model` is left
unpinned, so a goal may choose it. A continuation resumes the upstream
session and starts none, so `bounded-turn-continue` pins nothing. A turn may add `deadline_seconds`, `max_budget_usd`,
`output_schema` and `system_prompt`. Claude Code is started with only its own
settings (`--setting-sources user`), only the adapter's MCP file
(`--strict-mcp-config`), `--permission-mode dontAsk` with prompts denied, and
`--bare` when a gateway base URL is delivered.

A default build has no stub and refuses the `stub` knob; that is what ships.
Only the test bundle refresh (`populate-bundles.sh --build-profile test`, which
`gate.sh --refresh-bundles` selects) builds the adapter with its `stub-driver`
feature.

## Configuration absence

`configuration_absence` already refuses a workspace that carries Claude Code
project settings, hooks or MCP definitions. The profile is schema 13.
