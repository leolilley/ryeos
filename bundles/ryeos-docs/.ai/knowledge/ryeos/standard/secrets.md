<!-- ryeos:signed:2026-11-14T07:17:08Z:b1a202a28d62a5a52623a6f32b77b577e7208b20277c4290b4e9c0933eee6080:tu1kl76fOVZ9HCqXv2Fh1QqojM7jKiESgitENQTSe91bFH2lXoMwBcYzFsPLltxFMmv1ySaWxl5pHrd7mWP6DA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

1. **Sealed vault** — `ryeos vault put <NAME> …`. One store per node, not
   scoped per principal. Which principals may receive, replace or delete each
   name is decided by the node's signed `sealed_vault_names` policy (see
   [Per-name vault ceilings](#per-name-vault-ceilings)); a name the policy does
   not list is operator-only.
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

## Per-name vault ceilings

The vault is one shared store, so holding the `ryeos.execute.service.vault/*`
capabilities does not by itself let a principal touch every name. The node's
signed `sealed_vault_names` policy section lists, per name, two ceilings:

```yaml
schema: 1
names:
  - name: ZEN_API_KEY
    recipients:            # who may have the value delivered into a launch
      principals: [fp:<operator>, fp:<teammate>]
      capabilities: []
    writers:               # who may set or delete it
      principals: [fp:<operator>]
      capabilities: [ryeos.write.vault.zen_api_key]
```

- Each list holds principals and exact capabilities; both are sorted and
  unique, at least one entry is required, and a capability must be exact (no
  wildcard). A principal passes when it is listed or holds a listed capability
  exactly; a wildcard grant that merely covers the capability does not pass.
- Nothing is implied: an operator that should keep writing a listed name lists
  itself.
- **A name with no entry is operator-only.** Only the node's configured local
  operator may receive or write it. Every other principal is refused with
  `vault_name_has_no_ceiling`. A node whose vault predates the ceilings
  therefore behaves as before for the operator and fails closed for everyone
  else; `ryeos node doctor` lists such names in the
  `vault_names_without_ceiling` row (warning) with the fix.
- `vault/set` and `vault/delete` judge the caller against `writers` and refuse
  with 403 `vault_name_not_writable`. `vault/list` shows a caller only the
  names it may receive or write.
- Delivery judges every vault name a launch declares against `recipients`,
  under the principal and the capability ceiling the launch was admitted
  with, and refuses with 403 `vault_name_not_delivered`. This covers a
  project overlay that includes the operator vault and the `operator` vault
  namespace. A projectless launch carries no capability ceiling, so only
  listed principals (and the operator) receive. Principals without a grant of
  their own, such as `session:<id>` and `route-handler:<bundle>:<id>`, are
  judged by that exact string and must be listed by name; a route that
  declares `operator_vault: true` needs its `route-handler:` principal listed
  as a recipient for each name its handler declares.
- Every refusal names the fix: apply a `sealed_vault_names` section with the
  node stopped, `ryeos node policy-apply sealed_vault_names <source.yaml>`.
  The change takes effect at the next start. The section is carried across a
  policy-generation cut like any operator-applied section.
- On a remote node the writer check sees the principal the remote admitted:
  the delegated user when the caller holds a signed delegation, otherwise the
  calling node. A remote vault name must therefore list that principal as a
  writer on the remote.
- Provider-delivered environment names keep their own ceiling in
  `provider_environment`; bundle-scoped runtime vault entries stay bound to
  their bundle and are not named here.

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
