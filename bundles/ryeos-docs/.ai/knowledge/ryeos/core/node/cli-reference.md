<!-- ryeos:signed:2026-11-13T12:24:05Z:12bd64f3b14c9316db67e8eab56baa3a32e666e87179f65cc7d5e8a30682c493:k6Vxg0WG6dAjAynUWW8uyDN+y7eDaGyJF4EsM1/YrVoVlPSOveeIgof6e47tBc7bR6VKV2ELg9LUyr6oV+CTCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/node
tags: [reference, cli, verbs, aliases, lifecycle]
version: "3.6.0"
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
ryeos init [--node-profile <name> --host-class <class>] [--source <dir>] [--app-root <dir>] [--trust-file <file>]...
```

Packaged installs use `/usr/share/ryeos` by default. The full package selects
its exact complete seed explicitly:

```bash
ryeos init --node-profile full --host-class local
```

Fresh init refuses an absent selector; bundle presence never implies one.
`--host-class` is given exactly when `--node-profile` is, and names one of the
profile's declared host classes (`full` declares only `local`).
Development usage:

```bash
ryeos init --source bundles --node-profile full --host-class local --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
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

A stopped node whose native-capacity epoch has no incarnation left is refused
before launch with `native_capacity_exhausted`, naming the epoch; no restart
inhibition is recorded. The cure is the one `ryeos node doctor` names:
`ryeos node capacity reissue --reason "..."`, then `ryeos start` (or, where the
signed policy forbids re-issuance, re-provisioning). A start that uses the
epoch's last incarnation warns that the next start will be refused.

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
Package upgrades likewise require the association's pinned daemon executable
to equal the package-owned daemon path before creating an upgrade journal or
stopping the node. A service intentionally pinned to another prefix must be
upgraded by that prefix's owning installation mechanism; the ordinary package
installer never writes there or silently changes the association.

### `ryeos node host retire`

```bash
ryeos node host retire --confirm [--app-root <dir>]
ryeos node host retire --orphaned --confirm
```

Retires the native service that `ryeos node host setup` provisioned, through
the installed daemon as administrator, like setup. The service is brought down,
deactivated, and its service definition and logs are removed. Every form
requires `--confirm`.

With `--app-root` (or the default node), the node must be stopped: retirement
takes the node's existing lifecycle and state locks and refuses while either is
held. It creates and writes nothing in the app root, and the node remains a
usable direct node. A service whose recorded launch is not a RyeOS node
service for exactly that app root is refused.

With `--orphaned`, the CLI first lists every RyeOS node service whose recorded
app root no longer exists, then the administrator step re-checks each root and
retires only those still missing. A service whose app root exists is never
touched by the sweep. `ryeos node doctor` reports such services in its
`host_services` row.

Retirement needs root and a live service manager, so it has no automated
end-to-end test. A manual check on a runit host: set up a throwaway node with
`ryeos node host setup --confirm --app-root <dir>`, confirm
`/etc/runit/runsvdir/default/ryeos-<digest>` exists, run
`ryeos node host retire --confirm --app-root <dir>`, and confirm the link, the
`/etc/runit/sv/ryeos-<digest>` directory and
`/var/log/lillux-host-service/ryeos-<digest>` are gone.

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
[Execution Isolation](execution-isolation.md). Its `process_quarantine` row
warns about threads whose attached process group recovery cannot prove gone,
from the report the daemon writes after each recovery pass. Its
`stale_publication` row lists `publish_ready` hosted candidates whose base HEAD
has moved (session, candidate, base and current HEAD); each settles to
`base_superseded` on its owner's next publish.

The `external_bindings` row compares every pin of every installed
`external_execution`, `runtime_snapshot_production` and
`runtime_snapshot_qualification` binding with the installed signed bundles and
the node vault, read only: no adapter runs and the vault is opened without
rewrap recovery. It fails for each pin the next fresh operation through that
binding would refuse, and warns when it cannot decide one. Each finding names
the binding (section, id, digest), the field, the pinned value, what is
installed in that role, and the fix:

