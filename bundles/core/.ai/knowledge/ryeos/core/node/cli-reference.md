<!-- ryeos:signed:2026-05-31T04:22:26Z:750e10cc477ab2e7357cee1080a51790582c38d5b548d9ec6d3d4415c59b3e0f:iMWqqNMOGdeW8LgTILiFhJW4VLbSODefp+C8HJ1G3pmqiG/riB9DcrdEyXaPZlwp8Sno/sm2xQh8tlBNzPhpAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/core/node
tags: [reference, cli, verbs, aliases, lifecycle]
version: "3.0.0"
description: >
  Complete reference for the ryeos CLI: local lifecycle verbs, local
  operator verbs, daemon-backed verbs, aliases, and arguments.
---

# CLI Reference

The `ryeos` CLI has two execution paths:

1. Local verbs that run without daemon dispatch: `init`, `start`, `stop`,
   `status`, `trust pin`, `authorize-key`, `publish`, and local vault
   maintenance verbs.
2. Daemon-backed verbs declared by signed bundle YAML and dispatched over
   HTTP to a running daemon.

Daemon-backed dispatch is preflighted with local lifecycle status unless
`RYEOSD_URL` is set. Lifecycle verbs ignore `RYEOSD_URL`.

## Minimal lifecycle surface

### `ryeos init`

```bash
ryeos init [--source <dir>] [--system-space-dir <dir>] [--user-root <dir>]
           [--trust-file <file>...]
```

Packaged installs use `/usr/share/ryeos` by default, so plain
`ryeos init` is sufficient. Development usage:

```bash
ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

### `ryeos start`

```bash
ryeos start [--system-space-dir <dir>]
```

Starts the local daemon. Fails if not initialized, succeeds immediately
if already running, and uses the lifecycle start flock. Default readiness
timeout is 15 seconds.

### `ryeos stop`

```bash
ryeos stop [--force] [--system-space-dir <dir>]
```

Gracefully stops the local daemon via the UDS that just proved live.
Default graceful timeout is 10 seconds. `--force` re-confirms live
`status: "running"` and PID before signalling and verifies the PID is
`ryeosd` on Unix.

### `ryeos status`

```bash
ryeos status [--json] [--system-space-dir <dir>]
```

Read-only lifecycle status. Treats `daemon.json` as a hint and trusts
only a `lifecycle.status` response reporting `status: "running"`.

## Other local operator verbs

- `ryeos trust pin --from <PUBLISHER_TRUST.toml>` — pin publisher trust.
- `ryeos authorize-key --public-key <ed25519:...> --label <label> --scopes <scope,...>` — authorize a caller locally.
- `ryeos admission-token --label <label> --scopes <scope,...> [--ttl-secs <seconds>]` — mint a one-time local admission token file for remote bootstrap.
- `ryeos publish <bundle-dir> --key <private-key.pem> --owner <label>` — sign/publish bundle contents.

## Core daemon-backed verbs

- `ryeos execute <ref> [params...]` — execute an item by canonical ref.
- `ryeos fetch <ref> [--with-content] [--verify]` — resolve/read an item. Alias: `f`.
- `ryeos sign <ref> [--source project|user]` — sign an item. Alias: `s`.
- `ryeos verify <ref>` — verify signature, trust, and path anchoring.

## Bundle Management

- `ryeos bundle install <path>` — install bundle offline.
- `ryeos bundle list` — list installed bundles.
- `ryeos bundle remove <name>` — remove installed bundle offline.

## Standard workflow verbs

Standard contributes thread, event, scheduler, command, and compose
verbs such as `thread list`, `thread get`, `events replay`,
`scheduler register`, and `compose`.

## Remote Operations

Remote verbs cover cross-node configure/status, push/pull, execute,
threads, token-based admission, remote authorization, live bundle
install, and vault proxying.
See [Remote Command Reference](../remote/remote-command-reference.md).

## Aliases Quick Reference

| Alias | Verb |
|---|---|
| `f` | `fetch` |
| `s` | `sign` |
