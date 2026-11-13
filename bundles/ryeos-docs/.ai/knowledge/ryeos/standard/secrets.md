<!-- ryeos:signed:2026-11-13T06:48:28Z:017d3c0e53125a6a0aabf3f947a3b35c51d25cf955b159f146f75552bca60f3a:rDfGjV5CHBcezcs8y70+5d9a3osGGUEjL0OtaqzQVQUS6+PxbVFUAqsPPbbYhI4IZtTLJIpg86/oP7o3XWSTDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [secrets, vault, env, dotenv, providers, security]
version: "1.0.0"
description: >
  How RyeOS resolves credentials for an execution: provider API keys and
  tool/item required_secrets are read only from the source the launch's
  admitted environment authority names (the node's shared sealed vault, a
  pinned project .env overlay, or policy-declared provider environment).
  What belongs in .env vs operator/client config, and why.
---

# Secrets & credentials

Invariant: **provider API keys and tool/item `required_secrets` resolve through
the same stack.** There is no separate "provider key" plane. Both are resolved
daemon-side before launch and injected into the runtime subprocess environment.

## Resolution order

For each declared secret (a tool/item `required_secrets` entry, or a provider's
`auth.env_var`), the daemon reads only the source the launch's admitted
environment authority names:

1. **Sealed vault** — `ryeos vault put <NAME> …`. One store per node, shared
   by every principal that holds the `ryeos.execute.service.vault/*`
   capabilities: it is not scoped per principal, so a secret one of them sets
   is listed, replaced and delivered for the others.
2. **Project `.env` overlay** — read only from the pinned, identity-checked
   project root the authority names, and only for declared names the vault
   (when the authority includes it) did not already supply.
3. **Provider environment** — names the signed node policy declares, each
   with its own recipient ceiling, taken from the daemon's environment once at
   startup. Delivered only to a principal that ceiling admits.

The daemon host environment and unpinned `.env` files are otherwise not
secret sources.

- Every delivering launch: `authorize_secret_delivery` (`vault.rs`).
- HMAC re-derivation, which delivers to no process:
  `read_required_secrets_with_authority` (`vault.rs`) — same authority.

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
- **Handler routes** never hold it implicitly. A route's fixed
  `route-handler:<bundle>:<id>` principal gets it only when the signed route
  declares `source_config.authority.operator_vault: true` **and** the bundle's
  signed manifest allows it with `runtime_authority.operator_vault: true`.
  An undeclared route runs with the project overlay alone, and is refused
  with `operator_vault_capability_required` if its handler declares
  `required_secrets`. See [Routes](../core/node/routes.md#operator-vault).
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