| Code | Meaning |
|---|---|
| `binding_adapter_not_installed` | a pinned lifecycle adapter, supervisor, launcher, provider configuration adapter or connector is not installed |
| `binding_restoration_verifier_not_installed` | the qualification's restoration verifier is not the one the producer's adapter ships |
| `binding_provider_spec_differs` | the qualification's provider spec is not the one the producer's adapter declares |
| `binding_snapshot_spec_differs` | the producer's snapshot production spec is not the one its adapter declares |
| `binding_producer_digest_stale` | the qualification names a production binding that is absent or was recomposed |
| `binding_consumer_verifier_not_installed` | a selected consumer verifier is not shipped by any installed bundle |
| `binding_credential_absent` | the vault holds no credential for the binding's generation |
| `binding_credential_unverified` (warn) | the vault could not be read without writing it, for example a pending rewrap |

Pins the offline row cannot decide are listed under `not_checked`: a consumer
selection's `scenario_source_digest` needs the running node's derive (a launch
with a stale one is refused), and snapshots recorded in the runtime database
are not read. A binding file in a schema the node no longer admits fails the
`node_config` row with code `binding_schema_unsupported`, naming the file, the
schema and protocol seen and those admitted. At run time, an operation through
a production binding whose pinned adapter generation is not installed is
refused with HTTP 409 `binding_artifact_not_installed`, naming the same pin.

The `external_capacity` and `credential_profiles` rows ask the running daemon
(`service:external-execution/status`, a 10 s deadline) what still holds
external capacity or a credential-profile lock. With the node stopped both are
`n/a` with `state: not_checked`; a daemon that does not answer makes them warn
`not_checked` with the error. `external_capacity` covers unsettled
allocations and open Q local starts, `credential_profiles` every held profile
lock with the session, Q start or continuation reservation that holds it.
Every listed row carries an `attention` and a `fix`:

| Attention | Meaning | Row status |
|---|---|---|
| `in_use` | a live owner of this daemon generation holds it and releases it when its work ends | ok |
| `settling` | the daemon's own recovery owns it and retries it (a row a previous daemon generation left) | warning |
| `operator` | nothing on the node releases it without the operator: an unknown provider outcome, a Q start whose cleanup is unproved, a session whose outcome is unknown after a restart, a lock no owner names | fail |

`ryeos external-execution status` prints the same view as JSON.

The `remote_content_testimonies` row reads this node's accepted remote
qualification testimonies from its signed heads, offline and without
resolving bundles. For each content-lane acceptance it checks whether this
node holds an activation of exactly the content the origin qualified (same
activation program digest, consumer and declaration). It is advisory: it
warns but never fails the doctor, and is `n/a` when nothing is accepted.

| Code | Meaning |
|---|---|
| `ok` | this node activated the qualified release; launch can use the testimony |
| `no_local_activation` (warn) | this node has not activated it; the fix names `ryeos external-content activate` |
| `local_activation_differs` (warn) | this node activated the same activation ref as another program; both digests are listed, and the fix is the origin's release or a re-qualification against this node's |
| `acceptance_superseded` (warn) | an acceptance of either lane was recorded under an earlier acceptance evidence schema; export the testimony again from the origin |

See [Retained products](../execution/retained-products.md) for the content
testimony workflow.

The host-wide `host_services` row reads the administrator-owned native service
records without root. It warns when a RyeOS node service records an app root
that no longer exists, and names the fix,
`ryeos node host retire --orphaned --confirm`. It reports `n/a` where no native
service directory is readable without root.

Its `ui_browser_sessions` row checks that the signed browser-session policy is
present and valid (schema 5: session lifetime, idle timeout, launch-token
lifetime and the `session_scopes` ceiling). The live session count needs the
running node, so the row reports it as not available offline.

### `ryeos node reset execution-history`

```bash
ryeos node reset execution-history [--dry-run | --confirm] [--include-project-heads --confirm-project-heads] [--schema-cut-from <epoch> --schema-cut-to <epoch>] [--json] [--app-root <dir>]
```

Runs the explicit offline execution-history epoch retirement while the daemon
is stopped. It is a destructive schema/authority reset, not storage garbage
collection. Mutation requires `--confirm`; principal and deployed project HEADs
are included only with both project-head flags. Interactive terminals show
typed reset phases and exact retired-head counts; `--json` and redirected calls
emit no terminal control sequences. Restart the daemon and use ordinary
`ryeos maintenance gc` later to reclaim newly unreachable storage. See
[Maintenance GC](../services/maintenance-gc.md).

