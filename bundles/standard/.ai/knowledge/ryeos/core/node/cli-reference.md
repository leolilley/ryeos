<!-- ryeos:signed:2026-07-22T02:33:53Z:bbbac0c04584ce7e3b40d614f385a20c86278de3a3da8cf7ce717736f1ec11bf:tXeRkquOLVk4oNxZlBuloa3Y+3Qk9xTjGBH3IBq4SJ2tPInFMd3gZ+5Kh9mpyV7KtlYsE117dborPwL1fKr+BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [reference, cli, verbs, aliases, lifecycle]
version: "3.3.0"
description: >
  Complete reference for the ryeos CLI: local lifecycle verbs, local
  operator verbs, daemon-backed verbs, aliases, and arguments.
---

# CLI Reference

The `ryeos` CLI has two execution paths:

1. Local verbs that run without daemon dispatch: `init`, `start`, `stop`,
   `node status`, `node doctor`, `node gc`, `trust pin`, `authorize-key`,
   `publish`, and local vault maintenance verbs.
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
if already running, and uses the lifecycle start flock. The readiness timeout
is 15 minutes so verified projection recovery can finish. Interactive terminals
show the daemon's typed startup phases and counters in one redrawn boot line;
redirected output remains plain and deterministic.

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

Runs the offline node checklist. Its isolation row uses the production strict
loader: disabled is a healthy inactive opt-out, while enforced mode resolves
and inspects the selected signed backend bundle and validates resource limits.
Policy edits require a daemon restart; see
[Execution Isolation](execution-isolation.md).

### `ryeos node gc`

```bash
ryeos node gc --discard-thread-history [--dry-run | --confirm-discard-thread-history] [--sweep-cas] [--json] [--app-root <dir>]
```

Runs the explicit offline all-thread-history retirement while the daemon is
stopped. Mutation requires the confirmation flag. Interactive terminals show
typed maintenance phases and exact retired-head counts; `--json` and redirected
calls emit no terminal control sequences. See [Maintenance GC](../services/maintenance-gc.md).

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
- `ryeos verify <ref-or-.ai-path> [<ref-or-.ai-path>...]` — verify one or more items' signatures, trust, and path anchoring.

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
