<!-- ryeos:signed:2026-09-10T07:15:52Z:c2265b320ab69cd12753a92f2ed134b232111917910da71261a45edcdd0021ed:tv5fvTiTatE8cuChBbCt8ulU8a9l1we+Dje7djpeNL11qUsgh3hqtr2gNNntpyzHbgvG+2qHL0YVNlO7Zk4yDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [reference, cli, verbs, aliases, lifecycle]
version: "3.5.3"
description: >
  Complete reference for the ryeos CLI: local lifecycle verbs, local
  operator verbs, daemon-backed verbs, aliases, and arguments.
---

# CLI Reference

The `ryeos` CLI has two execution paths:

1. Local verbs that run without daemon dispatch: `init`, `start`, `stop`,
   `node status`, `node doctor`, the `node reset ...` family, `trust pin`,
   `authorize-key`, `publish`, and local vault maintenance verbs.
2. Daemon-backed verbs declared by signed bundle YAML and dispatched over
   HTTP to a running daemon.

Daemon-backed dispatch is preflighted with local lifecycle status unless
`RYEOSD_URL` is set. Lifecycle verbs ignore `RYEOSD_URL`.

Command metadata and bundle verification use registered definition admission:
the same signed policy, adapter checks, trust and bundle-generation fencing,
but no supervised process-scope acquisition. Direct executable dispatch
re-admits execution under the retained generation guard; it does not fall back
to definition admission if execution is refused. An `auto` command whose
resolved service declares `availability: both` uses the live daemon when that
node is running. Otherwise, its standalone service must prove stopped-node
ownership and independently admit the service. Standalone services do not
acquire the supervised worker controller's scope authority.

## Minimal lifecycle surface

Signed command project bindings distinguish selection from service parameters:
`no_project_flag` permits the CLI selector, `bind_parameter` projects a selected
path, and `bind_no_project_parameter` explicitly projects a projectless selection
as `true` into the named service argument. It does not emit an implicit `false`.
`request_project_path` separately supplies execution-envelope context. Both live
and offline binding use the same rules. Remote execute/status/list declare the
boolean projection; doctor, fetch, verification, binding and UI commands do not
acquire an undeclared service argument merely because they accept `--no-project`.

For direct `execute <item-ref>`, argv `--project` / `--no-project` select the
execution context separately from the item's structured parameters. JSON
`project`, `no_project`, and `project_path` fields supplied through `--input`
or the single-JSON-argument form remain item data; they neither select local
project authority nor disappear during CLI binding. A remote operation may
therefore carry its own project parameter while the outer call is projectless.
Signed command aliases still use their declared selector-to-service mappings.

### `ryeos init`

```bash
ryeos init [--node-profile <name>] [--source <dir>] [--app-root <dir>] [--trust-file <file>]...
```

Packaged installs use `/usr/share/ryeos` by default. The full package selects
its exact complete seed explicitly:

```bash
ryeos init --node-profile full
```

Fresh init refuses an absent selector; bundle presence never implies one.
Development usage:

