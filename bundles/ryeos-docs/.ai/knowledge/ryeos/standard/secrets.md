<!-- ryeos:signed:2026-11-13T00:58:36Z:33e04f958442b395f280faca940b1e195acf5f37f0fba1f216e7b3dd2bfb8a5b:PLxtsQCNXdy/tGe8IW38sfD6PZOoTN3UGk84TIFjt5sE8hUYoOPmT/o4ARwczHtIeSkfrcExL42ZADezKpUFAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
item. Values are never printed, only presence and source.

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
