<!-- ryeos:signed:2026-09-10T10:38:49Z:4f07929a2e2fce51bbbfda29d0902680d37cae017807f59142510b299c4c5089:Y76H3jnkotHKwEusZFnC2lbF7A1f2J9o6zPgemX5KGAUWY3kYdF8e1iCYBv/jbNWJPUGlLQir5bIK4zGxcI1Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: opencode
tags: [opencode, hosted-worker, activation, runbook]
version: "0.1.0"
description: >-
  Activation and release-acceptance runbook for the pinned opencode server
hosted behind the RyeOS structured-session bridge. The executable and protocol
closure are admitted; credentialed provider contact is deliberately not yet
qualified.
---

# opencode hosted activation

This bundle hosts OpenCode `1.18.30` (linux-x64 glibc standalone archive)
behind the signed `ryeos-structured-session-bridge` over the schema 6
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
- Network posture: the worker kind's network ceiling omission result is
  `node_policy`, so the loopback transport runs under the node's host
  networking. An `isolated` ceiling leaves the namespace loopback down and
  refuses the worker at launch. Bash command egress follows the node
  ceiling; only `webfetch` is forced through the deny-only ask path.

## Credential and session posture

- There is intentionally no `worker_execution:opencode/login` and no public
  raw-key route. A hosted command is durable testimony, so an API key, OAuth
  token or refresh token must not enter its payload even when its response is
  ephemeral. OpenCode remains non-contact-qualified until RyeOS's existing
  vault/credential-profile owner exposes a provider-neutral late-secret
  projection to structured sessions. That future projection must carry only a
  secret slot/reference in durable testimony and construct the vendor request
  only at the final in-memory contact boundary.
- Account facts are not projected: `credential_subject` is null because
  API-key providers expose no account identity through the server API.
- Session state lives in the credential-bearing unified `opencode.db`;
  `portable_state` is null. Node-local resume through the persisted private
  home is the v1 recovery posture (`session.resume` / `session.read`); the
  idle signal is absence from `GET /session/status`. Cross-site
  continuation of the upstream conversation is a non-goal — project
  candidates still hand off through the frozen-checkpoint machinery.
