<!-- ryeos:signed:2026-07-14T10:12:30Z:11510f27ac0cebcbd8976f02dadb1b84c55e85f822ee6981aa4f8bbcd9b8e108:UrwNrRsi8k/Wa0pWbd1OKfUXyIUjiVEaXGBEZXUkiOkckCllYu629KExFTuP6vjXp//qo6P1N+kPQUJVAZsgBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

In v1, the vault is a **single node-level store** protected by service
capabilities and daemon vault key material. It is not a per-principal
namespace. Remote vault commands proxy to these same routes on the target
node, so granting `ryeos.execute.service.vault.*` to a remote caller is
operator-level access to the target node vault.

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
