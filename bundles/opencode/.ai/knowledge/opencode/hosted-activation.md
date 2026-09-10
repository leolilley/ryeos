---
category: opencode
tags: [opencode, hosted-worker, activation, runbook]
version: "0.1.0"
description: >-
  Activation and release-acceptance runbook for the pinned opencode server
  hosted behind the RyeOS structured-session bridge. Seed; qualification
  evidence is not yet recorded.
---

# opencode hosted activation

This bundle hosts OpenCode `1.18.30` (linux-x64 glibc standalone archive)
behind the signed `ryeos-structured-session-bridge` over the schema 4
`http_sse` transport. The profile is `worker:opencode/hosted`; acquisitions
are `config:opencode/activation`. The activation ceremony follows
`knowledge:ryeos/core/execution/worker-hosted-execution` and the operator
flow of the Codex bundle README; only the opencode-specific facts live here.

## Runtime contract

- The bridge spawns `opencode serve --pure --hostname 127.0.0.1 --port 0`,
  discovers the loopback listener from the workload's stdout listening
  line, and authenticates with per-boot basic credentials supplied through
  `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`.
- All four XDG roots are redirected into the private credential home; HOME
  alone does not isolate opencode state. The compatibility seed is published
  through `OPENCODE_CONFIG` at `<home>/opencode.json`; the per-boot
  authority is the daemon-admitted `OPENCODE_CONFIG_CONTENT` inline config,
  which overrides any project `opencode.json`: `autoupdate` and `share`
  off, empty MCP map, `default_agent` pinned, and `webfetch` set to ask.
- Turn routes pin `agent: build`; `--pure` does not suppress
  project-authored `.opencode/` agents, which are admitted workspace
  prompt content rather than plugins.
- Permission asks arrive as `permission.v2.asked` and are deny-only:
  decisions map to `reject`, and `once`/`always` are refused before any
  upstream contact.

## Credential and session posture

- Enrollment is non-interactive: `worker_execution:opencode/login` drives
  `PUT /auth/{provider}` with an operator-supplied API key through the
  `credential.auth.set` route. There is no protocol-level device flow.
- Account facts are not projected: `credential_subject` is null because
  API-key providers expose no account identity through the server API.
- Session state lives in the credential-bearing unified `opencode.db`;
  `portable_state` is null. Node-local resume through the persisted private
  home is the v1 recovery posture (`session.resume` / `session.read`); the
  idle signal is absence from `GET /session/status`. Cross-site
  continuation of the upstream conversation is a non-goal — project
  candidates still hand off through the frozen-checkpoint machinery.
