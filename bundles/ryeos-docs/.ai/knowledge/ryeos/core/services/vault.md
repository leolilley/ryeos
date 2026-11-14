<!-- ryeos:signed:2026-11-14T07:17:08Z:943c6b9b989676be0f107265af1aa827c295f4c9f93229968c2516c63afe554d:grWIU84YwsfVGUgID481hvrr3yDEVh5+p8MksDP0XVycpS6RGNrcv2N4hMjxWaLw35j1BxoxMC/mux4XUxAPDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, vault, secrets, remote]
version: "1.1.0"
description: Vault service reference.
---

# Services: vault

Vault services mutate or read sealed node secrets:

- `vault/set`
- `vault/list`
- `vault/delete`

The vault is a **single node-level store** protected by service capabilities,
daemon vault key material and per-name ceilings. It is not a per-principal
namespace. Remote vault commands proxy to these same routes on the target
node, where the ceilings judge the principal the remote admitted (the
delegated user, or the calling node when no delegation is held).

Per-name ceilings come from the node's signed `sealed_vault_names` policy:

- `vault/set` and `vault/delete` require the caller to be a **writer** of the
  name; otherwise they refuse with 403 `vault_name_not_writable`.
- `vault/list` returns only the names the caller may receive or write.
- A name with no entry in the policy is **operator-only**: only the configured
  local operator may receive or write it, and any other caller is refused with
  403 `vault_name_has_no_ceiling`. `ryeos node doctor` warns
  (`vault_names_without_ceiling`) for each stored name in this state.
- Delivery into a launch requires the principal to be a **recipient**, refused
  with 403 `vault_name_not_delivered`.

Each refusal names the fix, `ryeos node policy-apply sealed_vault_names
<source.yaml>` on a stopped node. See
[Secrets & credentials](../../standard/secrets.md#per-name-vault-ceilings).

The current sealed backend is one bounded encrypted envelope. Every read,
runtime-vault get/list, and read-modify-write operation opens and validates the
whole plaintext map; mutations then seal the whole map again. The enforced
storage bounds are:

| Boundary | Maximum |
|---|---:|
| entries in the shared envelope | 1,024 |
| physical key | 256 bytes |
| value | 256 KiB |
| serialized/decrypted plaintext | 4 MiB |
| sealed envelope on disk | 6 MiB |

These are storage-admission and read bounds, not merely HTTP or UDS response
limits. Operator secrets and internally represented runtime-vault entries
share the same envelope and therefore the same aggregate limits.

These services are separate from runtime vault bindings: launch preflight
resolves required secrets before spawning a subprocess, then injects only
the declared bindings into the runtime environment.

## Runtime-vault listing

The bundle-scoped runtime-vault API keeps its existing logical refs:

```text
vault://bundle/<bundle-id>/<namespace>/<key>
```

Runtime namespaces and keys are `[A-Za-z0-9_]+` segments of at most 64
characters. `runtime.vault_list` accepts an optional exclusive lexical
`cursor` and a `limit` (default 64, maximum 128), and returns sorted keys plus
`next_cursor`. Its serialized response is capped at 64 KiB.

This pagination bounds callback materialization and wire responses; it does
not provide narrow storage I/O. The sealed backend still decrypts and
validates the complete bounded envelope before selecting a page. A sharded or
first-class scoped backend that can read one bundle/namespace without opening
the global map is deferred advanced work.

## Runtime secret bindings

Tools expose their secret needs through `required_secrets` metadata. The
dispatcher reads exactly those names before launch and refuses execution
if any declared secret is missing. Sources are checked in this order:

1. sealed node vault;
2. daemon host environment;
3. user/project `.env` overlay.

Only declared names are injected into the subprocess environment. The
daemon never pours the whole vault into a tool process.

Delivery from the operator vault is operator authority. An execution policy
that exposes it (a project overlay with `include_operator_vault: true`, or the
`operator` vault namespace) requires the fixed capability
`ryeos.use.vault.operator` at preauthorization; without it the daemon refuses
with `operator_vault_capability_required` before any launch or secret read.
The configured operator's bootstrap `*` grant covers it. Remote caller nodes
and narrower principals need it granted explicitly, and it should not be
granted to remote caller nodes by default. See
[Secrets & credentials](../../standard/secrets.md#who-may-read-the-operator-vault).

```yaml
category: agent-kiwi/oauth/connect
executor_id: "@subprocess"
required_secrets:
  - GOOGLE_CLIENT_ID
  - GOOGLE_CLIENT_SECRET
  - AGENT_KIWI_OAUTH_STATE_SECRET
```

Use vault services or CLI/remote-vault commands to provision encrypted
operator secrets. Hosted deployments may instead provide declared names
as service environment variables; local development may use project or
user `.env` files.

Non-secret runtime config such as public base URLs, allowed redirect
domains, or provider regions should be passed through ordinary tool
configuration, parameters, or project config. Reserve `required_secrets`
for values that must be treated as secrets.

Bundle-owned durable state is separate from the vault boundary. Bundle
events and projections may record that a credential exists, which
principal/account owns it, when it expires, and which vault or sealed
secret reference currently holds it. They should not store raw OAuth
refresh tokens, provider signing secrets, API keys, or client secrets in
plaintext event payloads. If a bundle needs portable secret-bearing state,
store an opaque vault reference or an envelope-encrypted blob whose
plaintext key is kept outside the event chain.

Security notes:

- `vault/set` receives the secret value in the HTTP request body.
- Request signing authenticates and protects integrity, but it is not
  transport encryption.
- Use TLS for non-loopback deployments.
