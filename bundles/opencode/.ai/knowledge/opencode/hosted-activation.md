<!-- ryeos:signed:2026-11-16T08:18:29Z:6041a886caacb2d22f2e17da0a484e6045520dfbc0a40e7e23f4e9511b1e2b90:gZbjrisMmrBahs3HNHlN/J1h1X6AcKZWOGmcF9m1mH0VEWC7RSQanGBtdnurNKM5F2K8+aUE6goC7ao+NQBGCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
  ephemeral. OpenCode remains non-contact-qualified. The substrate now has the generic
  mechanism: a worker environment may name a `delivered_secret`, read at
  session start under the root's sealed environment authority and installed
  only in the workload's environment, with only the name retained in durable
  testimony. OpenCode's own environment and credential profile are not yet
  wired to it; that needs a slot-backed credential profile, which is not
  built. Until then no OpenCode turn is credentialed.
- Account facts are not projected: `credential_subject` is null because
  API-key providers expose no account identity through the server API.
- Session state lives in the credential-bearing unified `opencode.db`;
  `portable_state` is null. Node-local resume through the persisted private
  home is the v1 recovery posture (`session.resume` / `session.read`); the
  idle signal is absence from `GET /session/status`. Cross-site
  continuation of the upstream conversation is a non-goal — project
  candidates still hand off through the frozen-checkpoint machinery.