Persisted container upgrades may bind destructive authorization to one exact
predecessor/current epoch pair with `--schema-cut-from` and `--schema-cut-to`.
The command refuses a different predecessor or a future runtime. Repeating it
after the named cut returns `already_current` without retiring new work. This
allows platform restarts to redrive one authorized cut without turning later
schema changes into implicit authorization. An already-current cut changes
nothing, so it inspects no execution history: its counts are zero and rows it
would never touch cannot refuse it. The scheduler namespace check still runs.

A reset that would retire history refuses with
`execution_history_scopes_unretired` while process-scope obligations remain
that no daemon recovery has retired (a Q session that ended by disconnect
leaves one). The refusal names how many it saw. Start the node once so its
recovery retires them, stop it, then rerun the reset.

The other clean-cut reset scopes share the same namespace and require the
daemon to be stopped:

```bash
ryeos node reset authorization --confirm
ryeos node reset replay-indexes --confirm
ryeos node reset external-content-bindings [--dry-run | --confirm]
ryeos node reset policy-generation --node-profile <name> --host-class <class> --confirm [--source <dir>] [--trust-file <file>]... [--keep-section <section>]... [--reset-section <section>]...
```

Each reset names one authority/schema epoch. None is a storage-reclamation
shortcut, and none broadens into another reset scope implicitly.

Large objects that `ryeos external-content scrub` quarantined for byte damage
stay on disk as evidence, counted against the large-object budget, until an
operator discards them with the daemon stopped:

```bash
ryeos node large-objects purge-quarantine [--dry-run | --confirm]
```

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
execution history.

Sections an operator applied with `ryeos node policy-apply` carry forward.
Every generation records them in a signed `_operator-sections.yaml` member,
and the cut compiles each carried body under the current section schema. If
any no longer validates, the cut refuses before changing anything and names
each one; `--reset-section <section>` takes the profile's body instead. Every
other section comes from the profile. A generation published before the record
existed cannot say which sections were chosen: the cut lists every section
that differs from the profile and needs each named with `--keep-section` or
`--reset-section`. Nothing is reset silently. The pinned request digest
covers the carried result, so a rerun after a crash reproduces it exactly.

The capacity issuance pins the `native_capacity` policy it was granted under.
When the resulting generation's `native_capacity` differs from the policy in
force (the section was reset, or the profile changed one no operator applied),
the cut amends the issuance in the same step, as `policy-apply
native_capacity` does, and its report carries `capacity_amendment`. The
amendment's checks run before the completion record is removed: while an
incarnation debit is outstanding, or the account's image does not fit the new
policy, the cut refuses with nothing changed; settle the account with
`ryeos node capacity reissue --reason "..."` and rerun it. A rerun after a
crash converges on the same amendment.

The same cut moves a node to a renamed profile. A node records the profile it
was initialized under in its substrate identity; a release that renames a
profile ships no profile under the old name, so the node keeps running on its
current generation but cannot be cut again under the old name. The remote
Codex controller profile `f5r-controller` is now `hosted-controller`, built
as the image `ryeos-hosted-controller`. Move a controller with:

```bash
ryeos node reset policy-generation --node-profile hosted-controller --host-class <recorded class> --confirm
```

The host class is the one the node recorded; it does not change. The cut
records `hosted-controller` in the substrate identity. On a hosted container,
sign the same `node.reset.policy-generation` operation against the
`ryeos-hosted-controller` image's seed and substrate digest (see
`deploy/composition-images.md`), move the service to that image, and set
`RYEOS_INIT_NODE_PROFILE=hosted-controller` wherever the deployment overrides
the image's value.

### `ryeos node host-access grant|revoke|list`

```bash
ryeos node host-access grant <item_ref> --project <dir> [--id <id>] \
  [--origin direct|graph_child|schedule]... [--env <NAME>]... \
  [--containment scope_required|group_allowed] [--app-root <dir>] [--json]
ryeos node host-access revoke <id> [--app-root <dir>] [--json]
ryeos node host-access list [--project <dir>] [--app-root <dir>] [--json]
```

The operator's grants of host-direct execution, in the `host_access`
node-policy section. `grant` resolves the item in the project, computes its
signer, source-closure and runtime digests, and installs the edited section
through the operator-signed `node.policy-apply`; re-running it replaces the
grant for that item. `revoke` removes one grant. `list --project` reports each
grant as `current` or `stale` against the item's bytes there. The node must be
stopped; grants take effect at the next start. A granted item runs on the host
with the operator's authority: see `ryeos/core/node/host-access`.

