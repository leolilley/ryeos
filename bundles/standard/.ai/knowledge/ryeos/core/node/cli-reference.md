<!-- ryeos:signed:2026-07-16T02:18:48Z:bd0bf0f21adbc1ec03cb23b9c7f5eade51f8ae52e28b80e1b4193bfc29aaf136:0hNO31M3y3p+xzbHSDsViS3NghfaVbxNOL7apt+1VQGmWGmqrybr/+slFlvtqmt16nlyA35JEMz2l++d+4tRCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [reference, cli, verbs, aliases, lifecycle]
version: "3.2.0"
description: >
  Complete reference for the ryeos CLI: local lifecycle verbs, local
  operator verbs, daemon-backed verbs, aliases, and arguments.
---

# CLI Reference

The `ryeos` CLI has two execution paths:

1. Local verbs that run without daemon dispatch: `init`, `start`, `stop`,
   `node status`, `trust pin`, `authorize-key`, `publish`, and local vault
   maintenance verbs.
2. Daemon-backed verbs declared by signed bundle YAML and dispatched over
   HTTP to a running daemon.

Daemon-backed dispatch is preflighted with local lifecycle status unless
`RYEOSD_URL` is set. Lifecycle verbs ignore `RYEOSD_URL`.

## Minimal lifecycle surface

### `ryeos init`

```bash
ryeos init [--source <dir>] [--app-root <dir>] [--trust-file <file>...]
```

Packaged installs use `/usr/share/ryeos` by default, so plain
`ryeos init` is sufficient. Development usage:

```bash
ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

### `ryeos start`

```bash
ryeos start [--app-root <dir>]
```

Starts the local daemon. Fails if not initialized, succeeds immediately
if already running, and uses the lifecycle start flock. Default readiness
timeout is 15 seconds.

### `ryeos stop`

```bash
ryeos stop [--force] [--app-root <dir>]
```

Connects to a configured live UDS, captures the kernel-authenticated peer with
`SO_PEERCRED` and `SO_PEERPIDFD`, verifies the peer names `ryeosd`, and sends
`SIGTERM` through that pidfd. The default graceful wait is 10 seconds.
`--force` takes a fresh socket peer pidfd before escalating to `SIGKILL` and
waiting two more seconds. Neither mode signals a PID from `daemon.json` or an
RPC response.

### `ryeos node status`

```bash
ryeos node status [--json] [--app-root <dir>]
```

Read-only lifecycle status. Treats `daemon.json` as a hint and trusts
only a `lifecycle.status` response reporting `status: "running"`.

### `ryeos node doctor`

```bash
ryeos node doctor [--json] [--no-bundles] [--app-root <dir>]
```

Runs the offline node checklist. Its sandbox row uses the production strict loader:
disabled is a healthy inactive opt-out, while enforced mode validates the
backend and resource limit. Policy edits require a daemon restart; see
[Execution Isolation](execution-isolation.md).

## Other local operator verbs

- `ryeos trust pin --from <PUBLISHER_TRUST.toml>` — pin publisher trust.
- `ryeos authorize-key --public-key <ed25519:...> --label <label> --scopes <scope,...>` — authorize a caller locally.
- `ryeos remote-descriptor --name <name> --url <url> [--output <path>]` — export this node's remote descriptor trust pin.
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
