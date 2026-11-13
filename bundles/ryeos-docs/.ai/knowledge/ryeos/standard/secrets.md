<!-- ryeos:signed:2026-11-13T01:39:38Z:0ddb6af3c642b2ce573d6d4bc6f430f2c1ece5fec4888ecea830e18711d78bbe:1gHvDmtAQ3cqHDjLsxRMDAaJVnCvh9CQJhh1pYOZVXhSCQmjp9bFuwGt+G8TS6zSxyYALYV2Axm/gOobQerPAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [secrets, vault, env, dotenv, providers, security]
version: "1.0.0"
description: >
  How RyeOS resolves credentials for an execution: provider API keys and
  tool/item required_secrets share one stack (vault, daemon host env, .env
  overlay). What belongs in .env vs operator/client config, and why.
---

# Secrets & credentials

Invariant: **provider API keys and tool/item `required_secrets` resolve through
the same stack.** There is no separate "provider key" plane. Both are resolved
daemon-side before launch and injected into the runtime subprocess environment.

## Resolution order

For each declared secret (a tool/item `required_secrets` entry, or a provider's
`auth.env_var`), the daemon resolves in precedence order:

1. **Sealed vault** — `ryeos vault put <NAME> …` (per operator principal).
2. **Daemon host environment** — the env the daemon process was started with
   (e.g. Railway/Fly/Render service variables). Read only for declared names.
3. **`.env` overlay** — conventional `.env` files, operator config dir first,
   then the project root (project overrides operator on collision).

Higher sources win, and a lower source is only consulted for keys the higher
ones did not already satisfy: if the vault and host env supply every declared
secret, no `.env` file is read at all.

- Tool/item `required_secrets`: `read_required_secrets` (`vault.rs`).
- Provider `auth.env_var`: `preflight_inject_provider_secret` →
  `read_explicit_secret` (`launch.rs`, `vault.rs`) — same stack.

## Who may read the operator vault

The sealed vault is the node operator's. Holding execute rights on an item
does not let a caller have the operator's secrets injected into it. Every
execution policy whose environment exposes the operator vault requires the
fixed capability **`ryeos.use.vault.operator`**:

- `environment: {kind: project_overlay, include_operator_vault: true}`;
- `environment: {kind: vault, namespace: operator}`.

The daemon checks it when it preauthorizes the policy, before any project
capture, launch reservation or secret read. A caller without it is refused
with HTTP 403 and the typed error code `operator_vault_capability_required`,
which names the capability. A policy with `include_operator_vault: false`
(the CLI's exclude-operator-vault control) still reads the project `.env`
overlay and needs no extra capability.

Who holds it:

- **The configured local operator** holds it through its bootstrap `*`
  grant. Its own runs are unaffected.
- **Remote caller nodes and every narrower principal** need it granted
  explicitly on the node whose vault would be read. Do not grant it to
  remote caller nodes by default: it hands that node every secret of
  yours its items declare. Prefer a project `.env` on the target, or the
  node's provider environment, and keep `include_operator_vault: false`.
- **Schedules** fire with their declared `capabilities`, so a schedule whose
  policy exposes the operator vault must list the capability.
- **Route handlers** run the bundle source the operator installed, under a
  fixed daemon principal; that principal carries the capability.
- **Cross-site worker handoff** re-checks it on the target: the owner's
  current target grant must hold it before the target delivers from its own
  vault.
- `remote execute` and `remote run` send `include_operator_vault: false` by
  default. Asking a remote to expose its operator vault needs the capability
  both locally (the source checks its caller) and in the target's grant.

## The `.env` overlay only reads what was declared

The overlay is scoped to the secrets a tool/provider actually declares. Any
other line in a `.env` — unrelated keys, blocked control names, even malformed
lines — is **ignored, never fatal**. A project `.env` may legitimately mix tool
secrets with app/client config; an unrelated line cannot fail a launch.

The only failure from the overlay is a *declared* secret that appears malformed
(e.g. `API_KEY` with no `=value`), which surfaces as a targeted error rather
than silently falling back to a stale value.

## Blocked names are never secrets

These names control the executor/daemon and can never be loaded as secrets
(declaring one as a `required_secret` is rejected at validation):

- exact: `PATH`, `HOME`, `PWD`, `USER`, `SHELL`, `TERM`, `PYTHON*`,
  `RYEOS_APP_ROOT`, proxy vars (`HTTP_PROXY`/`HTTPS_PROXY`/…), SSL vars.
- prefixes: `LD_`, `DYLD_`, `RYEOS_`, `RYEOSD_`.

## Where to put what

| Class | Examples | Put it in |
|---|---|---|
| Tool/provider secrets | `ZEN_API_KEY`, `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `SUPABASE_*`, `OXYLABS_*` | vault (preferred), daemon/service env, or a project/operator `.env` |
| RyeOS client/control config | `RYEOSD_URL`, `RYE_CLIENT_KEY_PEM` | operator shell / client app config — **not** the tool-secret path |

`RYEOSD_URL` and friends are not secrets; they configure how a *client* reaches
a daemon. Keeping them out of the tool-secret `.env` avoids confusion (and they
are ignored by the overlay regardless, since no tool declares them).

## Debugging resolution

Use `ryeos tool env-check <ref>` to see, per declared `required_secret`, which
source would satisfy it (vault / host env / which `.env`) — without running the
item. Values are never printed, only presence and source. For a caller without
`ryeos.use.vault.operator` it reports the project overlay alone and does not
consult the operator vault.

v1 reports an item's declared `required_secrets`. A directive's provider
`auth.env_var` is resolved separately at launch and is not yet enumerated by
env-check; the response carries `provider_auth_checked: false` so this is
explicit.

## Common mistakes

- Assuming provider keys must be in the vault, or must be in the daemon's
  process env only. They resolve from any of the three sources, same as tool
  secrets.
- Sourcing a whole project `.env` into your shell before `ryeos` CLI commands:
  that injects client/control vars (`RYEOSD_URL`) into the CLI and can break it.
  Pass tool inputs via `--input`; let the daemon resolve secrets.
