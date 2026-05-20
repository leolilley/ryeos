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

These services are separate from runtime vault bindings: launch preflight
resolves required secrets before spawning a subprocess, then injects only
the declared bindings into the runtime environment.

Security notes:

- `vault/set` receives the secret value in the HTTP request body.
- Request signing authenticates and protects integrity, but it is not
  transport encryption.
- Use TLS for non-loopback deployments.