```bash
ryeos init --source bundles --node-profile full --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

### `ryeos start`

```bash
ryeos start [--app-root <dir>] [--bind <addr>] [--uds-path <path>]
```

Starts the local daemon. Fails if not initialized, succeeds immediately
if already running, and uses the lifecycle start flock. A node with no host
association starts directly. A node explicitly configured with host supervision
requests the installed service through Lillux; missing or unhealthy supervision
is an error and never falls back to direct spawning. The readiness timeout is
15 minutes so verified projection recovery can finish. Interactive terminals
show the daemon's typed startup phases and counters in one redrawn boot line;
redirected output remains plain and deterministic.

Endpoint arguments are durable stopped-node configuration, not transient
process overrides. A differing value is published under the same lifecycle and
state locks as the launch request; a running or starting node must be stopped
before its endpoints can change. Direct and supervised launches then consume
the same persisted configuration.

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

For a configured supervised node, stop first publishes native down intent and
then terminates the exact pinned daemon. The daemon disappearing is not worker
scope-settlement evidence; normal restart recovery retains any remaining worker
cleanup obligations.

If the node's bootstrap configuration is malformed, stop cannot safely invent
a direct-daemon endpoint. It may still publish Down through an exact protected
host-service association; a direct node must have its bootstrap configuration
repaired before authenticated shutdown can proceed.

### `ryeos node host setup`

```bash
ryeos node host setup --confirm [--app-root <dir>] [--bind <addr>] [--uds-path <path>]
```

One-time administrator-maintenance setup for an existing initialized node that
will host dedicated workers requiring an OS delegation. It records an exact
account, app-root identity, node identity and installed daemon image in
administrator-owned host configuration, then leaves the new service down.
Optional endpoint arguments replace the stopped node's ordinary persisted
bootstrap configuration before the privileged host association is created.
Lillux chooses and provisions the supported native service-manager and scope
delegation; these are not node-policy or project settings. Afterwards the
ordinary `ryeos start`, `ryeos stop` and `ryeos node status` commands operate
the configured service without granting any host authority to workers.
The installed daemon starts with an empty environment and resolves the node's
persisted bootstrap configuration only after entering the selected account.

Setup intentionally leaves the native service inert across host boot. Running
the node remains an explicit `ryeos start` decision, rather than an implicit
host reboot side effect.

Nodes without this explicit association remain supported direct nodes. If an
association exists but its native service is absent, unhealthy or mismatched,
lifecycle commands fail closed rather than start an un-supervised replacement.

### `ryeos node status`

```bash
ryeos node status [--json] [--app-root <dir>]
```

Read-only lifecycle status. Treats `daemon.json` as a hint and trusts
only a `lifecycle.status` response reporting `status: "running"`. If complete
bootstrap configuration cannot be decoded, status may report retained terminal
startup-failure testimony for the selected app root, but it does not infer a
TCP or UDS endpoint from defaults or stale process metadata.

### `ryeos node doctor`

```bash
ryeos node doctor [--json] [--no-bundles] [--app-root <dir>]
```

Runs the offline node checklist. Its isolation row uses the production strict
loader: disabled is a healthy inactive opt-out, while enforced mode resolves
and inspects the selected signed backend bundle and validates resource limits.
Policy edits require a daemon restart; see
[Execution Isolation](execution-isolation.md).

### `ryeos node reset execution-history`

```bash
ryeos node reset execution-history [--dry-run | --confirm] [--include-project-heads --confirm-project-heads] [--json] [--app-root <dir>]
```

Runs the explicit offline execution-history epoch retirement while the daemon
is stopped. It is a destructive schema/authority reset, not storage garbage
collection. Mutation requires `--confirm`; principal and deployed project HEADs
are included only with both project-head flags. Interactive terminals show
typed reset phases and exact retired-head counts; `--json` and redirected calls
emit no terminal control sequences. Restart the daemon and use ordinary
`ryeos maintenance gc` later to reclaim newly unreachable storage. See
[Maintenance GC](../services/maintenance-gc.md).

The other clean-cut reset scopes share the same namespace and require the
daemon to be stopped:

```bash
ryeos node reset authorization --confirm
ryeos node reset replay-indexes --confirm
ryeos node reset external-content-bindings [--dry-run | --confirm]
ryeos node reset policy-generation --node-profile <name> --confirm [--source <dir>] [--trust-file <file>]...
```

Each reset names one authority/schema epoch. None is a storage-reclamation
shortcut, and none broadens into another reset scope implicitly.

`policy-generation` is the clean-cut path for a node whose complete signed
policy generation predates the current registered section set. It verifies a
publisher-signed init profile from the selected source root, treats that
profile's `exact_bundles` as the prospective complete bundle inventory, and
runs with the built-in trust roots plus every explicitly repeated
`--trust-file`; it then
runs the corresponding installs/removals and complete node-signed policy cut
inside the same locked init. The preceding completion record is durably
invalidated before the first mutation; a new signed fence is written only
after the operation completes, and startup refuses an absent or contradictory
fence. The cut preserves node identity, vault credentials, projects, and
execution history while deliberately retiring predecessor policy
customization.

## Other local operator verbs

- `ryeos trust pin --from <PUBLISHER_TRUST.toml>` — pin publisher trust.
- `ryeos authorize-key --public-key <ed25519:...> --label <label> --scopes <scope,...>` — authorize a caller locally.
- `ryeos remote-descriptor --name <name> --url <url> [--output <path>]` — export this node's remote descriptor trust pin.
- `ryeos admission-token --label <label> --scopes <scope,...> [--ttl-secs <seconds>]` — mint a one-time local admission token file for remote bootstrap.
- `ryeos publish <bundle-dir> --key <private-key.pem> --owner <label>` — sign/publish bundle contents.

## Core daemon-backed verbs

- `ryeos execute <ref> [params...]` — execute an item by canonical ref.
- `ryeos content pin <ref> --id <declaration>` — atomically complete a signed
  external-content pin through the daemon-owned, local-operator authoring path.
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
