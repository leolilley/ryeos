<!-- ryeos:signed:2026-05-22T04:30:07Z:185e11b0d37d9b516fecfca520ec56d100daa3b6bd76cd4d11c50e5b88a1bc3a:zgmLaKnRBj3jbwc9reEB67dO8Al23b6C1m4lXlSz2CTyL/iz88p3ljRdjQ9swOmKSRLg5TDKOPYLhArzlGzNBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
