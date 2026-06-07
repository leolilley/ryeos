<!-- ryeos:signed:2026-06-07T04:05:13Z:e1d5c5a1bb5cf3b5c4f9c2d6d927ccc728c1a03f4ca4dd3c66694fca29ddfcb4:Wk9yFGwqfvwU3Tc5eBLBKjNI4MOVoBTwZpaBFVOP7tGmlHUWDeuG7WfvdLy93ZmbUQ01AsVGlUGKyl3zejwhAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Security notes:

- `vault/set` receives the secret value in the HTTP request body.
- Request signing authenticates and protects integrity, but it is not
  transport encryption.
- Use TLS for non-loopback deployments.