### `ryeos node external-execution compose`

```bash
ryeos node external-execution compose --id <binding-id> --backend <backend> \
  --transport https --origin <https://host[:port]> --roots <roots.pem> \
  --account <label> --settings <file> --program <program.json> \
  --guest-runtime-manifest-hash <hash> \
  [--capacity-group <group>] [--runtime-qualification <file|none>] \
  [--limits <limits.json>] [--app-root <dir>] [--json]
ryeos node external-execution compose --id <binding-id> --backend <backend> \
  --transport file-relay --owner-workspace-id <id> --private-mailbox-parent <dir> \
  --account <label> --settings <file> --program <program.json> \
  --guest-runtime-manifest-hash <hash> [...same optional flags]
```

Composes, signs with the node identity and installs one external placement
binding under `.ai/node/external_execution/<id>.yaml` on a stopped node. It
holds the node state lock, so it refuses while the daemon runs. Artifact
identities (backend, supervisor, launcher, configuration adapter, connector)
and the settings schema digest come from the installed, verified bundles; an
unknown provider or backend, or an ambiguous installed generation, is refused.
The https origin must be a
canonical bare origin (lowercase ASCII host, no default port, no path, query,
fragment or userinfo, non-zero port) and `--roots` must hold only X.509
CERTIFICATE blocks. Every run names a fresh credential generation; provision
the provider secret next and then restart the node.

### `ryeos node external-execution provision-credential`

```bash
ryeos node external-execution provision-credential --binding <binding-id> \
  [--secret-fd <fd>] [--app-root <dir>] [--json]
```

Stores the provider secret for an installed binding in the node vault. The
binding names the owner, generation, backend and account, so the operator
supplies only the secret, from stdin or an inherited sealed memfd
(`--secret-fd`). Provisioning is insert-only: the same secret again is a
no-op and a different secret for an existing generation is refused; a new
secret needs a newly composed binding. Reports never carry the secret or the
physical vault key.

## Other local operator verbs

- `ryeos trust pin --from <PUBLISHER_TRUST.toml>` — pin publisher trust.
- `ryeos authorize-key --public-key <ed25519:...> --label <label> --scopes <scope,...>` — authorize a caller locally.
- `ryeos remote-descriptor --name <name> --url <url> [--output <path>]` — export this node's remote descriptor trust pin.
- `ryeos admission-token --label <label> --scopes <scope,...> [--ttl-secs <seconds>]` — mint a one-time local admission token file for remote bootstrap.
- `ryeos publish <bundle-dir> --key <private-key.pem> --owner <label>` — sign/publish bundle contents.

## Core daemon-backed verbs

- `ryeos external-execution status` — list unsettled external allocations, open
  Q local starts and held credential-profile locks, each with its owner,
  attention and fix. Local configured operator only; the doctor's
  `external_capacity` and `credential_profiles` rows read it.
- `ryeos execute <ref> [params...]` — execute an item by canonical ref.
- `ryeos content pin <ref> --id <declaration>` — atomically complete a signed
  external-content pin through the daemon-owned, local-operator authoring path.
- `ryeos external-content bindings <consumer-ref> <installed_bundle|pinned_project> [project-snapshot-hash]`
  — list a consumer's content bindings and whether each still authorizes it at
  that generation; read only, local operator.
- `ryeos external-content rebind <prior-binding-hash> <project-snapshot-hash>` —
  renew a pinned-project content binding for a new snapshot or re-signed
  consumer; local operator, refused with a typed code when the consumer changed.
- `ryeos fetch <ref> [--with-content] [--verify]` — resolve/read an item. Alias: `f`.
- `ryeos sign <ref> [--source project|user]` — sign an item. Alias: `s`.
- `ryeos verify <ref-or-.ai-path> [<ref-or-.ai-path>...]` — verify one or more items' signatures, trust, and path anchoring.

When the daemon refuses a buffered request with the typed
`process_creation_busy` 503 (see [HTTP API](http-api.md)), the CLI prints
the wait and reissues the same request, freshly signed, after the daemon's
`retry_after_ms`, for up to 30 seconds. The daemon raises that refusal only
before it admits the request, so the retry repeats nothing. Every other
failure, and a streamed run, is reported at once.

## Bundle Management

- `ryeos bundle install <name> <path>` — install bundle offline.
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
